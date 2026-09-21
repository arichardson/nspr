//! Patch identity, used to decide whether a pull request actually needs a push.
//!
//! A pull request's *displayed* diff is `diff(tree(merge_base), tree(head))`.
//! Amending a commit low in a stack shifts every descendant's tree, but leaves
//! their **patches** unchanged — so comparing trees would push every layer,
//! needlessly re-running CI and (under `dismiss_stale_reviews`) discarding
//! every approval above the change.
//!
//! `git patch-id` gives a stable hash of a diff that ignores line numbers and
//! surrounding context, which is exactly the comparison we want.

use color_eyre::eyre::Result;
use git2::{Oid, Repository};

/// Stable hash of the patch between two trees.
pub fn tree_patch_id(repo: &Repository, from: Oid, to: Oid) -> Result<Oid> {
    let from = repo.find_tree(from)?;
    let to = repo.find_tree(to)?;
    let diff = repo.diff_tree_to_tree(Some(&from), Some(&to), None)?;
    Ok(diff.patchid(None)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestRepo;

    /// The property the push gate depends on: shifting both endpoints of a
    /// diff by the same unrelated change leaves the patch id alone.
    #[test]
    fn patch_id_is_stable_when_an_unrelated_lower_layer_changes() {
        let t = TestRepo::new();
        let repo = t.open();

        // Layer 2 adds b.txt on top of layer 1's a.txt.
        let before_base = t.tree_with(&[("a.txt", "v1")]);
        let before_head = t.tree_with(&[("a.txt", "v1"), ("b.txt", "new")]);

        // Layer 1 is amended: a.txt changes. Layer 2 still just adds b.txt.
        let after_base = t.tree_with(&[("a.txt", "v2")]);
        let after_head = t.tree_with(&[("a.txt", "v2"), ("b.txt", "new")]);

        assert_eq!(
            tree_patch_id(&repo, before_base, before_head).unwrap(),
            tree_patch_id(&repo, after_base, after_head).unwrap(),
            "amending a lower layer must not invalidate an upper layer's patch"
        );
    }

    #[test]
    fn patch_id_changes_when_the_patch_changes() {
        let t = TestRepo::new();
        let repo = t.open();

        let base = t.tree_with(&[("a.txt", "v1")]);
        let head1 = t.tree_with(&[("a.txt", "v1"), ("b.txt", "one")]);
        let head2 = t.tree_with(&[("a.txt", "v1"), ("b.txt", "two")]);

        assert_ne!(
            tree_patch_id(&repo, base, head1).unwrap(),
            tree_patch_id(&repo, base, head2).unwrap()
        );
    }

    #[test]
    fn identical_trees_have_a_stable_empty_patch_id() {
        let t = TestRepo::new();
        let repo = t.open();
        let tree = t.tree_with(&[("a.txt", "v1")]);
        assert_eq!(
            tree_patch_id(&repo, tree, tree).unwrap(),
            tree_patch_id(&repo, tree, tree).unwrap()
        );
    }
}
