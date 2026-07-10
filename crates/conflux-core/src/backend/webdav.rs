//! A WebDAV backend (targeting Nextcloud), built on `reqwest::blocking`.
//!
//! Change detection uses the server's ETag as the remote id; `getlastmodified`
//! provides the mtime used for newer-wins conflict resolution.

use super::{Backend, RemoteSnapshot};
use crate::config::{Remote, Sync};
use crate::error::{Error, Result};
use crate::model::RemoteMeta;
use crate::relpath::RelPath;
use percent_encoding::percent_decode_str;
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{Method, StatusCode, Url};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::SystemTime;

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:getetag/>
    <d:getlastmodified/>
    <d:getcontentlength/>
    <d:resourcetype/>
  </d:prop>
</d:propfind>"#;

/// A WebDAV-backed remote rooted at `<remote.url>/<remote_path>/`.
pub struct WebdavBackend {
    client: Client,
    /// The server root (`remote.url`, trailing slash), used to create collections.
    root: Url,
    /// The group base (`root` + `remote_path`, trailing slash).
    base: Url,
    remote_path: String,
    username: Option<String>,
    password: Option<String>,
}

impl WebdavBackend {
    /// Build the backend, resolving credentials (may run `password_command`).
    pub fn new(remote: &Remote, sync: &Sync) -> Result<Self> {
        let root = root_url(&remote.url)?;
        let base = group_base_url(&remote.url, &sync.remote_path)?;
        let client = Client::builder()
            .build()
            .map_err(|e| Error::Backend(format!("failed to build http client: {e}")))?;
        Ok(WebdavBackend {
            client,
            root,
            base,
            remote_path: sync.remote_path.trim_matches('/').to_string(),
            username: remote.username.clone(),
            password: remote.resolve_password()?,
        })
    }

    /// URL for a file relative to the group base, percent-encoding each segment.
    fn url_for(&self, path: &RelPath) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| Error::Backend("base url cannot be a base".into()))?;
            segments.pop_if_empty(); // drop the trailing empty segment from the slash
            for part in path.as_str().split('/') {
                segments.push(part);
            }
        }
        Ok(url)
    }

    /// Apply basic auth if a username is configured.
    fn auth(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.username {
            Some(user) => builder.basic_auth(user, self.password.clone()),
            None => builder,
        }
    }

    fn method(&self, name: &str, url: Url) -> RequestBuilder {
        let method = Method::from_bytes(name.as_bytes()).expect("valid method");
        self.auth(self.client.request(method, url))
    }

    /// Create, top-down from the server root, every collection needed to hold
    /// `path`: the `remote_path` segments plus the file's parent directories.
    /// Creating top-down guarantees each MKCOL's parent already exists.
    fn ensure_parents(&self, path: &RelPath) -> Result<()> {
        let mut segments: Vec<String> = self
            .remote_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let parts: Vec<&str> = path.as_str().split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            segments.push((*part).to_string());
        }
        if segments.is_empty() {
            return Ok(());
        }

        let mut url = self.root.clone();
        for segment in &segments {
            {
                let mut seg = url
                    .path_segments_mut()
                    .map_err(|_| Error::Backend("base url cannot be a base".into()))?;
                seg.pop_if_empty();
                seg.push(segment);
            }
            self.mkcol(&url)?;
        }
        Ok(())
    }

    fn mkcol(&self, url: &Url) -> Result<()> {
        let resp = self.method("MKCOL", url.clone()).send().map_err(net_err)?;
        let status = resp.status();
        // 201 = created; 405/301 = already exists. Anything else is fatal.
        if status.is_success()
            || status == StatusCode::METHOD_NOT_ALLOWED
            || status == StatusCode::MOVED_PERMANENTLY
        {
            Ok(())
        } else {
            Err(Error::Backend(format!(
                "MKCOL {} failed: {status}",
                url.path()
            )))
        }
    }

    fn head_etag(&self, path: &RelPath) -> Result<Option<String>> {
        let resp = self
            .method("HEAD", self.url_for(path)?)
            .send()
            .map_err(net_err)?;
        Ok(resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(parse_etag))
    }
}

impl Backend for WebdavBackend {
    fn snapshot(&self) -> Result<RemoteSnapshot> {
        let resp = self
            .method("PROPFIND", self.base.clone())
            .header("Depth", "infinity")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(PROPFIND_BODY)
            .send()
            .map_err(net_err)?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            // The remote directory does not exist yet — treat as empty.
            return Ok(RemoteSnapshot::new());
        }
        if status.as_u16() != 207 && !status.is_success() {
            return Err(Error::Backend(format!("PROPFIND failed: {status}")));
        }

