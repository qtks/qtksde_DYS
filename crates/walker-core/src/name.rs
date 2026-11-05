use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// Normalize a file name to Unicode NFC before any hashing, name de-duplication
/// or search matching is performed. M0 applies this to every scanned entry.
pub fn normalize_name(name: &str) -> String {
    name.nfc().collect()
}

/// Extension used for distribution buckets: NFC-normalized and lowercased.
pub(crate) fn extension_key(name_nfc: &str) -> String {
    Path::new(name_nfc)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| normalize_name(ext).to_lowercase())
        .filter(|ext| !ext.is_empty())
        .unwrap_or_else(|| "(none)".to_string())
}
