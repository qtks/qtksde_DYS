use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// Normalize a file name to Unicode NFC before any hashing, name de-duplication
/// or search matching is performed. M0 applies this to every scanned entry.
pub fn normalize_name(name: &str) -> String {
    name.nfc().collect()
}

/// Extension for storage: NFC-normalized, lowercased, `None` when absent.
pub fn extension_of(name_nfc: &str) -> Option<String> {
    Path::new(name_nfc)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| normalize_name(ext).to_lowercase())
        .filter(|ext| !ext.is_empty())
}

/// Extension used for distribution buckets: NFC-normalized and lowercased.
pub(crate) fn extension_key(name_nfc: &str) -> String {
    extension_of(name_nfc).unwrap_or_else(|| "(none)".to_string())
}
