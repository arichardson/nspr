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
use crate::forge::{Forge, PrState};
use crate::stack::Stack;

pub const BEGIN: &str = "<!-- nspr:stack -->";
pub const END: &str = "<!-- /nspr:stack -->";
const LEGACY_SPR_MARKER: &str = "<!-- spr-dependencies -->";
const LEGACY_SPR_END: &str = "<!-- spr-dependencies-list-end -->";

/// A dependency pull request that has already been merged into the trunk and is
/// no longer present in the local commit stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPr {
    pub number: u64,
    pub title: String,
}

/// Extract pull request numbers from the generated `<!-- nspr:stack -->` block
/// of an existing comment, in top-to-bottom order.
pub fn extract_stack_pr_numbers(body: &str) -> Vec<u64> {
    let block = if let Some(start) = body.find(BEGIN)
        && let Some(end) = body[start..].find(END)
    {
        &body[start + BEGIN.len()..start + end]
    } else {
        return Vec::new();
    };

    let mut nums = Vec::new();
    let re = lazy_regex::regex!(r"^\s*-\s*(?:➡️\s*\*\*)?#(\d+)\b");
    for line in block.lines() {
        if let Some(caps) = re.captures(line)
            && let Ok(n) = caps[1].parse::<u64>()
            && !nums.contains(&n)
        {
            nums.push(n);
        }
    }
    nums
}

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
    render_with_merged(config, stack, current, &[])
}

fn format_layer_entry(layer: &crate::stack::Layer, is_current: bool) -> String {
    match (layer.pr, is_current) {
        (Some(n), true) => format!("➡️ **#{n}**"),
        (Some(n), false) => format!("#{n}"),
        (None, true) => format!("➡️ **(not submitted) {}**", layer.subject()),
        (None, false) => format!("(not submitted) {}", layer.subject()),
    }
}