        let body = resp.text().map_err(net_err)?;
        let mut snapshot = RemoteSnapshot::new();
        for entry in parse_multistatus(&body)? {
            if entry.is_collection {
                continue;
            }
            let Some(rel) = rel_from_href(&self.base, &entry.href) else {
                continue;
            };
            snapshot.insert(
                rel,
                RemoteMeta {
                    id: entry.etag.map(|e| parse_etag(&e)).unwrap_or_default(),
                    mtime: entry.last_modified.and_then(|m| parse_http_date(&m)),
                    size: entry.content_length.unwrap_or(0),
                },
            );
        }
        Ok(snapshot)
    }

    fn read(&self, path: &RelPath) -> Result<Vec<u8>> {
        let resp = self
            .method("GET", self.url_for(path)?)
            .send()
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(Error::Backend(format!(
                "GET {path} failed: {}",
                resp.status()
            )));
        }
        Ok(resp.bytes().map_err(net_err)?.to_vec())
    }

    // `_mtime` is ignored: the server assigns its own last-modified time.
    fn write(&self, path: &RelPath, data: &[u8], _mtime: Option<SystemTime>) -> Result<RemoteMeta> {
        self.ensure_parents(path)?;
        let resp = self
            .method("PUT", self.url_for(path)?)
            .body(data.to_vec())
            .send()
            .map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(Error::Backend(format!(
                "PUT {path} failed: {}",
                resp.status()
            )));
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(parse_etag);
        let id = match etag {
            Some(e) => e,
            None => self.head_etag(path)?.unwrap_or_default(),
        };
        Ok(RemoteMeta {
            id,
            mtime: None,
            size: data.len() as u64,
        })
    }

    fn remove(&self, path: &RelPath) -> Result<()> {
        let resp = self
            .method("DELETE", self.url_for(path)?)
            .send()
            .map_err(net_err)?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Error::Backend(format!("DELETE {path} failed: {status}")))
        }
    }

    fn finalize(&self) -> Result<()> {
        Ok(())
    }

    fn supports_empty_dirs(&self) -> bool {
        true
    }

    fn snapshot_dirs(&self) -> Result<BTreeSet<RelPath>> {
        let resp = self
            .method("PROPFIND", self.base.clone())
            .header("Depth", "infinity")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(PROPFIND_BODY)
            .send()
            .map_err(net_err)?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(BTreeSet::new());
        }
        if status.as_u16() != 207 && !status.is_success() {
            return Err(Error::Backend(format!("PROPFIND failed: {status}")));
        }
        let body = resp.text().map_err(net_err)?;

        // Collect every resource's relative path, and which of them are
        // collections. A collection is empty when no other path sits under it.
        let mut all: Vec<String> = Vec::new();
        let mut collections: Vec<String> = Vec::new();
        for entry in parse_multistatus(&body)? {
            let Some(rel) = rel_from_href(&self.base, &entry.href) else {
                continue; // the group base itself
            };
            let s = rel.as_str().to_string();
            if entry.is_collection {
                collections.push(s.clone());
            }
            all.push(s);
        }

        let mut empty = BTreeSet::new();
        for dir in collections {
            let prefix = format!("{dir}/");
            let has_child = all.iter().any(|p| *p != dir && p.starts_with(&prefix));
            if !has_child {
                if let Some(rel) = RelPath::from_relative(Path::new(&dir)) {
                    empty.insert(rel);
                }
            }
        }
        Ok(empty)
    }

    fn create_dir(&self, path: &RelPath) -> Result<()> {
        // MKCOL top-down: the remote_path segments, then every segment of `path`.
        let mut segments: Vec<String> = self
            .remote_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        segments.extend(path.as_str().split('/').map(str::to_string));

        let mut url = self.root.clone();
        for segment in &segments {
            {
                let mut seg = url
                    .path_segments_mut()
                    .map_err(|_| Error::Backend("base url cannot be a base".into()))?;
                seg.pop_if_empty();
                seg.push(segment);
            }
            self.mkcol(&url)?;
        }
        Ok(())
    }

    fn remove_dir(&self, path: &RelPath) -> Result<()> {
        let resp = self
            .method("DELETE", self.url_for(path)?)
            .send()
            .map_err(net_err)?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Error::Backend(format!("DELETE dir {path} failed: {status}")))
        }
    }
}

fn net_err(e: reqwest::Error) -> Error {
    Error::Backend(format!("webdav request failed: {e}"))
}

/// The server root URL with a guaranteed trailing slash.
fn root_url(remote_url: &str) -> Result<Url> {
    let s = format!("{}/", remote_url.trim_end_matches('/'));
    Url::parse(&s).map_err(|e| Error::Backend(format!("invalid webdav url `{s}`: {e}")))
}

/// Build the group base URL: `<remote.url>/<remote_path>/` (always trailing slash).
fn group_base_url(remote_url: &str, remote_path: &str) -> Result<Url> {
    let base = remote_url.trim_end_matches('/');
    let path = remote_path.trim_matches('/');
    let joined = if path.is_empty() {
        format!("{base}/")
    } else {
        format!("{base}/{path}/")
    };
    Url::parse(&joined).map_err(|e| Error::Backend(format!("invalid webdav url `{joined}`: {e}")))
}

