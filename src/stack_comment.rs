//! The stack table `nspr` posts on each pull request.
//!
//! GitHub shows a stacked pull request no differently from a standalone one,
//! so without this a reviewer has no way to tell that #102 is meaningless
//! until #101 lands, or that #103 is a sibling rather than a successor.
//!
//! The table is written into a marker-delimited block so that anything a human
//! adds around it survives being rewritten:
//!
//! ```text
//! <!-- nspr:stack -->
//! ...generated...
//! <!-- /nspr:stack -->
//! ```
//!
//! Splicing rather than replacing matters more than it looks: people reply to
//! these comments, and quietly eating an edit is the sort of thing that makes
//! a tool untrustworthy.

use color_eyre::eyre::Result;

use crate::config::Config;
use crate::forge::Forge;
use crate::stack::{Dep, Stack};

pub const BEGIN: &str = "<!-- nspr:stack -->";
pub const END: &str = "<!-- /nspr:stack -->";
const LEGACY_SPR_MARKER: &str = "<!-- spr-dependencies -->";
const LEGACY_SPR_END: &str = "<!-- spr-dependencies-list-end -->";

/// Replace the generated block in `body`, or append one if there is none.
///
/// A body that has a start marker but no end marker is treated as having no
/// block at all, rather than as licence to eat the rest of the comment.
pub fn splice(body: &str, block: &str) -> String {
    let block = format!("{BEGIN}\n{}\n{END}", block.trim_end());

    if let Some(start) = body.find(BEGIN)
        && let Some(end) = body[start..].find(END)
    {
        let end = start + end + END.len();
        return format!("{}{block}{}", &body[..start], &body[end..]);
    }

    if let Some(start) = body.find(LEGACY_SPR_MARKER) {
        let end = match body[start..].find(LEGACY_SPR_END) {
            Some(rel_end) => start + rel_end + LEGACY_SPR_END.len(),
            None => body.len(),
        };
        return format!("{}{block}{}", &body[..start], &body[end..]);
    }

    if body.trim().is_empty() {
        block
    } else {
        format!("{}\n\n{block}", body.trim_end())
    }
}

/// Remove the generated block from `body`, leaving anything a human wrote.
///
/// The inverse of [`splice`], for when a pull request stops being part of a
/// stack and the table has to come down.
pub fn strip(body: &str) -> String {
    let mut out = body.to_string();

    if let Some(start) = out.find(BEGIN)
        && let Some(end) = out[start..].find(END)
    {
        let end = start + end + END.len();
        out = format!("{}{}", &out[..start], &out[end..]);
    }

    if let Some(start) = out.find(LEGACY_SPR_MARKER) {
        let end = match out[start..].find(LEGACY_SPR_END) {
            Some(rel_end) => start + rel_end + LEGACY_SPR_END.len(),
            None => out.len(),
        };
        out = format!("{}{}", &out[..start], &out[end..]);
    }

    out.trim().to_string()
}

/// Render the stack as a tree, seen from layer `current`.
///
/// A tree rather than a list because the dependency graph is a tree: siblings
/// that merely happen to be adjacent in the local commit order must not look
/// like they depend on each other.
pub fn render(config: &Config, stack: &Stack, current: usize) -> String {
    let mut out = String::from("#### Stack\n\n");
    out.push_str(&format!("- `{}`\n", config.trunk));

    if stack.is_linear() {
        for (i, layer) in stack.layers.iter().enumerate() {
            let reference = match layer.pr {
                Some(n) => format!("#{n}"),
                None => "(not submitted)".to_string(),
            };
            if i == current {
                out.push_str(&format!(
                    "- ➡️ **{reference} {}**\n",
                    layer.subject(),
                ));
            } else {
                out.push_str(&format!("- {reference} {}\n", layer.subject(),));
            }
        }
    } else {
        // Group children by parent layer so DFS traversal keeps branches intact
        let mut children_map: std::collections::HashMap<
            Option<usize>,
            Vec<usize>,
        > = std::collections::HashMap::new();
        for (i, layer) in stack.layers.iter().enumerate() {
            let parent = match layer.dep {
                Dep::Main | Dep::ExternalPr(_) => None,
                Dep::Layer(j) => Some(j),
            };
            children_map.entry(parent).or_default().push(i);
        }

        render_children(stack, &children_map, None, 1, current, &mut out);
    }

    out.push_str(
        "\n<sub>Managed by [nspr](https://github.com/arichardson/nspr). \
         Each pull request shows only its own changes.</sub>",
    );
    out
}

fn render_children(
    stack: &Stack,
    children_map: &std::collections::HashMap<Option<usize>, Vec<usize>>,
    parent: Option<usize>,
    depth: usize,
    current: usize,
    out: &mut String,
) {
    if let Some(kids) = children_map.get(&parent) {
        for &i in kids {
            let layer = &stack.layers[i];
            let indent = "  ".repeat(depth);
            let reference = match layer.pr {
                Some(n) => format!("#{n}"),
                None => "(not submitted)".to_string(),
            };
            if i == current {
                out.push_str(&format!(
                    "{indent}- ➡️ **{reference} {}**\n",
                    layer.subject(),
                ));
            } else {
                out.push_str(&format!(
                    "{indent}- {reference} {}\n",
                    layer.subject(),
                ));
            }
            render_children(
                stack,
                children_map,
                Some(i),
                depth + 1,
                current,
                out,
            );
        }
    }
}

