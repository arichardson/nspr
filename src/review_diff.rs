//! What GitHub actually displays for a pull request.
//!
//! GitHub's "Files changed" is a *three-dot* diff, recomputed live from the
//! current branch tips:
//!
//! ```text
//! diff(tree(merge_base(base_tip, head_tip)), tree(head_tip))
//! ```
//!
//! That is a pure function of the commit graph, so we can reproduce it exactly
//! — which is what lets the test suite verify "the pull request shows the right
//! diff" without any network access, and lets `land` assert that its one
//! force-push is diff-neutral.

use color_eyre::eyre::Result;
use git2::{Oid, Repository};

/// The merge base GitHub would use for this pull request.
pub fn review_base(
    repo: &Repository,
    base_tip: Oid,
    head_tip: Oid,
) -> Result<Oid> {
    Ok(repo.merge_base(base_tip, head_tip)?)
}

/// Patch id of what GitHub would display. Used by the push gate.
pub fn displayed_patch_id(
    repo: &Repository,
    base_tip: Oid,
    head_tip: Oid,
) -> Result<Oid> {
    let mb = review_base(repo, base_tip, head_tip)?;
    crate::patch_id::tree_patch_id(
        repo,
        repo.find_commit(mb)?.tree_id(),
        repo.find_commit(head_tip)?.tree_id(),
    )
}

/// Rendered text of what GitHub would display. Used by tests and `status`.
pub fn displayed_patch(
    repo: &Repository,
    base_tip: Oid,
    head_tip: Oid,
) -> Result<String> {
    let mb = review_base(repo, base_tip, head_tip)?;
    render_tree_diff(
        repo,
        repo.find_commit(mb)?.tree_id(),
        repo.find_commit(head_tip)?.tree_id(),
    )
}

/// Render the diff between two trees as unified patch text.
pub fn render_tree_diff(
    repo: &Repository,
    from: Oid,
    to: Oid,
) -> Result<String> {
    let from = repo.find_tree(from)?;
    let to = repo.find_tree(to)?;
    let diff = repo.diff_tree_to_tree(Some(&from), Some(&to), None)?;

    let mut out = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        match line.origin() {
            '+' | '-' | ' ' => out.push(line.origin()),
            _ => {}
        }
        out.push_str(&String::from_utf8_lossy(line.content()));
        true
    })?;
    Ok(out)
}

/// The patch a local commit introduces, as GitHub would render it if the layer
/// were displayed correctly.
pub fn local_commit_patch(repo: &Repository, commit: Oid) -> Result<String> {
    let commit = repo.find_commit(commit)?;
    let parent = commit.parent(0)?;
    render_tree_diff(repo, parent.tree_id(), commit.tree_id())
}
