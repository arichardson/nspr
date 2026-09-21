//! The forge abstraction.
//!
//! All GitHub interaction goes through [`Forge`]. The real implementation talks
//! to the API; [`fake::FakeForge`] keeps everything in a local repository so the
//! stack engine can be tested end-to-end without a network.
//!
//! Keeping this seam is what makes the correctness claims in this crate
//! testable at all — spr, which calls `octocrab` directly from its command
//! functions, cannot be tested this way.

pub mod fake;
pub mod github;

use async_trait::async_trait;
use color_eyre::eyre::Result;
use git2::Oid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// GitHub's mergeability verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

/// GitHub's `mergeStateStatus`, insofar as we care about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeState {
    Clean,
    /// The head branch is behind its base.
    Behind,
    Blocked,
    Dirty,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub state: PrState,
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub base_oid: Oid,
    pub head_oid: Oid,
    pub mergeable: Mergeable,
    pub merge_state: MergeState,
    /// Auto-merge would merge this PR into its *base branch*, not the trunk —
    /// silently flattening a stack. Surfaced as a guardrail warning.
    pub auto_merge: bool,
    pub draft: bool,
}

impl PullRequest {
    pub fn is_open(&self) -> bool {
        self.state == PrState::Open
    }

    /// Whether the displayed diff is stale enough to be worth refreshing even
    /// if the patch itself has not changed.
    ///
    /// `Conflicting` always warrants a refresh. `Behind` deliberately does
    /// **not**, unless the repository requires branches to be up to date:
    /// in a stack, every layer above an amended one is "behind" by
    /// construction, and its displayed diff is still correct (the merge base
    /// stays pinned to where it forked). Refreshing on `Behind` unconditionally
    /// would push every layer on every amend, re-running CI and discarding
    /// approvals for no benefit.
    pub fn needs_refresh(&self, require_up_to_date: bool) -> bool {
        self.mergeable == Mergeable::Conflicting
            || (require_up_to_date && self.merge_state == MergeState::Behind)
    }
}

#[derive(Debug, Clone)]
pub struct CreatePr {
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PullRequestUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub state: Option<PrState>,
}

impl PullRequestUpdate {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.base.is_none()
            && self.state.is_none()
    }
}

/// A squash merge request.
///
/// `title` and `message` are **always** set explicitly: a PR branch's history
/// is full of `[nspr]` commits, so a repository configured with
/// `squash_merge_commit_message = COMMIT_MESSAGES` would otherwise paste that
/// noise onto the trunk. `expected_head` is a compare-and-swap guard.
#[derive(Debug, Clone)]
pub struct SquashMerge {
    pub title: String,
    pub message: String,
    pub expected_head: Oid,
}

#[derive(Debug, Clone)]
pub struct PushSpec {
    pub branch: String,
    /// `None` deletes the branch.
    pub oid: Option<Oid>,
    /// Only ever true for land-time cleanup, which is diff-neutral.
    pub force: bool,
}

impl PushSpec {
    pub fn fast_forward(branch: impl Into<String>, oid: Oid) -> Self {
        Self {
            branch: branch.into(),
            oid: Some(oid),
            force: false,
        }
    }

    pub fn forced(branch: impl Into<String>, oid: Oid) -> Self {
        Self {
            branch: branch.into(),
            oid: Some(oid),
            force: true,
        }
    }

    pub fn delete(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            oid: None,
            force: false,
        }
    }
}

/// Branch protection settings that affect how we should behave.
#[derive(Debug, Clone, Default)]
pub struct Protection {
    /// If set, every push to a layer dismisses that layer's approvals.
    pub dismiss_stale_reviews: bool,
    /// If set, stacks serialise badly and every layer re-runs CI.
    pub require_up_to_date: bool,
}

/// A comment on a pull request.
#[derive(Debug, Clone)]
pub struct Comment {
    pub id: u64,
    pub body: String,
}

#[async_trait(?Send)]
pub trait Forge {
    async fn get_pull_request(&self, number: u64) -> Result<PullRequest>;
    async fn create_pull_request(&self, req: CreatePr) -> Result<u64>;
    async fn update_pull_request(
        &self,
        number: u64,
        update: PullRequestUpdate,
    ) -> Result<()>;
    /// Squash-merge, returning the resulting commit on the trunk.
    async fn merge_pull_request(
        &self,
        number: u64,
        req: SquashMerge,
    ) -> Result<Oid>;
    async fn branch_protection(
        &self,
        branch: &str,
    ) -> Result<Option<Protection>>;
    /// Current tip of a remote branch, or `None` if it does not exist.
    async fn branch_oid(&self, branch: &str) -> Result<Option<Oid>>;
    async fn push(&self, specs: &[PushSpec]) -> Result<()>;
    /// Find an unused branch name starting from `preferred`.
    async fn unused_branch_name(&self, preferred: &str) -> Result<String>;
    /// Make `oid` available in the local object database.
    ///
    /// `land` needs the squash commit locally in order to replay the remaining
    /// layers onto it. Forges that share the local repository override this
    /// with a no-op.
    async fn fetch_commit(&self, oid: Oid) -> Result<()>;
    /// Comments nspr itself posted on a pull request.
    ///
    /// Only the authenticated user's own comments are of interest, since the
    /// stack comment is identified by a marker inside a comment we wrote.
    async fn list_own_comments(&self, number: u64) -> Result<Vec<Comment>>;
    async fn create_comment(&self, number: u64, body: &str) -> Result<u64>;
    async fn update_comment(&self, id: u64, body: &str) -> Result<()>;
    /// Open pull requests in the repository, optionally restricted to an author.
    async fn list_pull_requests(
        &self,
        author: Option<&str>,
    ) -> Result<Vec<ListedPr>>;
    /// Look up a pull request whose head branch matches `head`.
    async fn find_pull_request_by_head(
        &self,
        head: &str,
    ) -> Result<Option<PullRequest>>;
    /// Create or update native GitHub Stack objects for each bottom-to-top
    /// chain of pull request numbers (`chain.len() >= 2`).
    async fn sync_stacks(&self, chains: &[Vec<u64>]) -> Result<()> {
        let _ = chains;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPr {
    pub number: u64,
    pub title: String,
    pub state: PrState,
    pub draft: bool,
    pub base: String,
    pub head: String,
    pub review_decision: Option<ReviewDecision>,
    pub url: String,
}