fn layer_direct_merged_prs(
    layer: &crate::stack::Layer,
    merged_set: &std::collections::HashSet<u64>,
) -> Vec<u64> {
    let mut out = Vec::new();
    for &n in &layer.merged_pr_deps {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    for n in layer.external_pr_deps() {
        if merged_set.contains(&n) && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

fn layer_secondary_merged_prs(
    layer: &crate::stack::Layer,
    merged_set: &std::collections::HashSet<u64>,
) -> Vec<u64> {
    if layer.layer_deps().is_empty() {
        return Vec::new();
    }
    layer_direct_merged_prs(layer, merged_set)
}

fn escape_mermaid_label(s: &str) -> String {
    s.replace('"', "#quot;")
}

/// Render the stack seen from layer `current`, including any already-merged
/// dependencies (`merged_deps`) that are no longer in the local commit history.
pub fn render_with_merged(
    config: &Config,
    stack: &Stack,
    current: usize,
    merged_deps: &[MergedPr],
) -> String {
    let mut out = String::from("#### Stack\n\n");

    let merged_set: std::collections::HashSet<u64> =
        merged_deps.iter().map(|m| m.number).collect();
    let component = stack.component_of(current);

    let mut secondary_merged_prs: Vec<u64> = Vec::new();
    for &i in &component {
        for n in layer_secondary_merged_prs(&stack.layers[i], &merged_set) {
            if !secondary_merged_prs.contains(&n) {
                secondary_merged_prs.push(n);
            }
        }
    }

    if stack.is_component_linear(&component) && secondary_merged_prs.is_empty()
    {
        out.push_str(&format!("- `{}`\n", config.trunk));
        for m in merged_deps {
            out.push_str(&format!("- #{}\n", m.number));
        }
        for &i in &component {
            let entry = format_layer_entry(&stack.layers[i], i == current);
            out.push_str(&format!("- {entry}\n"));
        }
    } else {
        let mut raw_merged_for_layer: std::collections::HashMap<
            usize,
            Vec<u64>,
        > = std::collections::HashMap::new();
        for &i in &component {
            raw_merged_for_layer.insert(
                i,
                layer_direct_merged_prs(&stack.layers[i], &merged_set),
            );
        }

        // Transitive reduction for merged PR deps: if layer `i` depends on an
        // ancestor layer `j` in `component` that already depends on merged PR
        // `n`, do not redundantly attach `n` directly to `i`.
        let mut reduced_merged_for_layer: std::collections::HashMap<
            usize,
            Vec<u64>,
        > = std::collections::HashMap::new();
        for &i in &component {
            let mut ancestor_merged = std::collections::HashSet::new();
            let mut stack_dfs = stack.layers[i].layer_deps();
            let mut visited = std::collections::HashSet::new();
            while let Some(anc) = stack_dfs.pop() {
                if visited.insert(anc) {
                    if let Some(ms) = raw_merged_for_layer.get(&anc) {
                        ancestor_merged.extend(ms.iter().copied());
                    }
                    stack_dfs.extend(stack.layers[anc].layer_deps());
                }
            }
            let reduced: Vec<u64> = raw_merged_for_layer[&i]
                .iter()
                .copied()
                .filter(|n| !ancestor_merged.contains(n))
                .collect();
            reduced_merged_for_layer.insert(i, reduced);
        }

        let referenced_merged: std::collections::HashSet<u64> =
            reduced_merged_for_layer
                .values()
                .flatten()
                .copied()
                .collect();

        enum DagNode {
            Merged(MergedPr),
            Layer(usize),
        }

        let merged_by_num: std::collections::HashMap<u64, &MergedPr> =
            merged_deps.iter().map(|m| (m.number, m)).collect();
        let mut emitted_merged = std::collections::HashSet::new();
        let mut nodes: Vec<DagNode> = Vec::new();

        for m in merged_deps {
            if !referenced_merged.contains(&m.number)
                && emitted_merged.insert(m.number)
            {
                nodes.push(DagNode::Merged(m.clone()));
            }
        }

        for &i in &component {
            if let Some(ms) = reduced_merged_for_layer.get(&i) {
                for &n in ms {
                    if emitted_merged.insert(n) {
                        let m = merged_by_num.get(&n).map_or_else(
                            || MergedPr {
                                number: n,
                                title: String::new(),
                            },
                            |&m| m.clone(),
                        );
                        nodes.push(DagNode::Merged(m));
                    }
                }
            }
            nodes.push(DagNode::Layer(i));
        }

        // Group entries by their exact dependency set so each PR line is just
        // the PR (GitHub expands a bare `#N` list item into its status icon and
        // title) and the dependencies are stated once in a short heading.
        // A group is placed at the position of its first member; every later
        // member depends only on the group's key, which is already listed
        // above, so the grouped order stays topological.
        let dep_link =
            |n: u64| format!("[#{n}]({})", config.pull_request_url(n));
        let mut groups: Vec<(Vec<String>, Vec<String>)> = Vec::new();
        for node in &nodes {
            let (key, entry) = match node {
                DagNode::Merged(m) => (Vec::new(), format!("#{}", m.number)),
                DagNode::Layer(i) => {
                    let i = *i;
                    let layer = &stack.layers[i];
                    let mut key: Vec<String> = layer
                        .layer_deps()
                        .into_iter()
                        .map(|j| match stack.layers[j].pr {
                            Some(n) => dep_link(n),
                            None => format!("layer {}", j + 1),
                        })
                        .collect();
                    if let Some(ms) = reduced_merged_for_layer.get(&i) {
                        key.extend(ms.iter().map(|&n| dep_link(n)));
                    }
                    (key, format_layer_entry(layer, i == current))
                }
            };
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, entries)) => entries.push(entry),
                None => groups.push((key, vec![entry])),
            }
        }
        for (key, entries) in &groups {
            if key.is_empty() {
                out.push_str(&format!("- **On `{}`:**\n", config.trunk));
            } else {
                out.push_str(&format!("- **After {}:**\n", key.join(" + ")));
            }
            for entry in entries {
                out.push_str(&format!("  - {entry}\n"));
            }
        }

        let trunk_id = if config
            .trunk
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            config.trunk.as_str()
        } else {
            "trunk"
        };
        out.push_str("\n<details><summary>Dependency graph</summary>\n\n```mermaid\nflowchart BT\n");
        out.push_str(&format!(
            "  {trunk_id}[(\"{}\")]\n",
            escape_mermaid_label(&config.trunk)
        ));

        let mut clicks: Vec<(String, String)> = Vec::new();
        for node in &nodes {
            match node {
                DagNode::Merged(m) => {
                    let node_id = format!("PR{}", m.number);
                    let label = if m.title.is_empty() {
                        format!("#{}", m.number)
                    } else {
                        format!(
                            "#{} {}",
                            m.number,
                            escape_mermaid_label(&m.title)
                        )
                    };
                    out.push_str(&format!(
                        "  {node_id}[\"{label}\"] --> {trunk_id}\n"
                    ));
                    clicks.push((node_id, config.pull_request_url(m.number)));
                }
                DagNode::Layer(i) => {
                    let i = *i;
                    let layer = &stack.layers[i];
                    let (node_id, label) = match layer.pr {
                        Some(n) => {
                            let id = format!("PR{n}");
                            clicks
                                .push((id.clone(), config.pull_request_url(n)));
                            (
                                id,
                                format!(
                                    "#{n} {}",
                                    escape_mermaid_label(layer.subject())
                                ),
                            )
                        }
                        None => (
                            format!("L{i}"),
                            format!(
                                "(not submitted) {}",
                                escape_mermaid_label(layer.subject())
                            ),
                        ),
                    };
                    let mut targets: Vec<String> = layer
                        .layer_deps()
                        .into_iter()
                        .map(|j| match stack.layers[j].pr {
                            Some(n) => format!("PR{n}"),
                            None => format!("L{j}"),
                        })
                        .collect();
                    if let Some(ms) = reduced_merged_for_layer.get(&i) {
                        for &n in ms {
                            targets.push(format!("PR{n}"));
                        }
                    }
                    if targets.is_empty() {
                        out.push_str(&format!(
                            "  {node_id}[\"{label}\"] --> {trunk_id}\n"
                        ));
                    } else {
                        out.push_str(&format!(
                            "  {node_id}[\"{label}\"] --> {}\n",
                            targets.join(" & ")
                        ));
                    }
                }
            }
        }
        for (node_id, url) in clicks {
            out.push_str(&format!("  click {node_id} \"{url}\" \"_blank\"\n"));
        }
        let current_node_id = match stack.layers[current].pr {
            Some(n) => format!("PR{n}"),
            None => format!("L{current}"),
        };
        out.push_str("  classDef current stroke:#f78166,stroke-width:3px\n");
        out.push_str(&format!("  class {current_node_id} current\n"));
        out.push_str("```\n</details>\n");
    }

    out.push_str(
        "\n<sub>Managed by [nspr](https://github.com/arichardson/nspr). \
         Each pull request shows only its own changes.</sub>",
    );
    out
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
    update_for_opts(
        forge,
        config,
        stack,
        &crate::engine::SyncOptions::default(),
    )
    .await
}

