//! Landing a layer: squash-merge it, then repair everything that depended on
//! it.
//!
//! # Why this is more than "merge and delete the branch"
//!
//! A squash merge produces a **brand new commit** `S` on the trunk. Its SHA
//! appears nowhere in a dependent branch's ancestry, so the moment the trunk
//! moves to `S` the merge base of a dependent pull request collapses back to
//! the old trunk — and GitHub starts displaying the landed layer's changes as
//! part of the dependent's diff.
//!
//! Two other traps are worth spelling out, because the obvious implementations
//! hit both:
//!
//! * **Delete the head branch last.** GitHub auto-retargets dependent pull
//!   requests only when the branch is deleted through the web UI (or by the
//!   repository's auto-delete setting). Deleting via `git push --delete` or the
//!   refs API can instead **close the dependents outright**. So we `PATCH`
//!   their bases ourselves, first.
//! * **Pass the squash title and message explicitly.** A head branch's history
//!   is full of `[nspr]` revision commits. A repository configured with
//!   `squash_merge_commit_message = COMMIT_MESSAGES` would paste all of it onto
//!   the trunk.
//!
//! # The one force-push
//!
//! Repairing a dependent means rewriting its branch, which is the single place
//! `nspr` force-pushes. It is safe precisely because it is **diff-neutral**:
//! the tree at the new tip equals the tree at the old tip, and the new base
//! (`S`) has the same tree the old base did, so GitHub renders the identical
//! patch and inline comments re-anchor by position. [`land_layer`] checks this
//! and reports a warning if it does not hold — which it legitimately may not,
//! if somebody else landed something in the meantime.

use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Result, WrapErr as _, bail, eyre};
use git2::Oid;

use crate::config::Config;
use crate::forge::{Forge, PrState, PullRequestUpdate, PushSpec, SquashMerge};
use crate::git::Git;
use crate::review_diff::displayed_patch_id;
use crate::stack::{Dep, Stack};
use crate::trailers::DEPENDS_ON;

#[derive(Debug, Clone, Default)]
pub struct LandOptions {
    /// Override the squash commit's body.
    pub message: Option<String>,
    /// Leave the local commits alone instead of rebasing them onto the squash
    /// commit. Only useful for inspection; the next `nspr diff` would try to
    /// re-create the landed pull request.
    pub keep_local: bool,
}

/// What happened to one dependent layer.
#[derive(Debug, Clone)]
pub struct Repair {
    pub number: u64,
    pub branch: String,
    pub old_tip: Oid,
    pub new_tip: Oid,
    /// Number of revisions replayed onto the squash commit.
    pub revisions: usize,
    /// True if the replay conflicted and the branch was collapsed to a single
    /// commit instead.
    pub collapsed: bool,
    /// True if this was a *direct* dependent, and so was retargeted at the
    /// trunk.
    pub retargeted: bool,
}

#[derive(Debug, Clone)]
pub struct LandOutcome {
    pub number: u64,
    pub title: String,
    /// The squash commit now on the trunk.
    pub squash: Oid,
    pub repaired: Vec<Repair>,
    pub warnings: Vec<String>,
}

/// Pre-land snapshot of a dependent, taken before anything is mutated.
struct Dependent {
    layer: usize,
    number: u64,
    branch: String,
    base: String,
    tip: Oid,
}

