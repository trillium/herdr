//! Id-namespacing for federated origins.
//!
//! `TerminalId` (and workspace/pane public ids) are per-server strings that
//! collide across machines — every server mints `term_1`, `w_1`, etc. Before a
//! foreign id enters the local `AppState`, it is namespaced by the origin's
//! durable [`OriginKey`] so it cannot alias a local id. The `~` delimiter is
//! absent from origin keys (enforced by [`OriginKey`] construction) and from
//! herdr-minted ids (`term_<hex>`, `w_<n>`), so parsing is unambiguous.

use crate::terminal::TerminalId;

use super::origin::OriginKey;

const FED_PREFIX: &str = "fed~";
const SEP: char = '~';

/// Namespace an arbitrary foreign public id with its origin: `fed~<key>~<raw>`.
///
/// The shared shape for every namespaced id. Terminal ids go through
/// [`namespace_terminal_id`]; workspace ids (and, transitively, the tab/pane
/// ids herdr derives from a workspace id) go through this string form. The
/// origin key never contains the `~` separator, so the first `~` after the
/// prefix always delimits it.
pub fn namespace_public_id(origin: &OriginKey, raw: &str) -> String {
    format!("{FED_PREFIX}{}{SEP}{}", origin.as_str(), raw)
}

/// Namespace a foreign terminal id with its origin: `fed~<key>~<raw>`.
pub fn namespace_terminal_id(origin: &OriginKey, raw: &TerminalId) -> TerminalId {
    TerminalId::from_string(namespace_public_id(origin, raw.as_str()))
}

/// Whether a terminal id is a well-formed namespaced foreign id.
///
/// Validates the full `fed~<key>~<raw>` shape via [`parse_foreign_terminal_id`],
/// not just the prefix, so a malformed value like `fed~n1` (which fails parsing)
/// is not treated as foreign by callers that check this before parsing.
pub fn is_foreign(id: &TerminalId) -> bool {
    parse_foreign_terminal_id(id).is_some()
}

/// Split a namespaced foreign terminal id back into `(origin, raw)`.
///
/// Returns `None` for local (non-namespaced) ids and for ids whose origin
/// segment is not a valid [`OriginKey`]. The origin key never contains the
/// separator, so the first `~` after the prefix delimits it; anything after
/// belongs to the remote's raw id verbatim.
pub fn parse_foreign_terminal_id(id: &TerminalId) -> Option<(OriginKey, TerminalId)> {
    let rest = id.as_str().strip_prefix(FED_PREFIX)?;
    let (key, raw) = rest.split_once(SEP)?;
    if raw.is_empty() {
        return None;
    }
    Some((OriginKey::new(key).ok()?, TerminalId::from_string(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespacing_round_trips() {
        let origin = OriginKey::new("nABC123CNTRL").unwrap();
        let raw = TerminalId::from_string("term_18f2a3c1");

        let foreign = namespace_terminal_id(&origin, &raw);
        assert_eq!(foreign.as_str(), "fed~nABC123CNTRL~term_18f2a3c1");
        assert!(is_foreign(&foreign));

        let (parsed_origin, parsed_raw) = parse_foreign_terminal_id(&foreign).unwrap();
        assert_eq!(parsed_origin, origin);
        assert_eq!(parsed_raw, raw);
    }

    #[test]
    fn public_id_namespacer_shares_terminal_shape() {
        let origin = OriginKey::new("n1").unwrap();
        // The string namespacer and the terminal namespacer must agree so a
        // namespaced workspace id and its terminal ids carry the same prefix.
        assert_eq!(namespace_public_id(&origin, "w5"), "fed~n1~w5");
        assert_eq!(
            namespace_terminal_id(&origin, &TerminalId::from_string("term_1")).as_str(),
            namespace_public_id(&origin, "term_1")
        );
    }

    #[test]
    fn local_ids_are_not_foreign() {
        let local = TerminalId::from_string("term_18f2a3c1");
        assert!(!is_foreign(&local));
        assert!(parse_foreign_terminal_id(&local).is_none());
    }

    #[test]
    fn distinct_origins_never_collide() {
        let raw = TerminalId::from_string("term_1");
        let a = namespace_terminal_id(&OriginKey::new("n1").unwrap(), &raw);
        let b = namespace_terminal_id(&OriginKey::new("n2").unwrap(), &raw);
        assert_ne!(a, b);
    }

    #[test]
    fn malformed_foreign_ids_reject() {
        // Prefix present but missing separator / empty halves. Both the parser
        // and the is_foreign predicate must reject these — a bare `fed~` prefix
        // is not enough to call an id foreign.
        for malformed in ["fed~n1", "fed~~raw", "fed~n1~"] {
            let id = TerminalId::from_string(malformed);
            assert!(parse_foreign_terminal_id(&id).is_none(), "{malformed}");
            assert!(!is_foreign(&id), "{malformed}");
        }
    }

    #[test]
    fn origin_keys_that_would_alias_are_unrepresentable() {
        // "my~box" would namespace to `fed~my~box~term_1`, which parses back as
        // origin "my" with raw "box~term_1" — so such keys cannot be built.
        assert!(OriginKey::new("my~box").is_err());

        // The parsed halves of that id are a *valid* key and a raw remainder;
        // they can never be confused with an origin named "my~box".
        let id = TerminalId::from_string("fed~my~box~term_1");
        let (origin, raw) = parse_foreign_terminal_id(&id).unwrap();
        assert_eq!(origin, OriginKey::new("my").unwrap());
        assert_eq!(raw.as_str(), "box~term_1");
    }
}
