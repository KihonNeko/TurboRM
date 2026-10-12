//! Selection of the page used for a document's library thumbnail.
//!
//! `coverPageNumber` is a zero-based display-page number. The firmware reader
//! defaults it to `-1`; that value means the last visited page. Modern content
//! records the latter by page identity in `cPages.lastOpened`, while legacy
//! content uses the numeric `lastOpenedPage` metadata field. An absent cover
//! setting also uses the last visited page. Missing, stale, malformed, deleted,
//! or out-of-range page selections fall back to the first
//! active page. Returning an index into `pages` preserves that page's PDF
//! redirection and never revives a deleted page-map record.

use crate::notebook::PageRef;
use serde_json::Value;

/// Return the selected index in the active, display-ordered `pages` slice.
///
/// `None` is returned only when the document has no active pages.
pub fn page_index(content: &Value, metadata: &Value, pages: &[PageRef]) -> Option<usize> {
    if pages.is_empty() {
        return None;
    }

    let selected = match content.get("coverPageNumber") {
        Some(value) if value.as_i64().is_some_and(|n| n >= 0) => value
            .as_i64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n < pages.len()),
        // The native content reader supplies -1 when the field is absent. Keep
        // other negative and malformed values invalid rather than assigning
        // them semantics.
        None => last_opened_page(content, metadata, pages),
        Some(value) if value.as_i64() == Some(-1) => last_opened_page(content, metadata, pages),
        Some(_) => None,
    };

    Some(selected.unwrap_or(0))
}

/// The page the tablet last showed, as an index into the active,
/// display-ordered `pages` slice: `cPages.lastOpened` by page identity, else
/// the legacy `lastOpenedPage` metadata number.
///
/// `None` when neither names an active page, so the caller picks its own
/// default instead of opening a stale or deleted page.
pub fn last_opened_page(content: &Value, metadata: &Value, pages: &[PageRef]) -> Option<usize> {
    if let Some(id) = content
        .get("cPages")
        .and_then(|v| v.get("lastOpened"))
        .and_then(|v| v.get("value"))
        .and_then(Value::as_str)
    {
        // Identity lookup deliberately rejects stale and tombstoned IDs: the
        // caller's PageRefs contain active pages only.
        return pages.iter().position(|page| page.id == id);
    }

    metadata
        .get("lastOpenedPage")
        .and_then(Value::as_i64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n < pages.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(id: &str, background: Option<usize>) -> PageRef {
        PageRef {
            id: id.to_owned(),
            background,
        }
    }

    #[test]
    fn defaults_to_first_visible_page() {
        let pages = [page("first", Some(8)), page("second", Some(2))];
        assert_eq!(page_index(&json!({}), &json!({}), &pages), Some(0));
        assert_eq!(
            page_index(&json!({"coverPageNumber": 0}), &json!({}), &pages),
            Some(0)
        );
    }

    #[test]
    fn selects_configured_display_page_without_losing_redirection() {
        let pages = [page("first", Some(8)), page("second", Some(2))];
        let index = page_index(&json!({"coverPageNumber": 1}), &json!({}), &pages);
        assert_eq!(index, Some(1));
        assert_eq!(pages[index.unwrap()].background, Some(2));
    }

    #[test]
    fn minus_one_uses_legacy_last_opened_page() {
        let pages = [page("first", None), page("second", None)];
        assert_eq!(
            page_index(
                &json!({"coverPageNumber": -1}),
                &json!({"lastOpenedPage": 1}),
                &pages
            ),
            Some(1)
        );
    }

    #[test]
    fn v2_last_opened_register_uses_page_identity() {
        let pages = [page("page-b", Some(4)), page("page-a", Some(0))];
        let content = json!({
            "coverPageNumber": -1,
            "cPages": {"lastOpened": {"timestamp": "1:9", "value": "page-a"}}
        });
        assert_eq!(
            page_index(&content, &json!({"lastOpenedPage": 0}), &pages),
            Some(1)
        );
    }

    #[test]
    fn stale_deleted_or_invalid_selections_fall_back_to_first_visible() {
        let pages = [page("visible", None), page("also-visible", None)];
        for content in [
            json!({"coverPageNumber": -1, "cPages": {"lastOpened": {"value": "deleted"}}}),
            json!({"coverPageNumber": -2}),
            json!({"coverPageNumber": "one"}),
            json!({"coverPageNumber": 99}),
        ] {
            assert_eq!(
                page_index(&content, &json!({"lastOpenedPage": 99}), &pages),
                Some(0)
            );
        }
    }

    #[test]
    fn malformed_cover_does_not_use_valid_last_opened_page() {
        let pages = [page("first", None), page("second", None)];
        let metadata = json!({"lastOpenedPage": 1});
        for content in [
            json!({"coverPageNumber": "one"}),
            json!({"coverPageNumber": true}),
        ] {
            assert_eq!(page_index(&content, &metadata, &pages), Some(0));
        }
    }

    #[test]
    fn last_opened_page_ignores_the_cover_setting() {
        // A document whose cover is pinned to page 1 still reopens where the
        // tablet left it.
        let pages = [
            page("first", None),
            page("second", None),
            page("third", None),
        ];
        let content = json!({
            "coverPageNumber": 0,
            "cPages": {"lastOpened": {"timestamp": "1:2", "value": "third"}}
        });
        assert_eq!(last_opened_page(&content, &json!({}), &pages), Some(2));
        assert_eq!(
            last_opened_page(
                &json!({"coverPageNumber": 0}),
                &json!({"lastOpenedPage": 1}),
                &pages
            ),
            Some(1)
        );
        // Nothing recorded, or a page that no longer exists: no opinion.
        assert_eq!(last_opened_page(&json!({}), &json!({}), &pages), None);
        assert_eq!(
            last_opened_page(
                &json!({"cPages": {"lastOpened": {"value": "deleted"}}}),
                &json!({"lastOpenedPage": 1}),
                &pages
            ),
            None
        );
    }

    #[test]
    fn empty_document_has_no_cover() {
        assert_eq!(page_index(&json!({}), &json!({}), &[]), None);
    }
}