/// Derive a `RelPath` from a multistatus `href` relative to the base URL.
fn rel_from_href(base: &Url, href: &str) -> Option<RelPath> {
    let abs = base.join(href).ok()?;
    let rest = abs.path().strip_prefix(base.path())?;
    let mut parts = Vec::new();
    for seg in rest.split('/') {
        if seg.is_empty() {
            continue;
        }
        parts.push(percent_decode_str(seg).decode_utf8_lossy().into_owned());
    }
    if parts.is_empty() {
        return None;
    }
    RelPath::from_relative(Path::new(&parts.join("/")))
}

/// Strip a weak prefix and surrounding quotes from an ETag.
fn parse_etag(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .to_string()
}

fn parse_http_date(raw: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(raw).ok()
}

#[derive(Default, Debug)]
struct DavEntry {
    href: String,
    is_collection: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    content_length: Option<u64>,
}

/// Parse a WebDAV 207 Multistatus body into per-resource entries.
fn parse_multistatus(xml: &str) -> Result<Vec<DavEntry>> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut entries = Vec::new();
    let mut current: Option<DavEntry> = None;
    let mut tag: Vec<u8> = Vec::new();

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| Error::Backend(format!("invalid PROPFIND xml: {e}")))?;
        match event {
            Event::Start(e) => {
                let local = e.local_name().as_ref().to_vec();
                match local.as_slice() {
                    b"response" => current = Some(DavEntry::default()),
                    b"collection" => {
                        if let Some(c) = current.as_mut() {
                            c.is_collection = true;
                        }
                    }
                    _ => {}
                }
                tag = local;
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == b"collection" {
                    if let Some(c) = current.as_mut() {
                        c.is_collection = true;
                    }
                }
            }
            Event::Text(t) => {
                if let Some(c) = current.as_mut() {
                    let text = t
                        .unescape()
                        .map_err(|e| Error::Backend(format!("invalid xml text: {e}")))?
                        .into_owned();
                    match tag.as_slice() {
                        b"href" => c.href = text,
                        b"getetag" => c.etag = Some(text),
                        b"getlastmodified" => c.last_modified = Some(text),
                        b"getcontentlength" => c.content_length = text.trim().parse().ok(),
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"response" {
                    if let Some(c) = current.take() {
                        entries.push(c);
                    }
                }
                tag.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
    <d:multistatus xmlns:d="DAV:">
      <d:response>
        <d:href>/remote.php/dav/files/me/cfg/</d:href>
        <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
      </d:response>
      <d:response>
        <d:href>/remote.php/dav/files/me/cfg/nvim/init.lua</d:href>
        <d:propstat><d:prop>
          <d:getetag>"abc123"</d:getetag>
          <d:getlastmodified>Tue, 06 Aug 2024 10:00:00 GMT</d:getlastmodified>
          <d:getcontentlength>42</d:getcontentlength>
          <d:resourcetype/>
        </d:prop></d:propstat>
      </d:response>
    </d:multistatus>"#;

    #[test]
    fn parses_multistatus_files_and_collections() {
        let entries = parse_multistatus(SAMPLE).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_collection);
        assert!(!entries[1].is_collection);
        assert_eq!(entries[1].etag.as_deref(), Some("\"abc123\""));
        assert_eq!(entries[1].content_length, Some(42));
        assert!(entries[1].last_modified.is_some());
    }

    #[test]
    fn relativizes_href_against_base() {
        let base = Url::parse("https://h/remote.php/dav/files/me/cfg/").unwrap();
        let rel = rel_from_href(&base, "/remote.php/dav/files/me/cfg/nvim/init.lua").unwrap();
        assert_eq!(rel.as_str(), "nvim/init.lua");
        // The base collection itself yields nothing.
        assert!(rel_from_href(&base, "/remote.php/dav/files/me/cfg/").is_none());
    }

    #[test]
    fn percent_encoded_segments_decode() {
        let base = Url::parse("https://h/dav/").unwrap();
        let rel = rel_from_href(&base, "/dav/with%20space/a.txt").unwrap();
        assert_eq!(rel.as_str(), "with space/a.txt");
    }

    #[test]
    fn etag_parsing_strips_weak_and_quotes() {
        assert_eq!(parse_etag("W/\"xyz\""), "xyz");
        assert_eq!(parse_etag("\"abc\""), "abc");
    }

    #[test]
    fn group_base_url_normalizes_slashes() {
        let u = group_base_url("https://h/dav/", "/cfg/").unwrap();
        assert_eq!(u.as_str(), "https://h/dav/cfg/");
    }
}
