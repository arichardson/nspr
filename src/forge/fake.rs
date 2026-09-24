//! An in-memory forge for tests.
//!
//! Remote branches live as refs in the same repository, so tests can inspect
//! the resulting commit graph directly and ask
//! [`crate::review_diff`] what GitHub would display for it.
//!
//! The fake enforces the property the real GitHub would: a non-forced push must
//! be a fast-forward. A bug that would have required a force-push therefore
//! fails a test rather than silently stranding review comments in production.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use color_eyre::eyre::{Result, bail};
use git2::{Oid, Repository};

use super::{
    Comment, CreatePr, Forge, ListedPr, MergeState, Mergeable, PrState,
    Protection, PullRequest, PullRequestUpdate, PushSpec, RepoMergeSettings,
    ReviewDecision, SquashMerge,
};

#[derive(Debug, Clone)]
struct FakePr {
    number: u64,
    state: PrState,
    title: String,
    body: String,
    base: String,
    head: String,
    auto_merge: bool,
    draft: bool,
}

pub struct FakeForge {
    repo: Rc<Repository>,
    trunk: String,
    branches: RefCell<HashMap<String, Oid>>,
    prs: RefCell<Vec<FakePr>>,
    next_number: RefCell<u64>,
    /// Every push the engine attempted, in order. Tests assert on this.
    pub pushes: RefCell<Vec<PushSpec>>,
    /// Applies to every branch that has no entry in `protections`.
    pub protection: RefCell<Option<Protection>>,
    /// Per-branch overrides. In a real repository the trunk and a user's topic
    /// branches are almost never protected the same way, and that difference
    /// decides whether a push dismisses approvals.
    pub protections: RefCell<HashMap<String, Protection>>,
    /// `(pr number, comment)`, in creation order.
    comments: RefCell<Vec<(u64, Comment)>>,
    next_comment_id: RefCell<u64>,
    /// Counts `update_comment` calls. Rewriting a comment re-notifies everyone
    /// subscribed to the pull request, so a test asserts this stays flat when
    /// nothing about the stack actually changed.
    pub comment_updates: RefCell<u64>,
    pub review_decisions: RefCell<HashMap<u64, ReviewDecision>>,
    pub stacks: RefCell<Vec<Vec<u64>>>,
    pub merge_settings: RefCell<RepoMergeSettings>,
    /// `(pr number, displayed file list)` after every remote mutation.
    pub diff_observations: RefCell<Vec<(u64, Vec<String>)>>,
    /// `(pr number, draft)` in call order.
    pub draft_toggles: RefCell<Vec<(u64, bool)>>,
}

impl FakeForge {
    pub fn new(repo: Rc<Repository>, trunk: &str, trunk_oid: Oid) -> Self {
        let mut branches = HashMap::new();
        branches.insert(trunk.to_string(), trunk_oid);
        Self {
            repo,
            trunk: trunk.to_string(),
            branches: RefCell::new(branches),
            prs: RefCell::new(Vec::new()),
            next_number: RefCell::new(101),
            pushes: RefCell::new(Vec::new()),
            protection: RefCell::new(None),
            protections: RefCell::new(HashMap::new()),
            comments: RefCell::new(Vec::new()),
            next_comment_id: RefCell::new(1),
            comment_updates: RefCell::new(0),
            review_decisions: RefCell::new(HashMap::new()),
            stacks: RefCell::new(Vec::new()),
            merge_settings: RefCell::new(RepoMergeSettings::default()),
            diff_observations: RefCell::new(Vec::new()),
            draft_toggles: RefCell::new(Vec::new()),
        }
    }

    pub fn set_review_decision(&self, number: u64, decision: ReviewDecision) {
        self.review_decisions.borrow_mut().insert(number, decision);
    }

    pub fn trunk_oid(&self) -> Oid {
        self.branches.borrow()[&self.trunk]
    }

    pub fn branch(&self, name: &str) -> Option<Oid> {
        self.branches.borrow().get(name).copied()
    }

    pub fn branch_exists(&self, name: &str) -> bool {
        self.branches.borrow().contains_key(name)
    }

    /// Pushes that were forced. Should only ever happen during `land`.
    pub fn forced_pushes(&self) -> Vec<PushSpec> {
        self.pushes
            .borrow()
            .iter()
            .filter(|p| p.force)
            .cloned()
            .collect()
    }

    pub fn set_auto_merge(&self, number: u64, on: bool) {
        if let Some(pr) = self
            .prs
            .borrow_mut()
            .iter_mut()
            .find(|p| p.number == number)
        {
            pr.auto_merge = on;
        }
    }

