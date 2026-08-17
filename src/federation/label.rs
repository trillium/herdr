//! Origin label prefixes for federated sidebar display (N2 Part 1).
//!
//! Every workspace is labeled with its origin: foreign workspaces carry their
//! origin's tailnet name (truncated to 5 chars) as a `:` prefix, and the local
//! hub's own workspaces carry the local hostname (also truncated to 5 chars).
//! A single helper produces the 5-char prefix so foreign and local rows stay
//! uniform.

use std::sync::OnceLock;

use crate::platform;

/// Number of leading characters kept from an origin display label for the
/// sidebar prefix. Truncation is by characters, not bytes.
const ORIGIN_LABEL_PREFIX_CHARS: usize = 5;

/// The sidebar prefix for an origin: its label truncated to the first
/// [`ORIGIN_LABEL_PREFIX_CHARS`] characters (chars, not bytes). Empty or
/// whitespace-only labels yield an empty prefix, so callers keep their existing
/// empty-label fallback.
pub fn origin_label_prefix(label: &str) -> String {
    label
        .trim()
        .chars()
        .take(ORIGIN_LABEL_PREFIX_CHARS)
        .collect()
}

/// The local hub's own origin prefix: the machine hostname truncated to
/// [`ORIGIN_LABEL_PREFIX_CHARS`] chars. Empty when the hostname is unavailable.
///
/// Cached in a `OnceLock` because the sidebar calls this once per workspace per
/// render; the hostname does not change over a session.
pub fn local_origin_prefix() -> &'static str {
    static LOCAL: OnceLock<String> = OnceLock::new();
    LOCAL.get_or_init(|| origin_label_prefix(&platform::hostname().unwrap_or_default()))
}

/// Compose the sidebar display label for a workspace.
///
/// Foreign rows already carry their origin prefix in `custom_name` (set at
/// ingest), so they are returned unchanged. Local rows get the local hostname
/// prefix prepended here, so every workspace is labeled with its origin without
/// double-prefixing foreign rows. When the local hostname prefix is empty the
/// label is returned unchanged.
pub fn prefixed_workspace_label(is_foreign: bool, label: &str) -> String {
    if is_foreign {
        return label.to_string();
    }
    let prefix = local_origin_prefix();
    if prefix.is_empty() {
        return label.to_string();
    }
    format!("{prefix}:{label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_truncates_to_five_chars() {
        assert_eq!(origin_label_prefix("macbook"), "macbo");
        assert_eq!(origin_label_prefix("mini1"), "mini1");
        assert_eq!(origin_label_prefix("mini2"), "mini2");
        assert_eq!(origin_label_prefix("trilliums-mini"), "trill");
        // Unicode: five characters, not five bytes.
        assert_eq!(origin_label_prefix("münchen"), "münch");
    }

    #[test]
    fn prefix_handles_empty_and_short_labels() {
        assert_eq!(origin_label_prefix(""), "");
        assert_eq!(origin_label_prefix("   "), "");
        assert_eq!(origin_label_prefix("ab"), "ab");
    }

    #[test]
    fn local_prefix_is_stable_and_cached() {
        let first = local_origin_prefix();
        let second = local_origin_prefix();
        assert_eq!(first, second, "cached prefix is stable");
        assert!(
            first.chars().count() <= ORIGIN_LABEL_PREFIX_CHARS,
            "local prefix is at most 5 chars"
        );
    }

    #[test]
    fn prefixed_label_leaves_foreign_rows_unchanged() {
        assert_eq!(prefixed_workspace_label(true, "mini1:FM"), "mini1:FM");
    }

    #[test]
    fn prefixed_label_adds_local_prefix_to_local_rows() {
        let prefix = local_origin_prefix();
        if prefix.is_empty() {
            assert_eq!(prefixed_workspace_label(false, "FM"), "FM");
        } else {
            assert_eq!(
                prefixed_workspace_label(false, "FM"),
                format!("{prefix}:FM")
            );
        }
    }
}