/// Squash-merge layer `index` and repair its dependents.
///
/// The caller must re-discover the stack afterwards: the local commits are
/// rebased onto the squash commit, so their ids change and the landed layer is
/// gone.
pub async fn land_layer(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    index: usize,
    opts: &LandOptions,
) -> Result<LandOutcome> {
    git.check_no_uncommitted_changes()?;
    crate::upgrade::reject_if_legacy_spr(git, forge, config, stack).await?;

    let layer = &stack.layers[index];
    if let Dep::Layer(parent_idx) = layer.dep {
        let parent = &stack.layers[parent_idx];
        let parent_desc = match parent.pr {
            Some(n) => format!("#{n} (`{}`)", parent.subject()),
            None => format!("`{}`", parent.subject()),
        };
        let root_hint = stack
            .layers
            .iter()
            .find(|l| l.dep == Dep::Main)
            .and_then(|l| l.pr)
            .map(|n| {
                format!(" (e.g. `nspr land --pr={n}` or `nspr land --bottom`)")
            })
            .unwrap_or_default();
        bail!(
            "`{}` is stacked on {parent_desc}, so it cannot land yet.\n\
             Stacked pull requests must be merged from the bottom up into `{}` so each PR merges only its own changes.\n\
             • Land the bottom of the stack first{root_hint}, or run `nspr land --all` to land all ready layers in order.\n\
             • If this commit is actually independent of {parent_desc}, add `{DEPENDS_ON}: {}` to its commit message.",
            layer.subject(),
            config.trunk,
            config.trunk,
        );
    }

    let number = layer.pr.ok_or_else(|| {
        eyre!(
            "`{}` has no pull request yet; run `nspr diff` first",
            layer.subject()
        )
    })?;
    let pr = get_synced_pull_request(git, forge, number, true).await?;
    match pr.state {
        PrState::Merged => bail!(
            "#{number} has already been merged. Run `nspr sync` to bring the \
             local stack up to date."
        ),
        PrState::Closed => bail!("#{number} is closed."),
        PrState::Open => {}
    }
    if pr.draft {
        bail!("#{number} is still a draft; mark it ready for review first.");
    }

    let trunk_tip = forge
        .branch_oid(&config.trunk)
        .await?
        .ok_or_else(|| eyre!("no `{}` branch on the remote", config.trunk))?;
    forge.fetch_commit(trunk_tip).await?;

    check_merge_equals_cherrypick(git, layer.commit, trunk_tip, pr.head_oid)?;

    let mut warnings = Vec::new();
    let dependents =
        snapshot_dependents(git, forge, stack, index, config, &mut warnings)
            .await?;
    let direct: HashSet<usize> =
        stack.direct_dependents_of(index).into_iter().collect();

    // --- Step 1: retarget direct dependents, before anything is merged. -----
    let mut retargeted: Vec<(u64, String)> = Vec::new();
    for d in &dependents {
        if !direct.contains(&d.layer) || d.base == config.trunk {
            continue;
        }
        forge
            .update_pull_request(
                d.number,
                PullRequestUpdate {
                    base: Some(config.trunk.clone()),
                    ..Default::default()
                },
            )
            .await?;
        retargeted.push((d.number, d.base.clone()));
    }

    // --- Step 2: squash merge, with a compare-and-swap guard. ---------------
    let (title, message) = squash_message(layer, number, opts);
    let merged = forge
        .merge_pull_request(
            number,
            SquashMerge {
                title: title.clone(),
                message,
                expected_head: pr.head_oid,
            },
        )
        .await;

    let squash = match merged {
        Ok(oid) => oid,
        Err(e) => {
            // Put the dependents back where they were, so a failed land is not
            // also a mess.
            rollback(forge, &retargeted).await;
            return Err(e).wrap_err(format!("could not merge #{number}"));
        }
    };
    forge.fetch_commit(squash).await?;

    // --- Steps 3-5: replay each dependent's revisions onto the new tip. -----
    //
    // Topological order matters: a dependent of a dependent must be replayed
    // onto its own dependency's *new* tip, not its old one.
    let mut new_tip_of: HashMap<usize, Oid> = HashMap::from([(index, squash)]);
    let mut old_tip_of: HashMap<usize, Oid> =
        HashMap::from([(index, pr.head_oid)]);
    let mut repaired = Vec::new();

    let mut push_specs: Vec<PushSpec> =
        Vec::with_capacity(dependents.len() + 1);
    let mut ref_updates: Vec<(u64, Oid, Option<Oid>)> =
        Vec::with_capacity(dependents.len());

    for d in &dependents {
        let Dep::Layer(dep) = stack.layers[d.layer].dep else {
            unreachable!("dependents_of only returns layer dependencies");
        };
        let old_root = old_tip_of[&dep];
        let new_root = new_tip_of[&dep];

        let clean_msg = stack.layers[d.layer].message.clean_for_branch();
        let revisions = branch_revisions(git, d.tip, old_root)?;
        let (new_tip, collapsed) =
            match replay(git, &revisions, new_root, &clean_msg)? {
                Some(tip) => (tip, false),
                None => (
                    collapse(
                        git,
                        d,
                        old_root,
                        new_root,
                        &clean_msg,
                        &mut warnings,
                    )?,
                    true,
                ),
            };

        // The justification for force-pushing: the reviewer sees the same
        // patch afterwards.
        let before = displayed_patch_id(git.repo(), old_root, d.tip)?;
        let after = displayed_patch_id(git.repo(), new_root, new_tip)?;
        if before != after {
            warnings.push(format!(
                "#{}'s diff changed while landing #{number}; some inline \
                 comments may be marked outdated. This normally means the \
                 trunk moved underneath you.",
                d.number
            ));
        }

        push_specs.push(
            PushSpec::forced(&d.branch, new_tip)
                .with_label(format!("#{}", d.number)),
        );
        let new_root_commit =
            branch_revisions(git, new_tip, new_root)?.first().copied();
        ref_updates.push((d.number, new_tip, new_root_commit));

        new_tip_of.insert(d.layer, new_tip);
        old_tip_of.insert(d.layer, d.tip);
        repaired.push(Repair {
            number: d.number,
            branch: d.branch.clone(),
            old_tip: d.tip,
            new_tip,
            revisions: revisions.len(),
            collapsed,
            retargeted: direct.contains(&d.layer),
        });
    }

    // --- Step 6: push all repaired branches and delete the merged head branch
    // in a single git push operation. ----------------------------------------
    push_specs
        .push(PushSpec::delete(&pr.head).with_label(format!("delete #{number}")));
    forge.push(&push_specs).await?;

    for (dep_num, new_tip, new_root_commit) in ref_updates {
        crate::refs::update(git, dep_num, new_tip)?;
        if let Some(root_commit) = new_root_commit {
            crate::refs::update_root(git, dep_num, root_commit)?;
        }
    }
    crate::refs::remove(git, number)?;

    // --- Local cleanup: the landed commit becomes empty and drops out. ------
    if !opts.keep_local {
        let commits: Vec<Oid> = stack.layers.iter().map(|l| l.commit).collect();
        git.rebase_commits(&commits, squash).wrap_err(
            "#{number} landed, but the local stack could not be rebased onto \
             it. Run `git rebase --onto <trunk>` manually, then `nspr sync`.",
        )?;
    }

    Ok(LandOutcome {
        number,
        title,
        squash,
        repaired,
        warnings,
    })
}

