//! Content hashing used for change detection.

/// Hex-encoded blake3 hash of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