/// Extract `(layer_pr, dep_pr)` pairs from the generated `<!-- nspr:stack -->`
/// block of an existing comment.
///
/// Understands the grouped layout (`- **After #a + #b:**` followed by nested
/// `  - #n` entries) as well as the older inline `*(depends on ...)*` and
/// `*(also depends on ...)*` annotations.
pub fn extract_secondary_deps(body: &str) -> Vec<(u64, u64)> {
    let block = if let Some(start) = body.find(BEGIN)
        && let Some(end) = body[start..].find(END)
    {
        &body[start + BEGIN.len()..start + end]
    } else {
        return Vec::new();
    };

    let mut pairs = Vec::new();
    let push = |pairs: &mut Vec<(u64, u64)>, layer_pr: u64, dep_pr: u64| {
        if !pairs.contains(&(layer_pr, dep_pr)) {
            pairs.push((layer_pr, dep_pr));
        }
    };
    let line_re = lazy_regex::regex!(
        r"^\s*-\s*(?:➡️\s*\*\*)?#(\d+)\b.*?\*\((?:also )?depends on (.+)\)\*"
    );
    let heading_re =
        lazy_regex::regex!(r"^-\s*\*\*(?:After (.+)|On .+):\*\*\s*$");
    let entry_re = lazy_regex::regex!(r"^\s+-\s*(?:➡️\s*\*\*)?#(\d+)\b");
    let pr_re = lazy_regex::regex!(r"#(\d+)\b");
    let mut group_deps: Vec<u64> = Vec::new();
    for line in block.lines() {
        if let Some(caps) = heading_re.captures(line) {
            group_deps = caps
                .get(1)
                .map(|m| {
                    pr_re
                        .captures_iter(m.as_str())
                        .filter_map(|c| c[1].parse::<u64>().ok())
                        .collect()
                })
                .unwrap_or_default();
            continue;
        }
        if let Some(caps) = line_re.captures(line)
            && let Ok(layer_pr) = caps[1].parse::<u64>()
        {
            for dep_caps in pr_re.captures_iter(&caps[2]) {
                if let Ok(dep_pr) = dep_caps[1].parse::<u64>() {
                    push(&mut pairs, layer_pr, dep_pr);
                }
            }
            continue;
        }
        if let Some(caps) = entry_re.captures(line)
            && let Ok(layer_pr) = caps[1].parse::<u64>()
        {
            for &dep_pr in &group_deps {
                push(&mut pairs, layer_pr, dep_pr);
            }
        }
    }
    pairs
}