/// The lowest layer that is ready to land, if any.
pub fn next_landable(stack: &Stack) -> Option<usize> {
    stack
        .layers
        .iter()
        .position(|l| l.dep == Dep::Main && l.pr.is_some())
}

/// Refuse to land something the reviewers have not seen.
///
/// Derived from spr's equivalent check. Cherry-picking the local commit onto
/// the trunk and merging the pull request into the trunk must produce the same
/// tree; if they differ, the local commit has moved on since the last push.
fn check_merge_equals_cherrypick(
    git: &Git,
    local_commit: Oid,
    trunk_tip: Oid,
    head_oid: Oid,
) -> Result<()> {
    let index = git.cherrypick(local_commit, trunk_tip)?;
    if index.has_conflicts() {
        bail!(
            "this commit no longer applies on top of the trunk. Run \
             `nspr sync` to rebase, then try again."
        );
    }
    let cherrypicked = git.write_index(index)?;

    let merged = {
        let repo = git.repo();
        let trunk = repo.find_commit(trunk_tip)?;
        let head = repo.find_commit(head_oid)?;
        repo.merge_commits(&trunk, &head, None)?
    };
    let matches =
        !merged.has_conflicts() && git.write_index(merged)? == cherrypicked;

    if !matches {
        bail!(
            "the local commit has changed since the pull request was last \
             pushed, so landing it would merge something nobody reviewed. Run \
             `nspr diff` first."
        );
    }
    Ok(())
}

