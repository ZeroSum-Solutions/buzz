//! The keychain blob format and its bounds.
//!
//! All desktop secrets live as one JSON map under a single keychain entry, so
//! every read deserializes the whole map (`desktop/src-tauri/src/secret_store.rs`).
//! An uncapped record count is therefore an uncapped per-read cost as well as an
//! unbounded keychain entry, which is why the caps below are enforced here — at
//! the one place the bytes become a map — rather than at each caller.

use std::collections::HashMap;

use crate::namespace::MCP_NAMESPACE_PREFIX;

/// Username of the single blob keychain entry, within the service.
pub const BLOB_KEY: &str = "secrets";

/// Largest accepted serialized blob, in bytes.
pub const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// Largest accepted value for one `mcp:` record, in bytes.
pub const MAX_MCP_VALUE_BYTES: usize = 8 * 1024;

/// Largest accepted number of `mcp:` records in the whole store.
pub const MAX_MCP_RECORDS: usize = 256;

/// Why a blob could not be turned into a map.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    /// The serialized blob is over [`MAX_BLOB_BYTES`].
    #[error("keychain blob is {0} bytes, over the {MAX_BLOB_BYTES}-byte cap")]
    TooLarge(usize),
    /// The blob bytes are not UTF-8.
    #[error("keychain blob is not utf-8: {0}")]
    NotUtf8(String),
    /// The blob is not a JSON object of string values.
    #[error("keychain blob is not a json string map: {0}")]
    Malformed(String),
    /// More than [`MAX_MCP_RECORDS`] records carry the `mcp:` prefix.
    #[error("keychain blob holds {0} mcp records, over the {MAX_MCP_RECORDS} cap")]
    TooManyMcpRecords(usize),
    /// One `mcp:` record's value is over [`MAX_MCP_VALUE_BYTES`].
    #[error("mcp secret `{key}` is {len} bytes, over the {MAX_MCP_VALUE_BYTES}-byte cap")]
    McpValueTooLarge {
        /// The offending blob key.
        key: String,
        /// Its value length in bytes.
        len: usize,
    },
}

/// Parse raw blob bytes into the secret map, enforcing every bound.
///
/// The byte cap is checked before any UTF-8 decoding or JSON parsing, so a
/// hostile or corrupt entry cannot make the process allocate its way through a
/// large document first.
///
/// # Errors
/// Returns the [`BlobError`] naming the bound that was breached, or the parse
/// failure. Every error is returned to the caller; none is downgraded to an
/// empty map, because an empty map is indistinguishable from "no secrets" and
/// would silently unauthenticate every server.
pub fn parse_blob(bytes: &[u8]) -> Result<HashMap<String, String>, BlobError> {
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(BlobError::TooLarge(bytes.len()));
    }
    let json = std::str::from_utf8(bytes).map_err(|e| BlobError::NotUtf8(e.to_string()))?;
    let map: HashMap<String, String> =
        serde_json::from_str(json).map_err(|e| BlobError::Malformed(e.to_string()))?;
    check_mcp_bounds(&map)?;
    Ok(map)
}

/// Check the `mcp:`-scoped record-count and value-size bounds on `map`.
///
/// Exposed so the desktop write path can refuse an overflowing mutation with
/// the same error the read path would raise, instead of writing a blob that
/// can never be read back.
///
/// # Errors
/// [`BlobError::TooManyMcpRecords`] or [`BlobError::McpValueTooLarge`].
pub fn check_mcp_bounds(map: &HashMap<String, String>) -> Result<(), BlobError> {
    let mut mcp_records = 0usize;
    for (key, value) in map {
        if !key.starts_with(MCP_NAMESPACE_PREFIX) {
            continue;
        }
        mcp_records += 1;
        if value.len() > MAX_MCP_VALUE_BYTES {
            return Err(BlobError::McpValueTooLarge {
                key: key.clone(),
                len: value.len(),
            });
        }
    }
    if mcp_records > MAX_MCP_RECORDS {
        return Err(BlobError::TooManyMcpRecords(mcp_records));
    }
    Ok(())
}

/// Serialize `map` back to blob bytes, refusing a result over the blob cap.
///
/// # Errors
/// [`BlobError::TooLarge`] when the serialized form is over [`MAX_BLOB_BYTES`],
/// [`BlobError::Malformed`] when serialization itself fails, or the `mcp:`
/// bounds from [`check_mcp_bounds`].
pub fn serialize_blob(map: &HashMap<String, String>) -> Result<String, BlobError> {
    check_mcp_bounds(map)?;
    let json = serde_json::to_string(map).map_err(|e| BlobError::Malformed(e.to_string()))?;
    if json.len() > MAX_BLOB_BYTES {
        return Err(BlobError::TooLarge(json.len()));
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_blob_is_refused_before_parsing() {
        let bytes = vec![b'x'; MAX_BLOB_BYTES + 1];
        assert_eq!(
            parse_blob(&bytes),
            Err(BlobError::TooLarge(MAX_BLOB_BYTES + 1))
        );
    }

    #[test]
    fn record_count_and_value_size_are_capped() {
        let mut map = HashMap::new();
        for index in 0..=MAX_MCP_RECORDS {
            map.insert(format!("mcp:agent-a:1:s{index}"), "v".to_string());
        }
        assert_eq!(
            check_mcp_bounds(&map),
            Err(BlobError::TooManyMcpRecords(MAX_MCP_RECORDS + 1))
        );

        let mut map = HashMap::new();
        map.insert(
            "mcp:agent-a:1:big".to_string(),
            "v".repeat(MAX_MCP_VALUE_BYTES + 1),
        );
        assert_eq!(
            check_mcp_bounds(&map),
            Err(BlobError::McpValueTooLarge {
                key: "mcp:agent-a:1:big".to_string(),
                len: MAX_MCP_VALUE_BYTES + 1,
            })
        );
    }

    #[test]
    fn non_mcp_records_are_not_counted_against_the_mcp_caps() {
        let mut map = HashMap::new();
        for index in 0..(MAX_MCP_RECORDS * 2) {
            map.insert(format!("agent:npub{index}"), "nsec".to_string());
        }
        map.insert("identity".to_string(), "v".repeat(MAX_MCP_VALUE_BYTES + 1));
        assert_eq!(check_mcp_bounds(&map), Ok(()));
    }

    #[test]
    fn round_trips_a_valid_blob() {
        let mut map = HashMap::new();
        map.insert("identity".to_string(), "nsec1".to_string());
        map.insert("mcp:agent-a:1:token".to_string(), "s3cret".to_string());
        let json = serialize_blob(&map).expect("serializes");
        assert_eq!(parse_blob(json.as_bytes()).expect("parses"), map);
    }

    #[test]
    fn malformed_json_surfaces_rather_than_yielding_an_empty_map() {
        let err = parse_blob(b"not json").expect_err("must not silently succeed");
        assert!(matches!(err, BlobError::Malformed(_)), "{err:?}");
        let err = parse_blob(&[0xff, 0xfe]).expect_err("must not silently succeed");
        assert!(matches!(err, BlobError::NotUtf8(_)), "{err:?}");
    }
}
