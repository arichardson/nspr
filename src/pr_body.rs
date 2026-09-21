//! Cleaning up legacy `<!-- nspr:warning -->` blocks from pull request
//! descriptions.
//!
//! Previously, before `nspr` registered stacks with GitHub's native Stacks
//! REST API (`/repos/{owner}/{repo}/stacks`) and switched to 1-parent linear
//! branch commits, `nspr` injected a `<!-- nspr:warning -->` alert at the top
//! of stacked PR descriptions. That banner is no longer added; [`strip_warning`]
//! and [`splice_warning`] strip any legacy block if present so existing PRs
//! are cleaned up automatically.

pub const WARNING_BEGIN: &str = "<!-- nspr:warning -->";
pub const WARNING_END: &str = "<!-- /nspr:warning -->";

/// Return `body` with any legacy `<!-- nspr:warning -->` block removed.
pub fn splice_warning(body: &str, _is_stacked: bool) -> String {
    strip_warning(body)
}

/// Strip a legacy warning block from `body`, returning the clean description.
///
/// If the body contains a start marker but no end marker, it is treated as having
/// no valid warning block to avoid eating human-written content.
pub fn strip_warning(body: &str) -> String {
    if let Some(start) = body.find(WARNING_BEGIN)
        && let Some(end_offset) = body[start..].find(WARNING_END)
    {
        let end = start + end_offset + WARNING_END.len();
        let before = body[..start].trim();
        let after = body[end..].trim();

        if before.is_empty() {
            after.to_string()
        } else if after.is_empty() {
            before.to_string()
        } else {
            format!("{before}\n\n{after}")
        }
    } else {
        body.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_warning_leaves_clean_body_untouched() {
        let desc = "This adds the widget trait.";
        assert_eq!(splice_warning(desc, true), desc);
        assert_eq!(splice_warning(desc, false), desc);
    }

    #[test]
    fn splice_warning_strips_legacy_warning() {
        let old = format!(
            "{WARNING_BEGIN}\nOld warning\n{WARNING_END}\n\nOriginal text"
        );
        assert_eq!(splice_warning(&old, true), "Original text");
        assert_eq!(splice_warning(&old, false), "Original text");
    }

    #[test]
    fn strip_warning_on_clean_body() {
        let text = "Nothing special\nMultiple lines";
        assert_eq!(strip_warning(text), text);
    }

    #[test]
    fn strip_warning_on_warning_alone() {
        let with_warning =
            format!("{WARNING_BEGIN}\nOld warning\n{WARNING_END}");
        assert_eq!(strip_warning(&with_warning), "");
    }

    #[test]
    fn strip_warning_surrounding_text() {
        let body = format!(
            "Before note\n\n{WARNING_BEGIN}\nOld warning\n{WARNING_END}\n\nAfter details"
        );
        assert_eq!(strip_warning(&body), "Before note\n\nAfter details");
    }

    #[test]
    fn strip_warning_unterminated_marker_is_untouched() {
        let malformed = format!("{WARNING_BEGIN}\nSome text without end tag");
        assert_eq!(strip_warning(&malformed), malformed);
    }
}