    /// Simulate someone editing the title and description in the GitHub UI.
    pub fn edit_in_ui(&self, number: u64, title: &str, body: &str) {
        if let Some(pr) = self
            .prs
            .borrow_mut()
            .iter_mut()
            .find(|p| p.number == number)
        {
            pr.title = title.to_string();
            pr.body = body.to_string();
        }
    }

    /// Simulate someone squash-merging through the GitHub UI.
    pub fn external_squash_merge(&self, number: u64) -> Result<Oid> {
        self.do_squash_merge(number, None)
    }

    fn find(&self, number: u64) -> Result<FakePr> {
        self.prs
            .borrow()
            .iter()
            .find(|p| p.number == number)
            .cloned()
            .ok_or_else(|| color_eyre::eyre::eyre!("no such PR #{number}"))
    }

    fn do_squash_merge(
        &self,
        number: u64,
        req: Option<&SquashMerge>,
    ) -> Result<Oid> {
        let pr = self.find(number)?;
        if pr.state != PrState::Open {
            bail!("PR #{number} is not open");
        }
        let head_oid = self.branch(&pr.head).ok_or_else(|| {
            color_eyre::eyre::eyre!("head branch {} is gone", pr.head)
        })?;
        if let Some(req) = req
            && req.expected_head != head_oid
        {
            bail!(
                "PR #{number} head moved (expected {}, found {head_oid}); \
                 refusing to merge",
                req.expected_head
            );
        }

        let base_oid = self.branch(&pr.base).ok_or_else(|| {
            color_eyre::eyre::eyre!("base branch {} is gone", pr.base)
        })?;

        // Squash: a brand new single-parent commit on the base, carrying the
        // merged tree. This is what destroys SHA identity and breaks naive
        // stacking.
        let mb = self.repo.merge_base(base_oid, head_oid)?;
        let mut index = self.repo.merge_trees(
            &self.repo.find_commit(mb)?.tree()?,
            &self.repo.find_commit(base_oid)?.tree()?,
            &self.repo.find_commit(head_oid)?.tree()?,
            None,
        )?;
        if index.has_conflicts() {
            bail!("PR #{number} does not merge cleanly");
        }
        let tree_oid = index.write_tree_to(&self.repo)?;
        let tree = self.repo.find_tree(tree_oid)?;
        let sig = self.repo.signature()?;

        // With no explicit title and message, GitHub falls back to the
        // repository's squash-message setting. Model the worst case —
        // `COMMIT_MESSAGES`, which concatenates the branch's commit messages —
        // so tests notice if `nspr` ever stops passing them explicitly.
        let message = match req {
            Some(req) => format!("{}\n\n{}", req.title, req.message)
                .trim_end()
                .to_string(),
            None => self.branch_commit_messages(head_oid, base_oid)?,
        };

        let squash = self.repo.commit(
            None,
            &sig,
            &sig,
            &message,
            &tree,
            &[&self.repo.find_commit(base_oid)?],
        )?;

        self.branches.borrow_mut().insert(pr.base.clone(), squash);
        if let Some(p) = self
            .prs
            .borrow_mut()
            .iter_mut()
            .find(|p| p.number == number)
        {
            p.state = PrState::Merged;
        }
        Ok(squash)
    }

    /// Concatenated messages of the head branch's own commits, as GitHub's
    /// `COMMIT_MESSAGES` squash setting would produce.
    fn branch_commit_messages(&self, head: Oid, base: Oid) -> Result<String> {
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
        walk.push(head)?;
        walk.hide(base)?;
        let mut out = String::new();
        for oid in walk {
            let commit = self.repo.find_commit(oid?)?;
            out.push_str(&String::from_utf8_lossy(commit.message_bytes()));
            out.push_str("\n\n");
        }
        Ok(out.trim_end().to_string())
    }

    /// Record what every open pull request displays *right now*.
    ///
    /// Called after each individual remote mutation, not just at the end of a
    /// run. A stacked pull request whose diff is briefly wrong is not a
    /// cosmetic problem: GitHub assigns `CODEOWNERS` from whatever the diff
    /// happens to contain at that instant, and those assignments are not undone
    /// when the diff is corrected a second later.
    fn observe_displayed_diffs(&self) {
        for pr in self.prs.borrow().iter() {
            if pr.state != PrState::Open {
                continue;
            }
            let (Some(base), Some(head)) =
                (self.branch(&pr.base), self.branch(&pr.head))
            else {
                continue;
            };
            let Ok(paths) =
                crate::review_diff::displayed_paths(&self.repo, base, head)
            else {
                continue;
            };
            self.diff_observations.borrow_mut().push((pr.number, paths));
        }
    }