/// Post or update the stack comment on every layer that is part of a stack, and
/// take it down from every layer that is not.
///
/// Comments are only rewritten when their content actually changes: every
/// edit sends a notification, and a tool that re-notifies a dozen reviewers on
/// every `nspr diff` would be turned off within the week.
///
/// Retargeting a pull request at the trunk is the interesting case. Without the
/// take-down it keeps a table saying it is blocked on work it no longer depends
/// on, and reviewers have no reason to doubt it.
pub async fn update_all(
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
) -> Result<usize> {
    // A pull request on its own is not a stack; the table would be noise.
    let stacked: std::collections::HashSet<usize> = stack
        .pr_components()
        .into_iter()
        .filter(|component| component.len() >= 2)
        .flatten()
        .collect();

    let mut updated = 0;
    for (i, layer) in stack.layers.iter().enumerate() {
        let Some(number) = layer.pr else { continue };

        let existing =
            forge
                .list_own_comments(number)
                .await?
                .into_iter()
                .find(|c| {
                    c.body.contains(BEGIN) || c.body.contains(LEGACY_SPR_MARKER)
                });

        if !stacked.contains(&i) {
            let Some(comment) = existing else { continue };
            // Anything a human wrote around the table is worth keeping, so the
            // comment only goes away entirely when the table was all of it.
            let body = strip(&comment.body);
            if body.is_empty() {
                forge.delete_comment(comment.id).await?;
            } else {
                forge.update_comment(comment.id, &body).await?;
            }
            updated += 1;
            continue;
        }

        let block = render(config, stack, i);
        match existing {
            Some(comment) => {
                let body = splice(&comment.body, &block);
                if body != comment.body {
                    forge.update_comment(comment.id, &body).await?;
                    updated += 1;
                }
            }
            None => {
                forge.create_comment(number, &splice("", &block)).await?;
                updated += 1;
            }
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Layer;

    #[test]
    fn splice_into_an_empty_body() {
        assert_eq!(splice("", "table"), format!("{BEGIN}\ntable\n{END}"));
    }

    #[test]
    fn splice_preserves_text_around_the_block() {
        let body = format!("before\n\n{BEGIN}\nold\n{END}\n\nafter");
        let out = splice(&body, "new");
        assert!(out.starts_with("before\n\n"));
        assert!(out.ends_with("\n\nafter"));
        assert!(out.contains("new"));
        assert!(!out.contains("old"));
    }

    #[test]
    fn splice_appends_when_there_is_no_block() {
        let out = splice("just a comment", "table");
        assert!(out.starts_with("just a comment\n\n"));
        assert!(out.contains("table"));
    }

    /// A truncated block must not license eating the rest of the comment.
    #[test]
    fn splice_ignores_an_unterminated_block() {
        let body = format!("keep me\n{BEGIN}\ndangling");
        let out = splice(&body, "table");
        assert!(out.contains("keep me"));
        assert!(out.contains("dangling"));
        assert!(out.contains(END));
    }

    #[test]
    fn splice_is_idempotent() {
        let once = splice("hello", "table");
        assert_eq!(splice(&once, "table"), once);
    }

    #[test]
    fn render_linear_stack_is_completely_flat() {
        let config = Config::new(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "user".into(),
        );
        let oid = git2::Oid::ZERO_SHA1;
        let stack = Stack {
            base: oid,
            layers: vec![
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 1\n"),
                    pr: Some(101),
                    dep_spec: None,
                    dep: Dep::Main,
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 2\n"),
                    pr: Some(102),
                    dep_spec: None,
                    dep: Dep::Layer(0),
                },
            ],
        };

        let rendered = render(&config, &stack, 1);
        assert!(rendered.contains("- `main`\n"));
        assert!(rendered.contains("- #101 Layer 1\n"));
        assert!(rendered.contains("- ➡️ **#102 Layer 2**\n"));
        // Ensure no indented bullet points
        assert!(!rendered.contains("  -"));
    }

    #[test]
    fn render_branching_stack_is_indented() {
        let config = Config::new(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "user".into(),
        );
        let oid = git2::Oid::ZERO_SHA1;
        let stack = Stack {
            base: oid,
            layers: vec![
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 1\n"),
                    pr: Some(101),
                    dep_spec: None,
                    dep: Dep::Main,
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 2\n"),
                    pr: Some(102),
                    dep_spec: None,
                    dep: Dep::Layer(0),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse(
                        "Layer 3 sibling\n",
                    ),
                    pr: Some(103),
                    dep_spec: None,
                    dep: Dep::Layer(0),
                },
            ],
        };

        let rendered = render(&config, &stack, 1);
        assert!(!stack.is_linear());
        assert!(rendered.contains("  - #101 Layer 1\n"));
        assert!(rendered.contains("    - ➡️ **#102 Layer 2**\n"));
        assert!(rendered.contains("    - #103 Layer 3 sibling\n"));
    }
}
