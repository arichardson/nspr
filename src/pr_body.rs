//! Managing `<!-- nspr:warning -->` blocks in pull request descriptions.
//!
//! When a repository allows merge commits or rebase merges (rather than only
//! "Squash and merge"), merging an `nspr` branch via the GitHub Web UI with the
//! wrong merge strategy will include intermediate `[nspr]` revision commits in
//! trunk history. On such repositories, [`splice_warning`] appends a warning
//! disclaimer after a `---` separator at the bottom of the PR description.
//! On squash-only repositories, [`splice_warning`] strips any existing warning
//! block and leaves the description clean.

pub const WARNING_BEGIN: &str = "<!-- nspr:warning -->";
pub const WARNING_END: &str = "<!-- /nspr:warning -->";

pub const MERGE_STRATEGY_NOTE: &str = "\
> [!WARNING]
> It is recommended that this PR is merged using `nspr land`. If merging via the GitHub Web UI, please make sure to select **Squash and merge** and use the **PR title and description** as the commit message (rather than the default `[nspr]` branch commits).";

pub const WARNING_BLOCK: &str = "\
<!-- nspr:warning -->
---

> [!WARNING]
> It is recommended that this PR is merged using `nspr land`. If merging via the GitHub Web UI, please make sure to select **Squash and merge** and use the **PR title and description** as the commit message (rather than the default `[nspr]` branch commits).
<!-- /nspr:warning -->";

/// Ensure `body` ends with [`WARNING_BLOCK`] when `warn_merge_strategy` is
/// `true`, or has any existing warning block stripped when `false`.
pub fn splice_warning(body: &str, warn_merge_strategy: bool) -> String {
    splice_warning_with_deps(body, warn_merge_strategy, &[], "main")
}

/// Ensure `body` ends with a `<!-- nspr:warning -->` block containing any
/// active multi-dependency notice (`multi_deps` has 2+ open dependencies) and/or
/// the merge strategy disclaimer (`warn_merge_strategy`).
pub fn splice_warning_with_deps(
    body: &str,
    warn_merge_strategy: bool,
    multi_deps: &[String],
    trunk: &str,
) -> String {
    let clean = strip_warning(body);
    let has_multi_deps = multi_deps.len() >= 2;
    if !warn_merge_strategy && !has_multi_deps {
        return clean;
    }

    let mut sections = Vec::new();
    if has_multi_deps {
        let joined = multi_deps.join(", ");
        sections.push(format!(
            "> [!IMPORTANT]\n\
             > This pull request depends on multiple open pull requests ({joined}) and targets `{trunk}` until all but one dependency have landed. \
             **Do not merge this pull request yet** — its diff currently includes changes from its unmerged dependencies."
        ));
    }
    if warn_merge_strategy {
        sections.push(MERGE_STRATEGY_NOTE.to_string());
    }

    let block = format!(
        "{WARNING_BEGIN}\n---\n\n{}\n{WARNING_END}",
        sections.join("\n\n")
    );
    if clean.is_empty() {
        block
    } else {
        format!("{clean}\n\n{block}")
    }
}

/// Strip a warning block from `body`, returning the clean description.
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
    fn splice_warning_leaves_clean_body_untouched_when_false() {
        let desc = "This adds the widget trait.";
        assert_eq!(splice_warning(desc, false), desc);
    }

    #[test]
    fn splice_warning_appends_at_bottom_when_true() {
        let desc = "This adds the widget trait.";
        let expected = format!("{desc}\n\n{WARNING_BLOCK}");
        let result = splice_warning(desc, true);
        assert_eq!(result, expected);
        assert_eq!(splice_warning(&result, true), expected);
        assert_eq!(splice_warning(&result, false), desc);
    }

    #[test]
    fn splice_warning_strips_legacy_warning() {
        let old = format!(
            "{WARNING_BEGIN}\nOld warning\n{WARNING_END}\n\nOriginal text"
        );
        assert_eq!(splice_warning(&old, false), "Original text");
        assert_eq!(
            splice_warning(&old, true),
            format!("Original text\n\n{WARNING_BLOCK}")
        );
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