/// Read a pull request from the forge, waiting briefly if `pr.head_oid` is
/// lagging behind a branch tip that `nspr` itself just pushed.
///
/// GitHub updates `PullRequest.headRefOid` asynchronously in a background
/// worker after `git-receive-pack` completes. During `nspr land --all` (or
/// `nspr diff && nspr land`), the next layer is queried milliseconds after its
/// branch was pushed: the live Git ref (`branch_oid`) already points at the
/// new commit recorded in `refs/nspr/pr/<number>`, while `get_pull_request`
/// can still return the pre-push `head_oid` for 100ms–2s.
async fn get_synced_pull_request(
    git: &Git,
    forge: &dyn Forge,
    number: u64,
    wait_for_pr_sync: bool,
) -> Result<crate::forge::PullRequest> {
    let mut pr = forge.get_pull_request(number).await?;
    if pr.state != PrState::Open {
        return Ok(pr);
    }
    if let Some(recorded_oid) = crate::refs::get(git, number)
        && pr.head_oid != recorded_oid
        && forge.branch_oid(&pr.head).await? == Some(recorded_oid)
    {
        if wait_for_pr_sync {
            for delay_ms in [50_u64, 150, 300, 600, 1000, 1500, 2000, 2000] {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                    .await;
                pr = forge.get_pull_request(number).await?;
                if pr.head_oid == recorded_oid || pr.state != PrState::Open {
                    break;
                }
            }
        }
        pr.head_oid = recorded_oid;
    }
    forge.fetch_commit(pr.head_oid).await?;
    Ok(pr)
}

/// Capture every transitive dependent's remote state before we mutate
/// anything.
async fn snapshot_dependents(
    git: &Git,
    forge: &dyn Forge,
    stack: &Stack,
    index: usize,
    config: &Config,
    warnings: &mut Vec<String>,
) -> Result<Vec<Dependent>> {
    let mut out = Vec::new();
    for layer in stack.dependents_of(index) {
        let number = stack.layers[layer].pr.ok_or_else(|| {
            eyre!(
                "`{}` depends on this layer but has no pull request yet; run \
                 `nspr diff` first so it can be retargeted",
                stack.layers[layer].subject()
            )
        })?;
        let pr = get_synced_pull_request(git, forge, number, false).await?;
        if pr.state != PrState::Open {
            warnings.push(format!("#{number} is not open; leaving it alone."));
            continue;
        }
        if let Some(w) =
            crate::guardrails::auto_merge_warning(&pr, &config.trunk)
        {
            warnings.push(w);
        }
        out.push(Dependent {
            layer,
            number,
            branch: pr.head,
            base: pr.base,
            tip: pr.head_oid,
        });
    }
    Ok(out)
}

async fn rollback(forge: &dyn Forge, retargeted: &[(u64, String)]) {
    for (number, base) in retargeted {
        let _ = forge
            .update_pull_request(
                *number,
                PullRequestUpdate {
                    base: Some(base.clone()),
                    ..Default::default()
                },
            )
            .await;
    }
}

/// Title and body for the squash commit.
///
/// `Depends-On:` is dropped — it describes a review-time relationship that is
/// meaningless once the commit is on the trunk — but `Pull-Request:` and any
/// foreign trailers (`Signed-off-by:` and friends) are kept.
fn squash_message(
    layer: &crate::stack::Layer,
    number: u64,
    opts: &LandOptions,
) -> (String, String) {
    let title = format!("{} (#{number})", layer.subject());
    if let Some(message) = &opts.message {
        return (title, message.clone());
    }

    let mut message = layer.message.clone();
    message.remove(DEPENDS_ON);
    message.subject = String::new();
    (title, message.render().trim().to_string())
}

