//! The sync engine: bring every pull request in the stack in line with the
//! local commits.
//!
//! Layers are processed bottom-up so each one can be told the resulting tip of
//! the layer it depends on.
//!
//! # The push gate
//!
//! A layer is only pushed when it needs to be. The test is not "did the tree
//! change" — amending a commit low in a stack changes *every* descendant's
//! tree — but "did the **patch GitHub displays** change", which is exactly what
//! [`crate::patch_id`] answers. Skipping untouched layers preserves their CI
//! results and, under `dismiss_stale_reviews`, their approvals.

use color_eyre::eyre::{Result, bail};
use git2::Oid;

use crate::config::Config;
use crate::forge::{
    CreatePr, Forge, PrState, PullRequest, PullRequestUpdate, PushSpec,
};
use crate::git::Git;
use crate::patch_id::tree_patch_id;
use crate::review_diff::displayed_patch_id;
use crate::stack::{Dep, Stack, Trees};
use crate::trailers::{CommitMessage, DEPENDS_ON, PULL_REQUEST};

/// Default message for a push that does not change the displayed patch.
pub const AUTO_UPDATE_MESSAGE: &str = "[nspr] update";

#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Push every layer, even ones whose patch is unchanged.
    pub sync_all: bool,
    /// Use this message instead of prompting.
    pub message: Option<String>,
    /// Overwrite the pull request title/body from the local commit message.
    pub update_message: bool,
    /// Open new pull requests as drafts.
    pub draft: bool,
    /// Set when the repository requires branches to be up to date before
    /// merging. Only then is a "behind" layer worth refreshing; see
    /// [`crate::forge::PullRequest::needs_refresh`].
    pub refresh_when_behind: bool,
    /// Only sync this specific layer index (used by `nspr diff --cherry-pick`).
    pub only_layer: Option<usize>,
    /// Whether to push incremental `[nspr]` commits (`true`) or rewrite each
    /// PR branch as a single commit with force-pushes (`false`).
    pub preserve_commit_history: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            sync_all: false,
            message: None,
            update_message: false,
            draft: false,
            refresh_when_behind: false,
            only_layer: None,
            preserve_commit_history: true,
        }
    }
}

/// Supplies the "what changed?" message shown on update commits.
pub trait Prompter {
    fn update_message(&self, subject: &str) -> Result<String>;
}

/// Never prompts; always returns the same message. Used by tests and by
/// non-interactive invocations.
pub struct FixedPrompter(pub String);

