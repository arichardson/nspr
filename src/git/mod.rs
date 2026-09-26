/*
 * Portions of this file are derived from spr (https://github.com/spacedentist/spr)
 * Copyright (c) Radical HQ Limited, MIT licensed.
 * Modified for nspr: commit-message handling now uses git trailers, and the
 * synthesis primitives live in the `synthesis` submodule.
 */

//! Thin wrapper over `git2` providing the operations `nspr` needs.

pub mod synthesis;

use std::sync::Arc;

use color_eyre::eyre::{Error, Result, WrapErr as _, bail, eyre};
use git2::Oid;

#[derive(Clone)]
pub struct Git {
    repo: Arc<git2::Repository>,
    hooks: Arc<git2_ext::hooks::Hooks>,
}

impl Git {
    pub fn new(repo: git2::Repository) -> Self {
        Self {
            hooks: Arc::new(
                git2_ext::hooks::Hooks::with_repo(&repo)
                    .unwrap_or_else(|_| git2_ext::hooks::Hooks::new("")),
            ),
            #[allow(clippy::arc_with_non_send_sync)]
            repo: Arc::new(repo),
        }
    }

    pub fn repo(&self) -> &Arc<git2::Repository> {
        &self.repo
    }

    fn hooks(&self) -> &git2_ext::hooks::Hooks {
        self.hooks.as_ref()
    }

    pub fn head(&self) -> Result<Oid> {
        self.repo
            .head()?
            .resolve()?
            .target()
            .ok_or_else(|| eyre!("cannot resolve HEAD"))
    }

    pub fn resolve_reference(&self, reference: &str) -> Result<Oid> {
        Ok(self.repo.find_reference(reference)?.peel_to_commit()?.id())
    }

    /// Point `reference` at `oid`, creating it if it does not exist.
    pub fn set_reference(
        &self,
        reference: &str,
        oid: Oid,
        reason: &str,
    ) -> Result<()> {
        self.repo.reference(reference, oid, true, reason)?;
        Ok(())
    }