/// The commits `nspr` itself put on a head branch, oldest first.
///
/// Every update appends a commit whose **first** parent is the previous tip, so
/// the branch's own revisions are exactly the first-parent chain from the tip
/// down to the point where it joins the branch it was stacked on. `stop` is
/// that branch's pre-land tip; because head branches only ever fast-forward,
/// every historical tip of it is an ancestor of `stop`, which makes the test
/// below exact.
pub(crate) fn branch_revisions(
    git: &Git,
    tip: Oid,
    stop: Oid,
) -> Result<Vec<Oid>> {
    let mut out = Vec::new();
    let mut cur = tip;
    while !git.is_ancestor(cur, stop)? {
        out.push(cur);
        let commit = git.repo().find_commit(cur)?;
        match commit.parent_id(0) {
            Ok(parent) => cur = parent,
            // A root commit: the whole branch is its own history.
            Err(_) => break,
        }
    }
    out.reverse();
    Ok(out)
}

/// Replay a branch's revisions onto `onto`, preserving the boundaries
/// reviewers have been using as "changes since revision N".
///
/// Returns `None` if any revision conflicts, leaving the caller to collapse.
///
/// Each revision is cherry-picked relative to its first parent, so the diff
/// being replayed is "what changed in this revision" — which includes whatever
/// was merged forward from the dependency at the time. Those parts are already
/// present in `onto`, so they apply as no-ops and the commit drops out as
/// empty. That is how merge-forward commits disappear without special-casing.
pub(crate) fn replay(
    git: &Git,
    revisions: &[Oid],
    onto: Oid,
    initial_message: &str,
) -> Result<Option<Oid>> {
    let repo = git.repo();
    let mut tip = onto;
    let mut first = true;

    for &oid in revisions {
        let commit = repo.find_commit(oid)?;
        // Update commits are merges; pick relative to the first parent.
        let mainline = if commit.parent_count() > 1 { 1 } else { 0 };
        let onto_commit = repo.find_commit(tip)?;

        let mut index =
            repo.cherrypick_commit(&commit, &onto_commit, mainline, None)?;
        if index.has_conflicts() {
            return Ok(None);
        }
        let tree_oid = index.write_tree_to(repo)?;
        if tree_oid == onto_commit.tree_id() {
            // Purely a merge-forward of something now in `onto`.
            continue;
        }

        let msg = if first {
            first = false;
            initial_message.to_string()
        } else {
            String::from_utf8_lossy(commit.message_bytes()).into_owned()
        };

        tip = repo.commit(
            None,
            &commit.author(),
            &commit.committer(),
            &msg,
            &repo.find_tree(tree_oid)?,
            &[&onto_commit],
        )?;
    }

    if first {
        return Ok(None);
    }

    Ok(Some(tip))
}

/// Fallback when the replay conflicts: one commit carrying the layer's content
/// as it should look on top of the squash commit.
fn collapse(
    git: &Git,
    d: &Dependent,
    old_root: Oid,
    new_root: Oid,
    initial_message: &str,
    warnings: &mut Vec<String>,
) -> Result<Oid> {
    let index = git.merge_trees(
        git.tree_of(old_root)?,
        git.tree_of(new_root)?,
        git.tree_of(d.tip)?,
    )?;
    if index.has_conflicts() {
        bail!(
            "#{} conflicts with the squashed commit and cannot be rebased \
             automatically. Its branch has been left untouched; resolve it \
             locally and run `nspr diff`.",
            d.number
        );
    }
    let tree = git.write_index(index)?;

    warnings.push(format!(
        "#{}'s revision history could not be replayed cleanly, so it was \
         collapsed to a single commit. Reviewers will lose the \
         revision-by-revision view.",
        d.number
    ));

    git.create_derived_commit(d.tip, initial_message, tree, &[new_root])
}