    /// Forget everything observed so far, so a test can scope its assertions to
    /// a single command.
    pub fn clear_diff_observations(&self) {
        self.diff_observations.borrow_mut().clear();
    }

    /// Every distinct file list `number` was ever seen displaying, in order.
    pub fn observed_diffs(&self, number: u64) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        for (pr, paths) in self.diff_observations.borrow().iter() {
            if *pr == number && out.last() != Some(paths) {
                out.push(paths.clone());
            }
        }
        out
    }
}

#[async_trait(?Send)]
impl Forge for FakeForge {
    async fn get_pull_request(&self, number: u64) -> Result<PullRequest> {
        let pr = self.find(number)?;
        let base_oid = self.branch(&pr.base).unwrap_or(Oid::ZERO_SHA1);
        let head_oid = self.branch(&pr.head).unwrap_or(Oid::ZERO_SHA1);

        // Derive the merge state from the graph, as GitHub would. Note that
        // GitHub only surfaces BEHIND when the repository requires branches to
        // be up to date; otherwise a stacked pull request whose base branch has
        // advanced still reports CLEAN, because its displayed diff is fine.
        let requires_up_to_date = self
            .protection
            .borrow()
            .as_ref()
            .is_some_and(|p| p.require_up_to_date);
        let merge_state = if base_oid.is_zero() || head_oid.is_zero() {
            MergeState::Unknown
        } else {
            let mb = self.repo.merge_base(base_oid, head_oid)?;
            if mb == base_oid || !requires_up_to_date {
                MergeState::Clean
            } else {
                MergeState::Behind
            }
        };

        Ok(PullRequest {
            number: pr.number,
            node_id: format!("PR_{}", pr.number),
            state: pr.state,
            title: pr.title,
            body: pr.body,
            base: pr.base,
            head: pr.head,
            base_oid,
            head_oid,
            mergeable: Mergeable::Mergeable,
            merge_state,
            auto_merge: pr.auto_merge,
            draft: pr.draft,
        })
    }

    async fn create_pull_request(&self, req: CreatePr) -> Result<u64> {
        let mut n = self.next_number.borrow_mut();
        let number = *n;
        *n += 1;
        self.prs.borrow_mut().push(FakePr {
            number,
            state: PrState::Open,
            title: req.title,
            body: req.body,
            base: req.base,
            head: req.head,
            auto_merge: false,
            draft: req.draft,
        });
        Ok(number)
    }

    async fn update_pull_request(
        &self,
        number: u64,
        update: PullRequestUpdate,
    ) -> Result<()> {
        let base_changed = update.base.is_some();
        {
            let mut prs = self.prs.borrow_mut();
            let pr = prs.iter_mut().find(|p| p.number == number).ok_or_else(
                || color_eyre::eyre::eyre!("no such PR #{number}"),
            )?;
            if let Some(t) = update.title {
                pr.title = t;
            }
            if let Some(b) = update.body {
                pr.body = b;
            }
            if let Some(b) = update.base {
                pr.base = b;
            }
            if let Some(s) = update.state {
                pr.state = s;
            }
        }
        if base_changed {
            self.observe_displayed_diffs();
        }
        Ok(())
    }

    async fn merge_pull_request(
        &self,
        number: u64,
        req: SquashMerge,
    ) -> Result<Oid> {
        self.do_squash_merge(number, Some(&req))
    }

    async fn branch_protection(
        &self,
        branch: &str,
    ) -> Result<Option<Protection>> {
        if let Some(p) = self.protections.borrow().get(branch) {
            return Ok(Some(p.clone()));
        }
        Ok(self.protection.borrow().clone())
    }

    async fn branch_oid(&self, branch: &str) -> Result<Option<Oid>> {
        Ok(self.branch(branch))
    }

    async fn push(&self, specs: &[PushSpec]) -> Result<()> {
        for spec in specs {
            self.pushes.borrow_mut().push(spec.clone());
            match spec.oid {
                None => {
                    self.branches.borrow_mut().remove(&spec.branch);
                }
                Some(new) => {
                    let existing = self.branch(&spec.branch);
                    if let Some(old) = existing
                        && !spec.force
                        && old != new
                        && !self.repo.graph_descendant_of(new, old)?
                    {
                        bail!(
                            "non-fast-forward push to {}: {} is not a \
                             descendant of {}. nspr must never need a force \
                             push outside of `land`.",
                            spec.branch,
                            new,
                            old
                        );
                    }
                    self.branches.borrow_mut().insert(spec.branch.clone(), new);
                }
            }
        }
        // GitHub automatically closes an open pull request as `Merged` the
        // instant a `git push` makes its `head` SHA an ancestor of its `base`
        // branch tip (which happens on a naive stack reorder if the new upper
        // branch is pushed before the new lower PR's `base` is retargeted).
        for pr in self.prs.borrow_mut().iter_mut() {
            if pr.state != PrState::Open {
                continue;
            }
            let (Some(base_oid), Some(head_oid)) =
                (self.branch(&pr.base), self.branch(&pr.head))
            else {
                continue;
            };
            if head_oid == base_oid
                || self.repo.graph_descendant_of(base_oid, head_oid)?
            {
                pr.state = PrState::Merged;
            }
        }
        self.observe_displayed_diffs();
        Ok(())
    }