/// Post or update the stack comment on layers selected by `opts` (skipping
/// unrelated stacks when `nspr diff` is scoped to the current stack).
pub async fn update_for_opts(
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    opts: &crate::engine::SyncOptions,
) -> Result<usize> {
    let mut stack = stack.clone();
    let active_prs: std::collections::HashSet<u64> =
        stack.layers.iter().filter_map(|l| l.pr).collect();

    let mut existing_comments: std::collections::HashMap<
        usize,
        Option<crate::forge::Comment>,
    > = std::collections::HashMap::new();
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
        existing_comments.insert(i, existing);
    }

    let mut merged_cache: std::collections::HashMap<u64, Option<MergedPr>> =
        std::collections::HashMap::new();

    let mut updated = 0;
    for component in stack.components() {
        if !component.iter().any(|&i| opts.is_layer_selected(i)) {
            continue;
        }

        let mut candidate_merged_nums: Vec<u64> = Vec::new();
        let mut comment_secondary_pairs: Vec<(u64, u64)> = Vec::new();
        for &i in &component {
            for n in stack.layers[i].external_pr_deps() {
                if !active_prs.contains(&n)
                    && !candidate_merged_nums.contains(&n)
                {
                    candidate_merged_nums.push(n);
                }
            }
            for &n in &stack.layers[i].merged_pr_deps {
                if !active_prs.contains(&n)
                    && !candidate_merged_nums.contains(&n)
                {
                    candidate_merged_nums.push(n);
                }
            }
            if let Some(Some(comment)) = existing_comments.get(&i) {
                for n in extract_stack_pr_numbers(&comment.body) {
                    if !active_prs.contains(&n)
                        && !candidate_merged_nums.contains(&n)
                    {
                        candidate_merged_nums.push(n);
                    }
                }
                for (layer_pr, dep_pr) in extract_secondary_deps(&comment.body)
                {
                    if !active_prs.contains(&dep_pr) {
                        if !candidate_merged_nums.contains(&dep_pr) {
                            candidate_merged_nums.push(dep_pr);
                        }
                        if !comment_secondary_pairs
                            .contains(&(layer_pr, dep_pr))
                        {
                            comment_secondary_pairs.push((layer_pr, dep_pr));
                        }
                    }
                }
            }
        }

        let mut merged_deps: Vec<MergedPr> = Vec::new();
        for n in candidate_merged_nums {
            let entry = match merged_cache.get(&n) {
                Some(cached) => cached.clone(),
                None => {
                    let fetched = match forge.get_pull_request(n).await {
                        Ok(pr) if pr.state == PrState::Merged => {
                            Some(MergedPr {
                                number: pr.number,
                                title: pr.title,
                            })
                        }
                        _ => None,
                    };
                    merged_cache.insert(n, fetched.clone());
                    fetched
                }
            };
            if let Some(m) = entry {
                merged_deps.push(m);
            }
        }

        for (layer_pr, dep_pr) in comment_secondary_pairs {
            if merged_deps.iter().any(|m| m.number == dep_pr)
                && let Some(&i) = component
                    .iter()
                    .find(|&&i| stack.layers[i].pr == Some(layer_pr))
                && !stack.layers[i].merged_pr_deps.contains(&dep_pr)
            {
                stack.layers[i].merged_pr_deps.push(dep_pr);
            }
        }

        let open_prs_in_comp = component
            .iter()
            .filter(|&&i| stack.layers[i].pr.is_some())
            .count();
        let is_stacked = open_prs_in_comp + merged_deps.len() >= 2;

        for &i in &component {
            if !opts.is_layer_selected(i) {
                continue;
            }
            let Some(number) = stack.layers[i].pr else {
                continue;
            };
            let existing = existing_comments.remove(&i).flatten();

            if !is_stacked {
                let Some(comment) = existing else { continue };
                // Anything a human wrote around the table is worth keeping, so
                // the comment only goes away entirely when the table was all of
                // it.
                let body = strip(&comment.body);
                if body.is_empty() {
                    forge.delete_comment(comment.id).await?;
                } else {
                    forge.update_comment(comment.id, &body).await?;
                }
                updated += 1;
                continue;
            }

            let block = render_with_merged(config, &stack, i, &merged_deps);
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
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{Dep, Layer};

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
            trunk: "main".into(),
            base: oid,
            layers: vec![
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 1\n"),
                    pr: Some(101),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Main],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 2\n"),
                    pr: Some(102),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(0),
                    deps: vec![Dep::Layer(0)],
                    merged_pr_deps: Vec::new(),
                },
            ],
        };

        let rendered = render(&config, &stack, 1);
        assert!(rendered.contains("- `main`\n"));
        assert!(rendered.contains("- #101\n"));
        assert!(rendered.contains("- ➡️ **#102**\n"));
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
            trunk: "main".into(),
            base: oid,
            layers: vec![
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 1\n"),
                    pr: Some(101),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Main],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Layer 2\n"),
                    pr: Some(102),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(0),
                    deps: vec![Dep::Layer(0)],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse(
                        "Layer 3 sibling\n",
                    ),
                    pr: Some(103),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(0),
                    deps: vec![Dep::Layer(0)],
                    merged_pr_deps: Vec::new(),
                },
            ],
        };

        let rendered = render(&config, &stack, 1);
        assert!(!stack.is_linear());
        assert!(
            rendered.contains(
                "- **On `main`:**\n\
                 \x20 - #101\n\
                 - **After [#101](https://github.com/owner/repo/pull/101):**\n\
                 \x20 - ➡️ **#102**\n\
                 \x20 - #103\n"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("<details><summary>Dependency graph</summary>"),
            "{rendered}"
        );
        assert!(
            rendered.contains("PR102[\"#102 Layer 2\"] --> PR101\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("PR103[\"#103 Layer 3 sibling\"] --> PR101\n"),
            "{rendered}"
        );
        assert!(rendered.contains("class PR102 current\n"), "{rendered}");
    }

    #[test]
    fn render_multi_dependency_stack_before_and_after_merge() {
        let config = Config::new(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "user".into(),
        );
        let oid = git2::Oid::ZERO_SHA1;
        let stack_before = Stack {
            trunk: "main".into(),
            base: oid,
            layers: vec![
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("A1\n"),
                    pr: Some(56),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Main],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("A2\n"),
                    pr: Some(57),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(0),
                    deps: vec![Dep::Layer(0)],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("B1\n"),
                    pr: Some(58),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Main],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Top\n"),
                    pr: Some(59),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Layer(1), Dep::Layer(2)],
                    merged_pr_deps: Vec::new(),
                },
            ],
        };

        let before = render(&config, &stack_before, 1);
        assert!(
            before.contains(
                "- **On `main`:**\n\
                 \x20 - #56\n\
                 \x20 - #58\n\
                 - **After [#56](https://github.com/owner/repo/pull/56):**\n\
                 \x20 - ➡️ **#57**\n\
                 - **After [#57](https://github.com/owner/repo/pull/57) + [#58](https://github.com/owner/repo/pull/58):**\n\
                 \x20 - #59\n"
            ),
            "{before}"
        );
        assert!(
            before.contains(
                "```mermaid\n\
                 flowchart BT\n\
                 \x20 main[(\"main\")]\n\
                 \x20 PR56[\"#56 A1\"] --> main\n\
                 \x20 PR57[\"#57 A2\"] --> PR56\n\
                 \x20 PR58[\"#58 B1\"] --> main\n\
                 \x20 PR59[\"#59 Top\"] --> PR57 & PR58\n"
            ),
            "{before}"
        );

        // After #58 is merged into main, #59 has 1 open layer_dep (#57) and
        // #58 in merged_pr_deps. The rendered DAG stays identical.
        let stack_after = Stack {
            trunk: "main".into(),
            base: oid,
            layers: vec![
                stack_before.layers[0].clone(),
                stack_before.layers[1].clone(),
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("Top\n"),
                    pr: Some(59),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(1),
                    deps: vec![Dep::Layer(1), Dep::Main],
                    merged_pr_deps: vec![58],
                },
            ],
        };
        let after = render_with_merged(
            &config,
            &stack_after,
            1,
            &[MergedPr {
                number: 58,
                title: "B1".into(),
            }],
        );
        assert_eq!(after, before);
    }

    #[test]
    fn render_multiple_independent_stacks_isolates_each_component() {
        let config = Config::new(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "user".into(),
        );
        let oid = git2::Oid::ZERO_SHA1;
        let stack = Stack {
            trunk: "main".into(),
            base: oid,
            layers: vec![
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("ToolA 1\n"),
                    pr: Some(101),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Main],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("ToolA 2\n"),
                    pr: Some(102),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(0),
                    deps: vec![Dep::Layer(0)],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("ToolB 1\n"),
                    pr: Some(201),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Main,
                    deps: vec![Dep::Main],
                    merged_pr_deps: Vec::new(),
                },
                Layer {
                    commit: oid,
                    parent: oid,
                    message: crate::trailers::CommitMessage::parse("ToolB 2\n"),
                    pr: Some(202),
                    dep_spec: None,
                    dep_specs: Vec::new(),
                    dep: Dep::Layer(2),
                    deps: vec![Dep::Layer(2)],
                    merged_pr_deps: Vec::new(),
                },
            ],
        };

        let comment_a = render(&config, &stack, 0);
        assert!(comment_a.contains("- ➡️ **#101**\n"));
        assert!(comment_a.contains("- #102\n"));
        assert!(!comment_a.contains("#201"));
        assert!(!comment_a.contains("#202"));
        assert!(!comment_a.contains("  -"));

        let comment_b = render(&config, &stack, 3);
        assert!(comment_b.contains("- #201\n"));
        assert!(comment_b.contains("- ➡️ **#202**\n"));
        assert!(!comment_b.contains("#101"));
        assert!(!comment_b.contains("#102"));
        assert!(!comment_b.contains("  -"));
    }

    fn dag_layer(subject: &str, pr: u64, deps: Vec<Dep>) -> Layer {
        let oid = git2::Oid::ZERO_SHA1;
        let dep = Layer::compute_effective_dep(&deps);
        Layer {
            commit: oid,
            parent: oid,
            message: crate::trailers::CommitMessage::parse(&format!(
                "{subject}\n"
            )),
            pr: Some(pr),
            dep_spec: None,
            dep_specs: Vec::new(),
            dep,
            deps,
            merged_pr_deps: Vec::new(),
        }
    }

    /// `B`, `C`, `E` on main; `A -> B & C`; `D -> A & E`; `F -> D`.
    fn complex_dag() -> Stack {
        Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![
                dag_layer("B", 64, vec![Dep::Main]),
                dag_layer("C", 65, vec![Dep::Main]),
                dag_layer("A", 66, vec![Dep::Layer(0), Dep::Layer(1)]),
                dag_layer("E", 67, vec![Dep::Main]),
                dag_layer("D", 68, vec![Dep::Layer(2), Dep::Layer(3)]),
                dag_layer("F", 69, vec![Dep::Layer(4)]),
            ],
        }
    }

    #[test]
    fn render_complex_dag_groups_prs_by_shared_dependencies() {
        let config = Config::new(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "user".into(),
        );
        let rendered = render(&config, &complex_dag(), 2);
        let url = |n: u64| format!("https://github.com/owner/repo/pull/{n}");
        let expected = format!(
            "- **On `main`:**\n\
             \x20 - #64\n\
             \x20 - #65\n\
             \x20 - #67\n\
             - **After [#64]({}) + [#65]({}):**\n\
             \x20 - ➡️ **#66**\n\
             - **After [#66]({}) + [#67]({}):**\n\
             \x20 - #68\n\
             - **After [#68]({}):**\n\
             \x20 - #69\n",
            url(64),
            url(65),
            url(66),
            url(67),
            url(68),
        );
        assert!(rendered.contains(&expected), "{rendered}");
        assert!(!rendered.contains("depends on"), "{rendered}");
    }

    #[test]
    fn grouped_comment_round_trips_through_extract_functions() {
        let config = Config::new(
            "owner".into(),
            "repo".into(),
            "main".into(),
            "user".into(),
        );
        let body = splice("", &render(&config, &complex_dag(), 5));
        assert_eq!(
            extract_stack_pr_numbers(&body),
            vec![64, 65, 67, 66, 68, 69]
        );
        assert_eq!(
            extract_secondary_deps(&body),
            vec![(66, 64), (66, 65), (68, 66), (68, 67), (69, 68)]
        );
    }

    #[test]
    fn extract_secondary_deps_still_reads_inline_annotations() {
        let body = format!(
            "{BEGIN}\n- #57\n  - #59 *(also depends on [#58](https://x/58))*\n- #60 *(depends on [#57](https://x/57), [#58](https://x/58))*\n{END}"
        );
        assert_eq!(
            extract_secondary_deps(&body),
            vec![(59, 58), (60, 57), (60, 58)]
        );
    }
}
