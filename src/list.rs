//! `nspr list`: inspect open pull request stacks on GitHub.
//!
//! Stacks are grouped into trees by matching each pull request's base branch
//! against the head branches of other open pull requests. Roots are those
//! based on the trunk or an external branch.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use console::style;

use crate::forge::{ListedPr, ReviewDecision};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackNode {
    pub pr: ListedPr,
    pub children: Vec<StackNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStack {
    /// The base branch the bottom of the stack sits on (e.g. `main`).
    pub base: String,
    pub roots: Vec<StackNode>,
}

/// Group a flat list of pull requests into hierarchical stacks.
pub fn build_stacks(prs: Vec<ListedPr>, trunk: &str) -> Vec<PrStack> {
    let heads: HashSet<String> = prs.iter().map(|p| p.head.clone()).collect();

    let mut children_map: HashMap<String, Vec<ListedPr>> = HashMap::new();
    let mut roots: Vec<ListedPr> = Vec::new();

    for pr in prs {
        if heads.contains(&pr.base) && pr.base != pr.head {
            children_map.entry(pr.base.clone()).or_default().push(pr);
        } else {
            roots.push(pr);
        }
    }

    // Sort children by PR number for deterministic output
    for children in children_map.values_mut() {
        children.sort_by_key(|p| p.number);
    }

    // Group roots by base branch, putting trunk roots first
    let mut stacks_by_base: HashMap<String, Vec<StackNode>> = HashMap::new();
    let mut visited = HashSet::new();

    // Sort roots: trunk roots first, then by PR number
    roots.sort_by(|a, b| {
        let a_trunk = a.base == trunk;
        let b_trunk = b.base == trunk;
        match (a_trunk, b_trunk) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.base.cmp(&b.base).then_with(|| a.number.cmp(&b.number)),
        }
    });

    let mut base_order: Vec<String> = Vec::new();
    for root in roots {
        let base = root.base.clone();
        if !stacks_by_base.contains_key(&base) {
            base_order.push(base.clone());
        }
        let node = build_node(root, &children_map, &mut visited);
        stacks_by_base.entry(base).or_default().push(node);
    }

    base_order
        .into_iter()
        .map(|base| {
            let roots = stacks_by_base.remove(&base).unwrap_or_default();
            PrStack { base, roots }
        })
        .collect()
}

fn build_node(
    pr: ListedPr,
    children_map: &HashMap<String, Vec<ListedPr>>,
    visited: &mut HashSet<u64>,
) -> StackNode {
    visited.insert(pr.number);
    let mut children = Vec::new();
    if let Some(kids) = children_map.get(&pr.head) {
        for k in kids {
            if !visited.contains(&k.number) {
                children.push(build_node(k.clone(), children_map, visited));
            }
        }
    }
    StackNode { pr, children }
}

/// Render grouped stacks for terminal display.
pub fn format_stacks(stacks: &[PrStack]) -> String {
    if stacks.is_empty() {
        return "No open pull requests found.\n".to_string();
    }

    let mut out = String::new();
    for (i, stack) in stacks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for root in &stack.roots {
            format_node(root, 0, &mut out);
        }
    }
    out
}

fn format_node(node: &StackNode, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let decision = format_decision(node.pr.review_decision, node.pr.draft);
    let num = style(format!("#{}", node.pr.number)).bold();
    let _ = writeln!(out, "{indent}{num}  {decision}  {}", node.pr.title);
    for child in &node.children {
        format_node(child, depth + 1, out);
    }
}

fn format_decision(decision: Option<ReviewDecision>, draft: bool) -> String {
    if draft {
        return format!("{}", style("[Draft]").dim());
    }
    match decision {
        Some(ReviewDecision::Approved) => {
            format!("{}", style("[Approved]").green())
        }
        Some(ReviewDecision::ChangesRequested) => {
            format!("{}", style("[Changes Requested]").red())
        }
        Some(ReviewDecision::ReviewRequired) | None => {
            format!("{}", style("[Pending]").yellow())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::PrState;

    fn make_pr(
        number: u64,
        title: &str,
        base: &str,
        head: &str,
        decision: Option<ReviewDecision>,
    ) -> ListedPr {
        ListedPr {
            number,
            title: title.to_string(),
            state: PrState::Open,
            draft: false,
            base: base.to_string(),
            head: head.to_string(),
            review_decision: decision,
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    #[test]
    fn empty_list_reports_no_pull_requests() {
        let stacks = build_stacks(vec![], "main");
        assert!(stacks.is_empty());
        assert_eq!(format_stacks(&stacks), "No open pull requests found.\n");
    }

    #[test]
    fn groups_linear_stack_hierarchically() {
        let prs = vec![
            make_pr(17, "Wire up cache", "main", "users/me/cache", None),
            make_pr(
                18,
                "Add metrics",
                "users/me/cache",
                "users/me/metrics",
                Some(ReviewDecision::Approved),
            ),
        ];

        let stacks = build_stacks(prs, "main");
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].base, "main");
        assert_eq!(stacks[0].roots.len(), 1);
        assert_eq!(stacks[0].roots[0].pr.number, 17);
        assert_eq!(stacks[0].roots[0].children.len(), 1);
        assert_eq!(stacks[0].roots[0].children[0].pr.number, 18);

        let rendered = format_stacks(&stacks);
        assert!(rendered.contains("#17"));
        assert!(rendered.contains("#18"));
        assert!(rendered.contains("[Pending]"));
        assert!(rendered.contains("[Approved]"));
    }

    #[test]
    fn groups_branching_dag_stack() {
        let prs = vec![
            make_pr(1, "Base layer", "main", "branch-1", None),
            make_pr(2, "Child A", "branch-1", "branch-2", None),
            make_pr(3, "Child B", "branch-1", "branch-3", None),
        ];

        let stacks = build_stacks(prs, "main");
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].roots[0].children.len(), 2);
        assert_eq!(stacks[0].roots[0].children[0].pr.number, 2);
        assert_eq!(stacks[0].roots[0].children[1].pr.number, 3);
    }

    #[test]
    fn groups_multiple_independent_stacks() {
        let prs = vec![
            make_pr(1, "Feature A", "main", "users/me/a", None),
            make_pr(2, "Feature B", "develop", "users/me/b", None),
        ];

        let stacks = build_stacks(prs, "main");
        assert_eq!(stacks.len(), 2);
        assert_eq!(stacks[0].base, "main");
        assert_eq!(stacks[1].base, "develop");
    }

    #[test]
    fn handles_cycle_without_infinite_recursion() {
        let prs = vec![
            make_pr(1, "A", "branch-b", "branch-a", None),
            make_pr(2, "B", "branch-a", "branch-b", None),
        ];

        // Should terminate cleanly
        let stacks = build_stacks(prs, "main");
        let rendered = format_stacks(&stacks);
        assert!(!rendered.is_empty());
    }
}