    /// Commits reachable from `HEAD` but not from `base`, oldest first.
    pub fn commits_since(&self, base: Oid) -> Result<Vec<Oid>> {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL.union(git2::Sort::REVERSE))?;
        walk.push_head()?;
        walk.hide(base)?;
        Ok(walk.collect::<std::result::Result<Vec<Oid>, _>>()?)
    }

    pub fn tree_of(&self, oid: Oid) -> Result<Oid> {
        Ok(self.repo.find_commit(oid)?.tree_id())
    }

    pub fn parent_of(&self, oid: Oid) -> Result<Oid> {
        let commit = self.repo.find_commit(oid)?;
        if commit.parent_count() != 1 {
            bail!(
                "commit {} has {} parents; nspr expects a linear local stack",
                oid,
                commit.parent_count()
            );
        }
        Ok(commit.parent_id(0)?)
    }

    pub fn message_of(&self, oid: Oid) -> Result<String> {
        let commit = self.repo.find_commit(oid)?;
        Ok(String::from_utf8_lossy(commit.message_bytes()).into_owned())
    }

    pub fn short_id(&self, oid: Oid) -> Result<String> {
        let commit = self.repo.find_commit(oid)?;
        Ok(commit
            .as_object()
            .short_id()?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        if ancestor == descendant {
            return Ok(true);
        }
        Ok(self.repo.graph_descendant_of(descendant, ancestor)?)
    }

    pub fn merge_base(&self, a: Oid, b: Oid) -> Result<Oid> {
        Ok(self.repo.merge_base(a, b)?)
    }

    /// Cherry-pick `oid`'s patch onto `onto`, returning the resulting index.
    pub fn cherrypick(&self, oid: Oid, onto: Oid) -> Result<git2::Index> {
        let commit = self.repo.find_commit(oid)?;
        let onto_commit = self.repo.find_commit(onto)?;
        Ok(self
            .repo
            .cherrypick_commit(&commit, &onto_commit, 0, None)?)
    }

    pub fn write_index(&self, mut index: git2::Index) -> Result<Oid> {
        Ok(index.write_tree_to(self.repo.as_ref())?)
    }

    /// Three-way merge of trees, used to reconstruct a layer's effective tree
    /// when it declares a non-default dependency.
    pub fn merge_trees(
        &self,
        base: Oid,
        ours: Oid,
        theirs: Oid,
    ) -> Result<git2::Index> {
        let base = self.repo.find_tree(base)?;
        let ours = self.repo.find_tree(ours)?;
        let theirs = self.repo.find_tree(theirs)?;
        Ok(self.repo.merge_trees(&base, &ours, &theirs, None)?)
    }

    /// Create a commit with an explicitly chosen tree and parents.
    ///
    /// Author identity is taken from `attribution_commit` so the pull request
    /// is attributed correctly, but timestamps are set to now so the commit
    /// lands in the right place in GitHub's timeline.
    pub fn create_derived_commit(
        &self,
        attribution_commit: Oid,
        message: &str,
        tree_oid: Oid,
        parent_oids: &[Oid],
    ) -> Result<Oid> {
        let original = self.repo.find_commit(attribution_commit)?;
        let tree = self.repo.find_tree(tree_oid)?;
        let parents = parent_oids
            .iter()
            .map(|oid| self.repo.find_commit(*oid))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let parent_refs = parents.iter().collect::<Vec<_>>();
        let message = git2::message_prettify(message, None)?;

        let committer = self.repo.signature().or_else(|_| {
            git2::Signature::now(
                String::from_utf8_lossy(original.committer().name_bytes())
                    .as_ref(),
                String::from_utf8_lossy(original.committer().email_bytes())
                    .as_ref(),
            )
        })?;
        let author = git2::Signature::now(
            String::from_utf8_lossy(original.author().name_bytes()).as_ref(),
            String::from_utf8_lossy(original.author().email_bytes()).as_ref(),
        )?;

        Ok(self.repo.commit(
            None,
            &author,
            &committer,
            &message,
            &tree,
            &parent_refs,
        )?)
    }

    /// Rewrite the messages of a linear chain of commits starting at `base`.
    ///
    /// `commits` pairs each existing commit oid with its desired message.
    /// Returns the new oids. Commits whose message is unchanged are only
    /// rewritten if an earlier commit in the chain was (since their parent
    /// moved). `HEAD` is updated to the new tip.
    pub fn rewrite_messages(
        &self,
        base: Oid,
        commits: &[(Oid, String)],
    ) -> Result<Vec<Oid>> {
        if commits.is_empty() {
            return Ok(Vec::new());
        }

        let mut parent = base;
        let mut updating = false;
        let mut new_oids = Vec::with_capacity(commits.len());

        for (oid, desired) in commits {
            let commit = self.repo.find_commit(*oid)?;
            let current =
                String::from_utf8_lossy(commit.message_bytes()).into_owned();
            let desired = git2::message_prettify(desired, None)?;

            if current != desired {
                updating = true;
            }

            if updating {
                let new_oid = self.repo.commit(
                    None,
                    &commit.author(),
                    &commit.committer(),
                    &desired,
                    &commit.tree()?,
                    &[&self.repo.find_commit(parent)?],
                )?;
                self.hooks().run_post_rewrite_rebase(
                    self.repo.as_ref(),
                    &[(*oid, new_oid)],
                );
                new_oids.push(new_oid);
                parent = new_oid;
            } else {
                new_oids.push(*oid);
                parent = *oid;
            }
        }

        if updating {
            self.repo
                .find_reference("HEAD")?
                .resolve()?
                .set_target(parent, "nspr rewrote commit messages")?;
        }

        Ok(new_oids)
    }

    /// Rebase a linear chain of commits onto `new_parent`, updating `HEAD`.
    ///
    /// Commits that become empty (typically because they just landed) are
    /// dropped.
    pub fn rebase_commits(
        &self,
        commits: &[Oid],
        mut new_parent: Oid,
    ) -> Result<Vec<Oid>> {
        let mut result = Vec::new();
        let hooks = self.hooks();

        for oid in commits {
            let new_parent_commit = self.repo.find_commit(new_parent)?;
            let commit = self.repo.find_commit(*oid)?;

            let mut index = self.repo.cherrypick_commit(
                &commit,
                &new_parent_commit,
                0,
                None,
            )?;
            if index.has_conflicts() {
                bail!("rebase failed due to merge conflicts");
            }

            let tree_oid = index.write_tree_to(self.repo.as_ref())?;
            if tree_oid == new_parent_commit.tree_id() {
                // Became empty: almost always because it just landed.
                hooks.run_post_rewrite_rebase(
                    self.repo.as_ref(),
                    &[(*oid, new_parent)],
                );
                continue;
            }

            let tree = self.repo.find_tree(tree_oid)?;
            new_parent = self.repo.commit(
                None,
                &commit.author(),
                &commit.committer(),
                String::from_utf8_lossy(commit.message_bytes()).as_ref(),
                &tree,
                &[&new_parent_commit],
            )?;
            hooks.run_post_rewrite_rebase(
                self.repo.as_ref(),
                &[(*oid, new_parent)],
            );
            result.push(new_parent);
        }

        let new_commit = self.repo.find_commit(new_parent)?;
        let mut reference = self.repo.head()?.resolve()?;
        self.repo
            .checkout_tree(new_commit.as_object(), None)
            .map_err(Error::from)
            .wrap_err("could not check out rebased branch; rebase manually")?;
        reference.set_target(new_parent, "nspr rebased")?;

        Ok(result)
    }

    pub fn check_no_uncommitted_changes(&self) -> Result<()> {
        let mut opts = git2::StatusOptions::new();
        opts.include_ignored(false).include_untracked(false);
        if self.repo.statuses(Some(&mut opts))?.is_empty() {
            Ok(())
        } else {
            Err(eyre!("there are uncommitted changes; stash or commit them"))
        }
    }

    /// Check out a local branch and update the working tree and index.
    pub fn checkout_branch(&self, branch_name: &str) -> Result<()> {
        let head_ref = format!("refs/heads/{branch_name}");
        let commit = self.repo.find_reference(&head_ref)?.peel_to_commit()?;
        self.repo.checkout_tree(commit.as_object(), None)?;
        self.repo.set_head(&head_ref)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn opens_and_writes_refs_in_reftable_repository() {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args([
                "init",
                "--ref-format=reftable",
                "--initial-branch=main",
            ])
            .current_dir(dir.path())
            .status()
            .expect("failed to run git init");
        assert!(status.success(), "git init --ref-format=reftable failed");

        let repo = git2::Repository::open(dir.path())
            .expect("libgit2 should open reftable repository");
        let git = Git::new(repo);

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = git.repo().treebuilder(None).unwrap().write().unwrap();
        let tree = git.repo().find_tree(tree_id).unwrap();
        let c1 = git
            .repo()
            .commit(Some("HEAD"), &sig, &sig, "Initial commit\n", &tree, &[])
            .unwrap();
        let c1_commit = git.repo().find_commit(c1).unwrap();
        let c2 = git
            .repo()
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "Second commit\n\nPull-Request: #1\n",
                &tree,
                &[&c1_commit],
            )
            .unwrap();

        assert_eq!(git.head().unwrap(), c2);
        assert_eq!(git.resolve_reference("refs/heads/main").unwrap(), c2);
        git.set_reference("refs/nspr/pr-1/head", c2, "test reftable write")
            .unwrap();
        assert_eq!(git.resolve_reference("refs/nspr/pr-1/head").unwrap(), c2);
        assert_eq!(git.commits_since(c1).unwrap(), vec![c2]);
    }
}