impl Prompter for FixedPrompter {
    fn update_message(&self, _subject: &str) -> Result<String> {
        Ok(self.0.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerAction {
    Created,
    /// Pushed because the displayed patch changed.
    Updated,
    /// Pushed only to refresh a stale or conflicted view.
    Refreshed,
    /// Nothing to do.
    Skipped,
}

#[derive(Debug, Clone)]
pub struct LayerOutcome {
    pub index: usize,
    pub number: u64,
    pub branch: String,
    pub tip: Oid,
    pub action: LayerAction,
    /// The base branch the pull request now targets.
    pub base: String,
    /// True if we had to retarget the pull request's base.
    pub retargeted: bool,
}

/// Bring every layer's pull request in line with the local stack.
///
/// Runs in four passes, which is not incidental complexity: deciding *which*
/// layers to push has to happen before any push, because the decision is made
/// against the remote state as the reviewer currently sees it.
pub async fn sync_stack(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &mut Stack,
    opts: &SyncOptions,
    prompter: &dyn Prompter,
) -> Result<Vec<LayerOutcome>> {
    resolve_external_deps(forge, stack).await?;

    let merge_settings = forge.repo_merge_settings().await?;
    let mut opts = opts.clone();
    opts.preserve_commit_history =
        config.preserve_commit_history.resolve(merge_settings);

    let trees = stack.all_trees(git)?;

    let prs = gather(forge, stack).await?;
    reject_unusable(&prs)?;
    crate::upgrade::reject_if_legacy_spr_with_prs(
        git,
        config,
        stack,
        &prs,
        opts.only_layer,
    )?;
    let decision = decide(git, stack, &prs, &trees, &opts)?;

    let mut staged = false;
    let outcomes = execute(
        git,
        forge,
        config,
        stack,
        &prs,
        &trees,
        &decision,
        &opts,
        prompter,
        &mut staged,
    )
    .await?;

    // A staged layer is parked at `merge_base(old_base, new_base)` during the
    // first pass so the retarget never widens the displayed diff. Once every
    // pull request points at its new base branch, immediately move parked
    // branches forward onto their new base tips in the same `nspr diff` run so
    // the user never has to run `nspr diff` twice.
    if staged {
        let mut catchup_opts = opts.clone();
        catchup_opts.refresh_when_behind = true;
        // Never re-prompt on the catch-up pass; the patch is already up to date.
        catchup_opts.message = Some(AUTO_UPDATE_MESSAGE.to_string());
        let trees = stack.all_trees(git)?;
        let prs = gather(forge, stack).await?;
        reject_unusable(&prs)?;
        let decision = decide(git, stack, &prs, &trees, &catchup_opts)?;
        let mut staged_again = false;
        let second = execute(
            git,
            forge,
            config,
            stack,
            &prs,
            &trees,
            &decision,
            &catchup_opts,
            prompter,
            &mut staged_again,
        )
        .await?;
        let mut merged = outcomes;
        for next in second {
            if let Some(prev) =
                merged.iter_mut().find(|o| o.index == next.index)
            {
                let action = match (prev.action, next.action) {
                    (LayerAction::Created, _) => LayerAction::Created,
                    (LayerAction::Updated, _) | (_, LayerAction::Updated) => {
                        LayerAction::Updated
                    }
                    (LayerAction::Refreshed, _)
                    | (_, LayerAction::Refreshed) => LayerAction::Refreshed,
                    _ => next.action,
                };
                let retargeted = prev.retargeted || next.retargeted;
                *prev = LayerOutcome {
                    action,
                    retargeted,
                    ..next
                };
            } else {
                merged.push(next);
            }
        }
        return Ok(merged);
    }

    Ok(outcomes)
}

/// Pass A: fetch the current state of every existing pull request.
///
/// Public because `nspr status` answers "what would `diff` do?" by running the
/// same passes and stopping short of [`execute`]. Anything else would be a
/// reimplementation that could quietly drift out of step with the real gate.
pub async fn gather(
    forge: &dyn Forge,
    stack: &Stack,
) -> Result<Vec<Option<PullRequest>>> {
    let mut prs = Vec::with_capacity(stack.layers.len());
    for layer in &stack.layers {
        match layer.pr {
            None => prs.push(None),
            Some(number) => {
                let pr = forge.get_pull_request(number).await?;
                forge.fetch_commit(pr.head_oid).await?;
                if pr.base_oid != Oid::ZERO_SHA1 {
                    forge.fetch_commit(pr.base_oid).await?;
                }
                prs.push(Some(pr));
            }
        }
    }
    Ok(prs)
}

/// Refuse to push to a pull request that is no longer accepting work.
///
/// Separate from [`gather`] because `status` wants to *show* you a closed or
/// merged pull request rather than fail on it.
pub fn reject_unusable(prs: &[Option<PullRequest>]) -> Result<()> {
    for pr in prs.iter().flatten() {
        let number = pr.number;
        match pr.state {
            PrState::Closed => bail!(
                "pull request #{number} is closed. Remove its \
                 `{PULL_REQUEST}:` trailer to open a new one."
            ),
            // Detected by state, never by sha: a squash merge creates a commit
            // with a brand new id, so the local commit is nowhere in the
            // trunk's history even though its content is.
            PrState::Merged => bail!(
                "pull request #{number} has already been merged. Run `nspr \
                 sync` to drop it from the stack, or remove its \
                 `{PULL_REQUEST}:` trailer to submit what is left as a new \
                 pull request."
            ),
            PrState::Open => {}
        }
    }
    Ok(())
}

/// Which layers need pushing, and why.
pub struct Decision {
    /// The layer's own patch differs from what the reviewer currently sees.
    pub patch_changed: Vec<bool>,
    /// The layer's local commit message differs from the initial commit on its
    /// PR branch.
    pub message_changed: Vec<bool>,
    /// The layer's branch history must be rewritten (force-pushed) because its
    /// commit message changed or its base branch was rewritten.
    pub rewrite_history: Vec<bool>,
    /// The layer will be pushed. A layer can need a push without its patch
    /// having changed: see pass C.
    pub push: Vec<bool>,
}

/// Passes B and C: decide which layers to push.
pub fn decide(
    git: &Git,
    stack: &Stack,
    prs: &[Option<PullRequest>],
    trees: &Trees,
    opts: &SyncOptions,
) -> Result<Decision> {
    let n = stack.layers.len();
    let mut patch_changed = vec![false; n];
    let mut message_changed = vec![false; n];
    let mut rewrite_history = vec![false; n];
    let mut push = vec![false; n];

    let mut current_anchor: Vec<Option<Oid>> = vec![None; n];

    // Pass B: compare what each pull request displays *right now* against what
    // it ought to display. Both sides are computed from local state on the
    // right and remote state on the left, which is the whole point: the target
    // is `dep_tree(i)`, the dependency's local effective tree, never the
    // dependency's remote tip, which may be several amends out of date.
    for i in 0..n {
        if let Some(only) = opts.only_layer
            && i != only
        {
            continue;
        }
        let Some(pr) = &prs[i] else {
            push[i] = true; // Nothing exists yet; it must be created.
            continue;
        };

        let remote_base_tip = match stack.layers[i].dep {
            Dep::Main | Dep::ExternalPr(_) => Some(stack.base),
            Dep::Layer(j) => prs[j].as_ref().map(|p| p.head_oid),
        };

        let mut needs_conflict_refresh = false;
        patch_changed[i] = match remote_base_tip {
            // The dependency has no pull request yet, so there is nothing
            // meaningful to compare against.
            None => true,
            Some(base_tip) => {
                let first_oid = find_root_commit(git, pr, base_tip);
                current_anchor[i] =
                    first_oid.and_then(|r| git.parent_of(r).ok());
                if let Some(first_oid) = first_oid {
                    let current_msg = git.message_of(first_oid)?;
                    let desired_msg =
                        stack.layers[i].message.clean_for_branch();
                    if current_msg.trim() != desired_msg.trim() {
                        message_changed[i] = true;
                        rewrite_history[i] = true;
                    }
                    if !opts.preserve_commit_history && first_oid != pr.head_oid
                    {
                        rewrite_history[i] = true;
                    }
                }

                let shown =
                    displayed_patch_id(git.repo(), base_tip, pr.head_oid)?;
                let desired = tree_patch_id(
                    git.repo(),
                    trees.dep[i],
                    trees.effective[i],
                )?;
                needs_conflict_refresh = would_conflict_or_diverge_on_forge(
                    git,
                    base_tip,
                    pr.head_oid,
                    trees.dep[i],
                    trees.effective[i],
                )?;
                shown != desired
            }
        };

        let behind_base = opts.refresh_when_behind
            && remote_base_tip.is_some_and(|base_tip| {
                current_anchor[i] != Some(base_tip)
                    || !git.is_ancestor(base_tip, pr.head_oid).unwrap_or(false)
            });

        push[i] = patch_changed[i]
            || message_changed[i]
            || rewrite_history[i]
            || needs_conflict_refresh
            || opts.sync_all
            || pr.needs_refresh(opts.refresh_when_behind)
            || behind_base;
    }

    // Pass C: a push is only safe once its dependency's remote tip already
    // carries the dependency's effective tree.
    //
    // Pushing layer `i` requires its base branch tip `tip_dep` to carry
    // `effective(dep)`, so GitHub will display `diff(tree(tip_dep), effective(i))`.
    // If `tip_dep` is stale, that diff would wrongly include the dependency's
    // un-pushed changes. Restacking the dependency is the fix, and it cascades:
    // reverse order suffices because forward references are rejected at
    // discovery, so `j < i` always.
    for i in (0..n).rev() {
        if !push[i] {
            continue;
        }
        let Dep::Layer(j) = stack.layers[i].dep else {
            continue;
        };
        let up_to_date = match &prs[j] {
            None => false,
            Some(pr) => git.tree_of(pr.head_oid)? == trees.effective[j],
        };
        if !up_to_date {
            push[j] = true;
        }
    }

    // Keep every pull request branch 1-parent linear (zero merge commits) so
    // GitHub's native "Merge full stack" and "Rebase stack" buttons work:
    // - If a layer's base has not moved (`!base_moved`) and its commit message
    //   is unchanged, we append a 1-parent fast-forward commit (`rewrite_history = false`),
    //   leaving untouched upper layers alone (`0` pushes to upper layers).
    // - Whenever a layer `i` is pushed and its target base tip differs from its
    //   current branch root's parent (`base_moved`), we replay `i`'s 1-parent
    //   revision chain onto the new base tip (`rewrite_history[i] = true`), and
    //   cascade that re-anchoring to any open dependent layers above `i`.
    // - When `!opts.preserve_commit_history`, any pushed layer rewrites its
    //   branch as a single clean commit (`rewrite_history[i] = true`).
    for i in 0..n {
        if let Some(only) = opts.only_layer
            && i != only
        {
            continue;
        }
        if prs[i].is_none() {
            continue;
        }
        if let Dep::Layer(j) = stack.layers[i].dep
            && rewrite_history[j]
        {
            rewrite_history[i] = true;
            push[i] = true;
        }
        let base_moved = match stack.layers[i].dep {
            Dep::Main | Dep::ExternalPr(_) => {
                current_anchor[i] != Some(stack.base)
            }
            Dep::Layer(j) => {
                push[j]
                    || prs[j].as_ref().map(|p| p.head_oid) != current_anchor[i]
            }
        };
        if push[i] && (!opts.preserve_commit_history || base_moved) {
            rewrite_history[i] = true;
        }
    }

    Ok(Decision {
        patch_changed,
        message_changed,
        rewrite_history,
        push,
    })
}

fn find_root_commit(
    git: &Git,
    pr: &PullRequest,
    fallback_base_tip: Oid,
) -> Option<Oid> {
    if let Some(root_oid) = crate::refs::get_root(git, pr.number)
        && git.is_ancestor(root_oid, pr.head_oid).unwrap_or(false)
    {
        return Some(root_oid);
    }
    crate::land::branch_revisions(git, pr.head_oid, fallback_base_tip)
        .ok()
        .and_then(|r| r.first().copied())
}

/// Check if updating the base branch to `new_base_tree` without pushing this PR
/// would cause GitHub's 3-way merge check to report a merge conflict or merge
/// to a tree different from `desired_effective_tree`.
fn would_conflict_or_diverge_on_forge(
    git: &Git,
    old_base_tip: Oid,
    old_head_tip: Oid,
    new_base_tree: Oid,
    desired_effective_tree: Oid,
) -> Result<bool> {
    let old_base_tree = git.tree_of(old_base_tip)?;
    if old_base_tree == new_base_tree {
        return Ok(false);
    }
    let merge_base_oid = git.merge_base(old_base_tip, old_head_tip)?;
    let merge_base_tree = git.tree_of(merge_base_oid)?;
    let old_head_tree = git.tree_of(old_head_tip)?;
    let index =
        git.merge_trees(merge_base_tree, new_base_tree, old_head_tree)?;
    if index.has_conflicts() {
        return Ok(true);
    }
    let merged_tree = git.write_index(index)?;
    Ok(merged_tree != desired_effective_tree)
}

/// Where a layer's branch must sit while its pull request is retargeted.
///
/// GitHub shows a pull request's diff as `tree(head) - tree(merge_base(base,
/// head))`, and it hands that file list to `CODEOWNERS` on **every** push, not
/// only at the end of a run. So re-anchoring a head onto a base branch the pull
/// request has not been pointed at yet is not a harmless intermediate state: if
/// the new anchor carries commits the old base lacks, the pull request briefly
/// displays all of them and GitHub subscribes their owners for good.
///
/// Anchoring at the merge base of the old and new base branches avoids that
/// entirely, because it is an ancestor of both: the displayed diff is the
/// layer's own patch before the base change and after it. The branch is left
/// slightly behind its new base, which is the ordinary state of any stack whose
/// trunk has moved on, and is repaired by the next push that has a reason to
/// happen.
///
/// Returns `None` when no staging is needed.
fn safe_retarget_anchor(
    git: &Git,
    pr: &PullRequest,
    new_base_branch: &str,
    old_base_tip: Oid,
    new_base_tip: Oid,
) -> Result<Option<Oid>> {
    if pr.base == new_base_branch {
        return Ok(None);
    }
    // Nothing the new base has is missing from the old one, so the diff cannot
    // grow no matter which order the two operations happen in.
    if git.is_ancestor(new_base_tip, old_base_tip)? {
        return Ok(None);
    }
    let anchor = git.merge_base(old_base_tip, new_base_tip)?;
    Ok((anchor != new_base_tip).then_some(anchor))
}

/// Express a layer's content on top of `onto` instead of its usual parent.
///
/// `desired` is the layer's tree as computed against the base it is *about* to
/// have; `ancestor_tree` is the tree that base change is relative to. Replaying
/// the difference between them onto `onto` gives the same patch sitting on the
/// older anchor.
///
/// Returns `None` if the three-way merge conflicts, which is the caller's cue
/// to give up on staging rather than to fail.
fn rebase_tree_onto(
    git: &Git,
    ancestor_tree: Oid,
    onto: Oid,
    desired: Oid,
) -> Result<Option<Oid>> {
    let ours = git.tree_of(onto)?;
    if ours == ancestor_tree {
        return Ok(Some(desired));
    }
    let index = git.merge_trees(ancestor_tree, ours, desired)?;
    if index.has_conflicts() {
        return Ok(None);
    }
    Ok(Some(git.write_index(index)?))
}

/// Pass D: push bottom-up, so every layer sees its dependency's *new* tip.
#[allow(clippy::too_many_arguments)]
async fn execute(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &mut Stack,
    prs: &[Option<PullRequest>],
    trees: &Trees,
    decision: &Decision,
    opts: &SyncOptions,
    prompter: &dyn Prompter,
    // Set when a layer was left behind its new base by `safe_retarget_anchor`.
    staged: &mut bool,
) -> Result<Vec<LayerOutcome>> {
    let n = stack.layers.len();
    let mut outcomes: Vec<LayerOutcome> = Vec::new();
    let mut tips: Vec<Oid> = Vec::with_capacity(n);
    let mut branches: Vec<String> = Vec::with_capacity(n);
    let mut base_branches: Vec<String> = Vec::with_capacity(n);
    let mut new_roots: Vec<Option<Oid>> = vec![None; n];
    let mut retargeted_early: Vec<bool> = vec![false; n];
    let mut undraft: Vec<String> = Vec::new();

    let mut messages: Vec<CommitMessage> =
        stack.layers.iter().map(|l| l.message.clone()).collect();

    let mut reserved_branches: std::collections::HashSet<String> =
        prs.iter().flatten().map(|p| p.head.clone()).collect();
    let mut push_specs: Vec<PushSpec> = Vec::new();

    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        let (parent_tip, base_branch) = match stack.layers[i].dep {
            Dep::Main | Dep::ExternalPr(_) => {
                (stack.base, config.trunk.clone())
            }
            Dep::Layer(j) => (tips[j], branches[j].clone()),
        };
        base_branches.push(base_branch);

        let desired_tree = trees.effective[i];
        let layer_commit = stack.layers[i].commit;
        let subject = stack.layers[i].subject().to_string();

        match &prs[i] {
            None if !decision.push[i] => {
                tips.push(stack.base);
                branches.push(String::new());
            }
            None => {
                let preferred = config.branch_name_for(&subject);
                let mut candidate =
                    forge.unused_branch_name(&preferred).await?;
                let mut suffix = 1usize;
                while reserved_branches.contains(&candidate) {
                    candidate = forge
                        .unused_branch_name(&format!("{preferred}-{suffix}"))
                        .await?;
                    suffix += 1;
                }
                reserved_branches.insert(candidate.clone());

                // The dependency may be parked behind its own base, in which
                // case the tree computed against the eventual base does not
                // belong on top of it.
                let desired_tree = rebase_tree_onto(
                    git,
                    trees.dep[i],
                    parent_tip,
                    desired_tree,
                )?
                .unwrap_or(desired_tree);

                let initial_msg = stack.layers[i].message.clean_for_branch();
                let tip = git.synthesize_initial_commit(
                    parent_tip,
                    desired_tree,
                    layer_commit,
                    &initial_msg,
                )?;
                new_roots[i] = Some(tip);
                push_specs.push(PushSpec::fast_forward(&candidate, tip));
                tips.push(tip);
                branches.push(candidate);
            }
            Some(pr) => {
                if !decision.push[i] {
                    tips.push(pr.head_oid);
                    branches.push(pr.head.clone());
                    continue;
                }

                // Where the branch's own revision chain starts (for replay).
                let fallback_base_tip = match stack.layers[i].dep {
                    Dep::Main | Dep::ExternalPr(_) => stack.base,
                    Dep::Layer(j) => prs[j]
                        .as_ref()
                        .map(|p| p.head_oid)
                        .unwrap_or(stack.base),
                };
                let old_root_parent =
                    match find_root_commit(git, pr, fallback_base_tip) {
                        Some(root_oid) => git.parent_of(root_oid)?,
                        None => fallback_base_tip,
                    };

                // What GitHub's three-dot diff is comparing against right now:
                // if we already retargeted the PR before the push, its base is
                // `base_branches[i]`; otherwise its base is still `pr.base` at
                // `pr.base_oid`.
                let current_pr_base_tip = if pr.base_oid != Oid::ZERO_SHA1 {
                    pr.base_oid
                } else {
                    old_root_parent
                };

                // If the pull request is being retargeted to a base branch
                // whose *current* remote tip sits between the PR's current
                // base tip and the PR's current head (`current_pr_base_tip <=
                // target_remote_tip <= pr.head_oid` — for example, re-attaching
                // a PR from `main` onto a parent PR branch that it was
                // originally stacked on), we can retarget the base *before*
                // `git push`. That only advances `merge_base(base, head)` from
                // `current_pr_base_tip` up to `target_remote_tip`, which
                // shrinks (or preserves) the displayed diff immediately and
                // makes `pr.base` already match `base_branches[i]` when
                // `git push` runs, so no parking or second push is needed.
                let new_base_current_remote_tip = match stack.layers[i].dep {
                    Dep::Main | Dep::ExternalPr(_) => Some(stack.base),
                    Dep::Layer(j) => prs[j].as_ref().map(|p| p.head_oid),
                };
                let retargeted_before_push = if pr.base != base_branches[i]
                    && let Some(target_remote_tip) = new_base_current_remote_tip
                    && git.is_ancestor(target_remote_tip, pr.head_oid)?
                    && (git
                        .is_ancestor(current_pr_base_tip, target_remote_tip)?
                        || git
                            .is_ancestor(old_root_parent, target_remote_tip)?)
                {
                    if config.draft_while_retargeting && !pr.draft {
                        forge.set_draft(&pr.node_id, true).await?;
                        undraft.push(pr.node_id.clone());
                    }
                    forge
                        .update_pull_request(
                            pr.number,
                            PullRequestUpdate {
                                base: Some(base_branches[i].clone()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    true
                } else {
                    false
                };
                retargeted_early[i] = retargeted_before_push;

                // Re-anchoring and retargeting cannot be done atomically, so
                // when they disagree the branch is parked at their merge base
                // until the pull request points somewhere that makes the
                // forward move safe. If `pr.base` is another branch in this
                // stack that is also being rewritten in the same `git push`
                // batch, the anchor must be an ancestor of both `pr.base`'s old
                // tip and its newly pushed tip so the push does not widen this
                // PR's diff before `update_pull_request` changes `pr.base`.
                let effective_old_base_tip = if let Some((_, pushed_tip)) =
                    branches.iter().zip(&tips).find(|(b, _)| *b == &pr.base)
                {
                    git.merge_base(current_pr_base_tip, *pushed_tip)?
                } else {
                    current_pr_base_tip
                };
                let anchor = if retargeted_before_push {
                    parent_tip
                } else {
                    safe_retarget_anchor(
                        git,
                        pr,
                        &base_branches[i],
                        effective_old_base_tip,
                        parent_tip,
                    )?
                    .unwrap_or(parent_tip)
                };

                // The layer's content is computed against the base it is about
                // to have, so it has to be replayed whenever the anchor is
                // something else. That covers this layer being parked and,
                // because `parent_tip` is already the parked tip in that case,
                // a layer stacked on a parked dependency as well: without the
                // replay it would carry content its own base does not have.
                //
                // Replaying can conflict where the forward move would not.
                // Nothing is lost by declining, it just means taking the
                // ordinary path.
                let (anchor, desired_tree) = match rebase_tree_onto(
                    git,
                    trees.dep[i],
                    anchor,
                    desired_tree,
                )? {
                    Some(tree) => (anchor, tree),
                    None => (parent_tip, desired_tree),
                };
                if anchor != parent_tip {
                    *staged = true;
                }
                // A parked branch is being moved off its current history, so it
                // is rewritten whether or not anything else asked for that.
                let rewrite = decision.rewrite_history[i]
                    || anchor != parent_tip
                    || desired_tree != trees.effective[i];

                if !opts.preserve_commit_history {
                    let clean_msg = stack.layers[i].message.clean_for_branch();
                    let tip = git.synthesize_initial_commit(
                        anchor,
                        desired_tree,
                        layer_commit,
                        &clean_msg,
                    )?;
                    new_roots[i] = Some(tip);
                    push_specs.push(PushSpec::forced(&pr.head, tip));
                    tips.push(tip);
                    branches.push(pr.head.clone());
                    continue;
                }

                let mut tip = if rewrite {
                    let revisions = crate::land::branch_revisions(
                        git,
                        pr.head_oid,
                        old_root_parent,
                    )?;
                    let clean_msg = stack.layers[i].message.clean_for_branch();
                    match crate::land::replay(
                        git, &revisions, anchor, &clean_msg,
                    )? {
                        Some(t) => t,
                        None => git.synthesize_initial_commit(
                            anchor,
                            desired_tree,
                            layer_commit,
                            &clean_msg,
                        )?,
                    }
                } else {
                    pr.head_oid
                };

                if !rewrite || git.tree_of(tip)? != desired_tree {
                    // Prompt only when reviewers will actually see something
                    // different.
                    let message =
                        match (&opts.message, decision.patch_changed[i]) {
                            (Some(m), _) => m.clone(),
                            (None, true) => {
                                prompter.update_message(&subject)?
                            }
                            (None, false) => AUTO_UPDATE_MESSAGE.to_string(),
                        };
                    tip = git.synthesize_update_commit(
                        tip,
                        desired_tree,
                        layer_commit,
                        &message,
                    )?;
                }

                if rewrite {
                    new_roots[i] =
                        crate::land::branch_revisions(git, tip, anchor)?
                            .first()
                            .copied();
                    push_specs.push(PushSpec::forced(&pr.head, tip));
                } else {
                    push_specs.push(PushSpec::fast_forward(&pr.head, tip));
                }

                tips.push(tip);
                branches.push(pr.head.clone());
            }
        }
    }

    // Drafts are exempt from `CODEOWNERS` auto-assignment, so flipping a pull
    // request to draft across a retarget is a second line of defence for
    // anybody who wants one. Off unless asked for: parking the branch already
    // keeps the displayed diff correct, marking a pull request ready again
    // re-runs the assignment anyway, and a draft left behind by an interrupted
    // run is its own kind of mess.
    if config.draft_while_retargeting {
        for i in 0..n {
            let Some(pr) = &prs[i] else { continue };
            if pr.draft || pr.base == base_branches[i] || retargeted_early[i] {
                continue;
            }
            forge.set_draft(&pr.node_id, true).await?;
            undraft.push(pr.node_id.clone());
        }
    }

    // Phase 2: push all new and updated branches in a single git push.
    if !push_specs.is_empty() {
        forge.push(&push_specs).await?;
    }

    // Phase 3: create/update pull requests on the forge and record local refs.
    let warn_merge_strategy = opts.preserve_commit_history
        && !forge.repo_merge_settings().await?.is_squash_only();
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        let base_branch = base_branches[i].clone();
        let tip = tips[i];
        let branch = branches[i].clone();
        let subject = stack.layers[i].subject().to_string();

        let outcome = match &prs[i] {
            None if !decision.push[i] => continue,
            None => {
                let body = crate::pr_body::splice_warning(
                    &stack.layers[i].message.body,
                    warn_merge_strategy,
                );
                let number = forge
                    .create_pull_request(CreatePr {
                        title: subject,
                        body,
                        base: base_branch.clone(),
                        head: branch.clone(),
                        draft: opts.draft,
                    })
                    .await?;

                messages[i].set(PULL_REQUEST, &config.pull_request_url(number));
                stack.layers[i].pr = Some(number);
                crate::refs::update_root(git, number, tip)?;

                LayerOutcome {
                    index: i,
                    number,
                    branch,
                    tip,
                    action: LayerAction::Created,
                    base: base_branch,
                    retargeted: false,
                }
            }
            Some(pr) => {
                let mut action = if !decision.push[i] {
                    LayerAction::Skipped
                } else if decision.patch_changed[i]
                    || decision.message_changed[i]
                {
                    LayerAction::Updated
                } else {
                    LayerAction::Refreshed
                };

                if let Some(root_commit) = new_roots[i] {
                    crate::refs::update_root(git, pr.number, root_commit)?;
                }

                let retargeted = pr.base != base_branch;
                let mut update = PullRequestUpdate::default();
                if retargeted && !retargeted_early[i] {
                    update.base = Some(base_branch.clone());
                }
                if opts.update_message || decision.message_changed[i] {
                    if pr.title != subject {
                        update.title = Some(subject);
                    }
                    let body = crate::pr_body::splice_warning(
                        &stack.layers[i].message.body,
                        warn_merge_strategy,
                    );
                    if pr.body != body {
                        update.body = Some(body);
                    }
                } else {
                    let body = crate::pr_body::splice_warning(
                        &pr.body,
                        warn_merge_strategy,
                    );
                    if pr.body != body {
                        update.body = Some(body);
                    }
                }
                if !update.is_empty() {
                    if action == LayerAction::Skipped {
                        action = LayerAction::Updated;
                    }
                    forge.update_pull_request(pr.number, update).await?;
                }

                LayerOutcome {
                    index: i,
                    number: pr.number,
                    branch,
                    tip,
                    action,
                    base: base_branch,
                    retargeted,
                }
            }
        };

        if let Some(value) = canonical_dep(config, stack, i)
            && messages[i].get(DEPENDS_ON) != Some(value.as_str())
        {
            messages[i].set(DEPENDS_ON, &value);
        }

        crate::refs::update(git, outcome.number, outcome.tip)?;
        outcomes.push(outcome);
    }

    // Every base now points where it should, so it is safe to be looked at
    // again.
    for node_id in undraft {
        forge.set_draft(&node_id, false).await?;
    }

    apply_message_edits(git, stack, &messages)?;
    forge.sync_stacks(&stack.pr_chains()).await?;
    Ok(outcomes)
}

/// The canonical `Depends-On:` value for a layer, if it declared one.
///
/// Layers with no trailer are left alone: the implicit "previous layer" rule
/// is the common case and spelling it out on every commit would be noise.
fn canonical_dep(config: &Config, stack: &Stack, i: usize) -> Option<String> {
    stack.layers[i].dep_spec.as_ref()?;
    match stack.layers[i].dep {
        Dep::Main => Some(config.trunk.clone()),
        Dep::Layer(j) => stack.layers[j].pr.map(|n| format!("#{n}")),
        // Still unresolved, so we have nothing better to write.
        Dep::ExternalPr(_) => None,
    }
}

/// Resolve `Depends-On:` references to pull requests outside the local stack.
///
/// A **merged** reference becomes [`Dep::Main`]. Anything else is an error: we
/// must never fall back to "previous layer", which would silently re-stack the
/// commit onto a sibling the author explicitly disclaimed.
async fn resolve_external_deps(
    forge: &dyn Forge,
    stack: &mut Stack,
) -> Result<()> {
    for i in 0..stack.layers.len() {
        let Dep::ExternalPr(number) = stack.layers[i].dep else {
            continue;
        };
        let pr = forge.get_pull_request(number).await?;
        match pr.state {
            PrState::Merged => stack.layers[i].dep = Dep::Main,
            _ => bail!(
                "`{}` declares `{DEPENDS_ON}: #{number}`, but that pull \
                 request is neither in this stack nor merged.\nEither include \
                 its commit in the stack or point the trailer elsewhere.",
                stack.layers[i].subject(),
            ),
        }
    }
    Ok(())
}

/// Write the updated commit messages back to the local history and bring the
/// in-memory stack in line with the resulting commit ids.
///
/// `rewrite_messages` leaves commits whose message is unchanged alone (and
/// everything below the first change), so this is a no-op in the common case
/// where nothing needed recording.
pub(crate) fn apply_message_edits(
    git: &Git,
    stack: &mut Stack,
    messages: &[CommitMessage],
) -> Result<()> {
    let unchanged = stack
        .layers
        .iter()
        .zip(messages)
        .all(|(layer, message)| layer.message.render() == message.render());
    if unchanged {
        return Ok(());
    }

    let pairs: Vec<(Oid, String)> = stack
        .layers
        .iter()
        .zip(messages)
        .map(|(layer, message)| (layer.commit, message.render()))
        .collect();

    let new_oids = git.rewrite_messages(stack.base, &pairs)?;
    for ((layer, oid), message) in
        stack.layers.iter_mut().zip(new_oids).zip(messages)
    {
        layer.commit = oid;
        layer.message = message.clone();
    }
    // Parent links shift along with the rewrite.
    let mut parent = stack.base;
    for layer in stack.layers.iter_mut() {
        layer.parent = parent;
        parent = layer.commit;
    }
    Ok(())
}