    async fn unused_branch_name(&self, preferred: &str) -> Result<String> {
        let branches = self.branches.borrow();
        if !branches.contains_key(preferred) {
            return Ok(preferred.to_string());
        }
        for i in 1.. {
            let candidate = format!("{preferred}-{i}");
            if !branches.contains_key(&candidate) {
                return Ok(candidate);
            }
        }
        unreachable!()
    }

    /// The fake's "remote" branches are refs in the local repository, so every
    /// object is already present.
    async fn fetch_commit(&self, oid: Oid) -> Result<()> {
        self.repo.find_commit(oid)?;
        Ok(())
    }

    /// Every comment in the fake was written by the one user it models, so
    /// there is nothing to filter.
    async fn list_own_comments(&self, number: u64) -> Result<Vec<Comment>> {
        Ok(self
            .comments
            .borrow()
            .iter()
            .filter(|(pr, _)| *pr == number)
            .map(|(_, c)| c.clone())
            .collect())
    }

    async fn create_comment(&self, number: u64, body: &str) -> Result<u64> {
        let mut next = self.next_comment_id.borrow_mut();
        let id = *next;
        *next += 1;
        self.comments.borrow_mut().push((
            number,
            Comment {
                id,
                body: body.to_string(),
            },
        ));
        Ok(id)
    }

    async fn update_comment(&self, id: u64, body: &str) -> Result<()> {
        let mut comments = self.comments.borrow_mut();
        let Some((_, comment)) = comments.iter_mut().find(|(_, c)| c.id == id)
        else {
            bail!("no such comment: {id}");
        };
        comment.body = body.to_string();
        *self.comment_updates.borrow_mut() += 1;
        Ok(())
    }

    async fn delete_comment(&self, id: u64) -> Result<()> {
        let mut comments = self.comments.borrow_mut();
        let before = comments.len();
        comments.retain(|(_, c)| c.id != id);
        if comments.len() == before {
            bail!("no such comment: {id}");
        }
        Ok(())
    }

    async fn set_draft(&self, node_id: &str, draft: bool) -> Result<()> {
        let number: u64 = node_id
            .strip_prefix("PR_")
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                color_eyre::eyre::eyre!("bad fake node id: {node_id}")
            })?;
        let mut prs = self.prs.borrow_mut();
        let Some(pr) = prs.iter_mut().find(|p| p.number == number) else {
            bail!("no such PR #{number}");
        };
        pr.draft = draft;
        drop(prs);
        self.draft_toggles.borrow_mut().push((number, draft));
        Ok(())
    }

    async fn list_pull_requests(
        &self,
        _author: Option<&str>,
    ) -> Result<Vec<ListedPr>> {
        let prs = self.prs.borrow();
        let decisions = self.review_decisions.borrow();
        Ok(prs
            .iter()
            .filter(|pr| pr.state == PrState::Open)
            .map(|pr| ListedPr {
                number: pr.number,
                title: pr.title.clone(),
                state: pr.state,
                draft: pr.draft,
                base: pr.base.clone(),
                head: pr.head.clone(),
                review_decision: decisions.get(&pr.number).copied(),
                url: format!("https://github.com/fake/repo/pull/{}", pr.number),
            })
            .collect())
    }

    async fn find_pull_request_by_head(
        &self,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let prs = self.prs.borrow();
        let number = prs
            .iter()
            .find(|pr| pr.head == head && pr.state == PrState::Open)
            .or_else(|| prs.iter().find(|pr| pr.head == head))
            .map(|pr| pr.number);
        drop(prs);
        match number {
            Some(n) => self.get_pull_request(n).await.map(Some),
            None => Ok(None),
        }
    }

    async fn sync_stacks(&self, chains: &[Vec<u64>]) -> Result<()> {
        *self.stacks.borrow_mut() = chains.to_vec();
        Ok(())
    }

    async fn repo_merge_settings(&self) -> Result<RepoMergeSettings> {
        Ok(*self.merge_settings.borrow())
    }
}
