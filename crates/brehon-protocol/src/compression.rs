//! LZ4 compression for protocol messages.
//!
//! Messages larger than the compression threshold are compressed using LZ4
//! to reduce bandwidth usage. A prefix byte indicates whether the message
//! is compressed.
//!
//! # Format
//!
//! ```text
//! [1 byte: compression flag][N bytes: payload]
//! ```
//!
//! - `0x00`: Payload is uncompressed
//! - `0x01`: Payload is LZ4 compressed

use thiserror::Error;

/// Compression prefix byte indicating uncompressed data.
pub const PREFIX_UNCOMPRESSED: u8 = 0x00;

/// Compression prefix byte indicating LZ4 compressed data.
pub const PREFIX_COMPRESSED: u8 = 0x01;

/// Threshold in bytes above which messages are compressed.
/// Messages <= this size are sent uncompressed.
pub const COMPRESSION_THRESHOLD: usize = 256;

/// Errors that can occur during compression/decompression.
#[derive(Debug, Error)]
pub enum CompressionError {
    /// Failed to compress data.
    #[error("compression failed: {0}")]
    Compress(String),

    /// Failed to decompress data.
    #[error("decompression failed: {0}")]
    Decompress(#[from] lz4_flex::block::DecompressError),

    /// Invalid compression prefix byte.
    #[error("invalid compression prefix: 0x{0:02x}")]
    InvalidPrefix(u8),

    /// Data too short (missing prefix byte).
    #[error("data too short: expected at least 1 byte")]
    DataTooShort,
}

/// Compress data if it exceeds the threshold.
///
/// Returns a buffer with a prefix byte followed by the payload.
/// - If `data.len() > COMPRESSION_THRESHOLD`, compresses with LZ4 and uses prefix `0x01`
/// - Otherwise, returns uncompressed with prefix `0x00`
///
/// # Example
///
/// ```rust
/// use brehon_protocol::compression::{compress, decompress};
///
/// let data = b"Hello, world!";
/// let compressed = compress(data);
/// let decompressed = decompress(&compressed).unwrap();
/// assert_eq!(data.as_slice(), decompressed.as_slice());
/// ```
pub fn compress(data: &[u8]) -> Vec<u8> {
    if data.len() > COMPRESSION_THRESHOLD {
        let compressed = lz4_flex::compress_prepend_size(data);
        // Only use compression if it actually reduces size
        if compressed.len() < data.len() {
            let ratio = 100.0 - (compressed.len() as f64 / data.len() as f64 * 100.0);
            tracing::debug!(
                "Compressed {} -> {} bytes ({:.1}% reduction)",
                data.len(),
                compressed.len(),
                ratio
            );
            let mut result = Vec::with_capacity(1 + compressed.len());
            result.push(PREFIX_COMPRESSED);
            result.extend_from_slice(&compressed);
            return result;
        }
    }

    // Uncompressed: prefix + original data
    let mut result = Vec::with_capacity(1 + data.len());
    result.push(PREFIX_UNCOMPRESSED);
    result.extend_from_slice(data);
    result
}

/// Decompress data that was compressed with [`compress`].
///
/// # Errors
///
/// Returns an error if:
/// - The data is empty (missing prefix byte)
/// - The prefix byte is invalid (not 0x00 or 0x01)
/// - LZ4 decompression fails
///
/// # Example
///
/// ```rust
/// use brehon_protocol::compression::{compress, decompress};
///
/// let original = vec![0u8; 1000]; // Large enough to compress
/// let compressed = compress(&original);
/// let decompressed = decompress(&compressed).unwrap();
/// assert_eq!(original, decompressed);
/// ```
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, CompressionError> {
    if data.is_empty() {
        return Err(CompressionError::DataTooShort);
    }

    let prefix = data[0];
    let payload = &data[1..];

    match prefix {
        PREFIX_UNCOMPRESSED => Ok(payload.to_vec()),
        PREFIX_COMPRESSED => {
            lz4_flex::decompress_size_prepended(payload).map_err(CompressionError::from)
        }
        _ => Err(CompressionError::InvalidPrefix(prefix)),
    }
}

#[cfg(test)]
mod tests {
    use crate::compression::*;

    #[test]
    fn test_small_data_not_compressed() {
        let data = b"small";
        let result = compress(data);
        assert_eq!(result[0], PREFIX_UNCOMPRESSED);
        assert_eq!(&result[1..], data.as_slice());
    }

    #[test]
    fn test_large_data_compressed() {
        // Create compressible data (repeated pattern)
        let data: Vec<u8> = (0..1000).map(|i| (i % 10) as u8).collect();
        let result = compress(&data);
        assert_eq!(result[0], PREFIX_COMPRESSED);
        // Compressed data should be smaller
        assert!(result.len() < data.len());
    }

    #[test]
    fn test_roundtrip_small() {
        let data = b"Hello, world!";
        let compressed = compress(data);
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(data.as_slice(), decompressed.as_slice());
    }

    #[test]
    fn test_roundtrip_large() {
        // Large compressible data
        let data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let compressed = compress(&data);
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(data, decompressed);
    }

    #[test]
    fn test_roundtrip_random_data() {
        // Random-ish data that may not compress well
        let data: Vec<u8> = (0..500).map(|i| ((i * 17 + 31) % 256) as u8).collect();
        let compressed = compress(&data);
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(data, decompressed);
    }

    #[test]
    fn test_empty_data() {
        let data: &[u8] = &[];
        let compressed = compress(data);
        assert_eq!(compressed[0], PREFIX_UNCOMPRESSED);
        let decompressed = decompress(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn test_decompress_empty_fails() {
        let result = decompress(&[]);
        assert!(matches!(result, Err(CompressionError::DataTooShort)));
    }

    #[test]
    fn test_decompress_invalid_prefix() {
        let data = [0xFF, 0x01, 0x02, 0x03];
        let result = decompress(&data);
        assert!(matches!(result, Err(CompressionError::InvalidPrefix(0xFF))));
    }

    #[test]
    fn test_threshold_boundary() {
        // Exactly at threshold - should not compress
        let data: Vec<u8> = (0..COMPRESSION_THRESHOLD)
            .map(|i| (i % 256) as u8)
            .collect();
        let result = compress(&data);
        assert_eq!(result[0], PREFIX_UNCOMPRESSED);

        // Just above threshold - should compress (if compressible)
        let data: Vec<u8> = vec![0u8; COMPRESSION_THRESHOLD + 1];
        let result = compress(&data);
        assert_eq!(result[0], PREFIX_COMPRESSED);
    }

    #[test]
    fn test_incompressible_data_stays_uncompressed() {
        // High-entropy data that doesn't compress well
        // Even if above threshold, if compression makes it bigger, keep uncompressed
        let data: Vec<u8> = (0..300).map(|i| ((i * 127 + 53) % 256) as u8).collect();
        let result = compress(&data);
        let decompressed = decompress(&result).unwrap();
        assert_eq!(data, decompressed);
    }
}
