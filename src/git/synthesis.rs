//! Commit synthesis: the single place PR-branch commits are constructed.
//!
//! Every commit `nspr` pushes to a pull request branch is built here, keeping
//! all PR branch commits strictly 1-parent linear so GitHub's native "Merge
//! stack" and "Rebase stack" Web UI features work while retaining revision
//! history for reviewers.

use color_eyre::eyre::{Result, eyre};
use git2::Oid;

use super::Git;

impl Git {
    /// Synthesize the next 1-parent commit on a pull request branch.
    ///
    /// `current_remote_tip` is the sole parent, making the resulting push a
    /// strict fast-forward.
    pub fn synthesize_update_commit(
        &self,
        current_remote_tip: Oid,
        target_tree: Oid,
        attribution_commit: Oid,
        message: &str,
    ) -> Result<Oid> {
        let oid = self.create_derived_commit(
            attribution_commit,
            message,
            target_tree,
            &[current_remote_tip],
        )?;

        if !self.repo().graph_descendant_of(oid, current_remote_tip)? {
            return Err(eyre!(
                "internal error: synthesized commit {oid} is not a descendant \
                 of {current_remote_tip}; pushing it would require a force"
            ));
        }

        Ok(oid)
    }

    /// Synthesize the *first* commit of a new pull request branch.
    ///
    /// There is no previous tip to extend, so the branch simply starts at
    /// `base_tip` with the given tree.
    pub fn synthesize_initial_commit(
        &self,
        base_tip: Oid,
        target_tree: Oid,
        attribution_commit: Oid,
        message: &str,
    ) -> Result<Oid> {
        self.create_derived_commit(
            attribution_commit,
            message,
            target_tree,
            &[base_tip],
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::git::Git;
    use crate::testutil::TestRepo;

    #[test]
    fn update_commit_is_linear_fast_forward() {
        let t = TestRepo::new();
        let base = t.commit_file("main", "a.txt", "1", &[]);
        let git = Git::new(t.open());

        let head_tree = t.tree_with(&[("a.txt", "1"), ("b.txt", "2")]);
        let tip1 = git
            .synthesize_initial_commit(base, head_tree, base, "v1")
            .unwrap();

        let new_head_tree =
            t.tree_with(&[("a.txt", "1"), ("b.txt", "2-updated")]);
        let tip2 = git
            .synthesize_update_commit(tip1, new_head_tree, base, "v2")
            .unwrap();

        let repo = git.repo();
        assert!(repo.graph_descendant_of(tip2, tip1).unwrap());
        assert_eq!(repo.find_commit(tip2).unwrap().parent_count(), 1);
        assert_eq!(repo.merge_base(base, tip2).unwrap(), base);
    }
}
