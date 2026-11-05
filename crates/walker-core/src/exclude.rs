use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_EXCLUDES: [&str; 4] = ["/proc", "/sys", "node_modules", ".git/objects"];

/// Matches directory paths against exclusion rules.
///
/// Rules beginning with `/` are absolute path prefixes. Other rules are
/// slash-separated component sequences matched anywhere in a path.
#[derive(Debug, Clone, Default)]
pub struct ExcludeMatcher {
    prefixes: Vec<PathBuf>,
    component_rules: Vec<Vec<OsString>>,
}

impl ExcludeMatcher {
    pub fn new(patterns: &[String]) -> Self {
        let mut prefixes = Vec::new();
        let mut component_rules = Vec::new();
        for raw in patterns {
            let pattern = raw.trim().trim_end_matches('/');
            if pattern.is_empty() {
                continue;
            }
            if pattern.starts_with('/') {
                prefixes.push(PathBuf::from(pattern));
                continue;
            }
            let parts: Vec<OsString> = pattern
                .split('/')
                .filter(|part| !part.is_empty())
                .map(OsString::from)
                .collect();
            if !parts.is_empty() {
                component_rules.push(parts);
            }
        }
        Self {
            prefixes,
            component_rules,
        }
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        if self.prefixes.iter().any(|prefix| path.starts_with(prefix)) {
            return true;
        }
        if self.component_rules.is_empty() {
            return false;
        }
        let components: Vec<&OsStr> = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        self.component_rules.iter().any(|rule| {
            !rule.is_empty()
                && rule.len() <= components.len()
                && components.windows(rule.len()).any(|window| {
                    window
                        .iter()
                        .zip(rule.iter())
                        .all(|(a, b)| *a == b.as_os_str())
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_absolute_prefixes() {
        let matcher = ExcludeMatcher::new(&["/proc".to_string(), "/sys".to_string()]);
        assert!(matcher.is_excluded(Path::new("/proc/1/fd")));
        assert!(matcher.is_excluded(Path::new("/sys/kernel")));
        assert!(!matcher.is_excluded(Path::new("/home/ming/proc")));
        assert!(!matcher.is_excluded(Path::new("/procfs")));
    }

    #[test]
    fn matches_component_sequences_anywhere() {
        let matcher =
            ExcludeMatcher::new(&["node_modules".to_string(), ".git/objects".to_string()]);
        assert!(matcher.is_excluded(Path::new("/a/node_modules/pkg/index.js")));
        assert!(matcher.is_excluded(Path::new("/a/.git/objects/ab/cdef")));
        assert!(!matcher.is_excluded(Path::new("/a/.git/config")));
        assert!(!matcher.is_excluded(Path::new("/a/node_modules_backup")));
    }
}
