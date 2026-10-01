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
    /// When set, only land layers in this set (for example, a single connected
    /// stack component selected via `nspr land --all <PR>`).
    pub only_layers: Option<HashSet<usize>>,
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
    let comp_set: HashSet<usize> =
        stack.component_of(index).into_iter().collect();
    let prs_for_check = crate::engine::gather_for(
        forge,
        stack,
        &crate::engine::SyncOptions {
            only_layers: Some(comp_set.clone()),
            ..Default::default()
        },
    )
    .await?;
    crate::upgrade::reject_if_legacy_spr_with_prs(
        git,
        config,
        stack,
        &prs_for_check,
        None,
        Some(&comp_set),
    )?;

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
    if pr.state == PrState::Closed {
        bail!("#{number} is closed.");
    }
    if pr.state == PrState::Open && pr.draft {
        bail!("#{number} is still a draft; mark it ready for review first.");
    }

    let trunk_tip = forge
        .branch_oid(&config.trunk)
        .await?
        .ok_or_else(|| eyre!("no `{}` branch on the remote", config.trunk))?;
    forge.fetch_commit(trunk_tip).await?;

    let already_landed_on_trunk = if pr.state == PrState::Merged {
        let squash_candidate = pr.merge_commit.unwrap_or(trunk_tip);
        forge.fetch_commit(squash_candidate).await?;
        check_already_merged_matches_local(
            git,
            layer,
            number,
            pr.base_oid,
            pr.head_oid,
            squash_candidate,
            trunk_tip,
            &config.trunk,
        )?;
        None
    } else if let Some(landed_oid) = find_landed_commit_on_trunk(
        git.repo(),
        trunk_tip,
        Oid::ZERO_SHA1,
        number,
    )? {
        check_already_merged_matches_local(
            git,
            layer,
            number,
            pr.base_oid,
            pr.head_oid,
            landed_oid,
            trunk_tip,
            &config.trunk,
        )?;
        Some(landed_oid)
    } else {
        check_merge_equals_cherrypick(
            git,
            layer,
            trunk_tip,
            Oid::ZERO_SHA1,
            pr.head_oid,
        )?
    };

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
    let squash = if pr.state == PrState::Merged {
        pr.merge_commit.unwrap_or(trunk_tip)
    } else if let Some(landed_oid) = already_landed_on_trunk {
        forge
            .update_pull_request(
                number,
                PullRequestUpdate {
                    state: Some(PrState::Closed),
                    ..Default::default()
                },
            )
            .await?;
        warnings.push(format!(
            "#{number} was still open on GitHub, but its changes were already \
             on `{}` ({}); closed #{number} without creating a duplicate \
             commit.",
            config.trunk,
            git.short_id(landed_oid)?,
        ));
        landed_oid
    } else {
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

        match merged {
            Ok(oid) => oid,
            Err(e) => {
                // Put the dependents back where they were, so a failed land is not
                // also a mess.
                rollback(forge, &retargeted).await;
                return Err(e).wrap_err(format!("could not merge #{number}"));
            }
        }
    };
    forge.fetch_commit(squash).await?;
    print_landed(git, config, number, &title, squash)?;

    // --- Steps 3-5: replay each dependent's revisions onto the new tip. -----
    //
    // Topological order matters: a dependent of a dependent must be replayed
    // onto its own dependency's *new* tip, not its old one.
    let mut new_tip_of: HashMap<usize, Oid> = HashMap::from([(index, squash)]);
    let mut old_tip_of: HashMap<usize, Oid> =
        HashMap::from([(index, pr.head_oid)]);
    let mut repaired = Vec::new();

    let mut push_specs: Vec<PushSpec> = Vec::with_capacity(dependents.len());
    let mut ref_updates: Vec<(u64, Oid, Option<Oid>)> =
        Vec::with_capacity(dependents.len());
    let preserve_commit_history = if dependents.is_empty() {
        true
    } else {
        config
            .preserve_commit_history
            .resolve(forge.repo_merge_settings().await?)
    };

    for d in &dependents {
        let Dep::Layer(dep) = stack.layers[d.layer].dep else {
            unreachable!("dependents_of only returns layer dependencies");
        };
        let old_root = old_tip_of[&dep];
        let new_root = new_tip_of[&dep];

        if old_root == Oid::ZERO_SHA1
            || git.is_ancestor(new_root, d.tip).unwrap_or(false)
        {
            new_tip_of.insert(d.layer, d.tip);
            old_tip_of.insert(d.layer, d.tip);
            continue;
        }

        let clean_msg = crate::engine::branch_initial_message(
            preserve_commit_history,
            &stack.layers[d.layer].message,
        );
        let initial_msg =
            (!preserve_commit_history).then_some(clean_msg.as_str());
        let revisions = branch_revisions(git, d.tip, old_root)?;
        let (new_tip, collapsed) = match replay(
            git,
            &revisions,
            new_root,
            initial_msg,
            Some(stack.layers[d.layer].commit),
        )? {
            Some(tip) => (tip, false),
            None => {
                log::debug!(
                    "replaying {} revision(s) of #{} onto {new_root} conflicted; falling back to collapsed commit",
                    revisions.len(),
                    d.number
                );
                (
                    collapse(
                        git,
                        d,
                        old_root,
                        new_root,
                        &clean_msg,
                        &mut warnings,
                    )?,
                    true,
                )
            }
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

    // --- Step 6: push any repaired dependent branches and delete the merged
    // head branch via the forge API. -----------------------------------------
    if !push_specs.is_empty() {
        forge.push(&push_specs).await?;
        print_repaired(config, &repaired);
    }

    for (dep_num, new_tip, new_root_commit) in ref_updates {
        crate::refs::update(git, dep_num, new_tip)?;
        if let Some(root_commit) = new_root_commit {
            crate::refs::update_root(git, dep_num, root_commit)?;
        }
    }
    forge.delete_branch(&pr.head).await?;
    crate::refs::remove(git, number)?;

    // --- Local cleanup: the landed commit becomes empty and drops out. ------
    let rebase_onto = if git.is_ancestor(squash, trunk_tip)? {
        trunk_tip
    } else {
        squash
    };
    if !opts.keep_local {
        let removed = HashSet::from([index]);
        if let Err(rebase_err) =
            stack.rebase_without(git, &removed, rebase_onto, false)
        {
            if rebase_onto != stack.base && dependents.is_empty() {
                log::debug!(
                    "rebasing remaining local commits onto {rebase_onto} failed ({rebase_err}); falling back to rebasing onto current stack base {}",
                    stack.base
                );
                stack
                    .rebase_without(git, &removed, stack.base, false)
                    .wrap_err(
                        "#{number} landed, but the local stack could not be rebased onto \
                         it. Run `git rebase --onto <trunk>` manually, then `nspr sync`.",
                    )?;
                warnings.push(format!(
                    "#{number} landed and was removed from your local branch, \
                     but rebasing the remaining commits onto `{}` failed \
                     ({rebase_err}); left the remaining commits on their \
                     current base `{}`. Run `nspr sync` when ready.",
                    config.trunk,
                    git.short_id(stack.base)?,
                ));
            } else {
                return Err(rebase_err).wrap_err(format!(
                    "#{number} landed, but the local stack could not be rebased onto \
                     it. Run `git rebase --onto {}` manually, then `nspr sync`.",
                    config.trunk
                ));
            }
        }
    }

    Ok(LandOutcome {
        number,
        title,
        squash: rebase_onto,
        repaired,
        warnings,
    })
}

/// State of a layer's pull request tracked across `land_all`.
#[derive(Debug, Clone)]
struct LayerPrState {
    number: u64,
    branch: String,
    base: String,
    base_oid: Oid,
    tip: Oid,
    merge_commit: Option<Oid>,
    state: PrState,
    draft: bool,
    auto_merge_warning: Option<String>,
    /// If this layer depends on another layer in `stack`, the remote tip of
    /// that dependency that `tip` is currently anchored on.
    anchor_tip: Option<Oid>,
}

/// Squash-merge all ready layers from the bottom up, preserving their existing
/// head commits (and green CI checks) whenever GitHub can 3-way merge them
/// cleanly into the trunk, and only repairing any remaining unmerged layers
/// once at the end.
pub async fn land_all(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    opts: &LandOptions,
) -> Result<Vec<LandOutcome>> {
    git.check_no_uncommitted_changes()?;
    let prs_for_check = crate::engine::gather_for(
        forge,
        stack,
        &crate::engine::SyncOptions {
            only_layers: opts.only_layers.clone(),
            ..Default::default()
        },
    )
    .await?;
    crate::upgrade::reject_if_legacy_spr_with_prs(
        git,
        config,
        stack,
        &prs_for_check,
        None,
        opts.only_layers.as_ref(),
    )?;

    if stack.layers.is_empty() {
        bail!("nothing to land: no commits ahead of `{}`", config.trunk);
    }
    if next_landable(stack).is_none() {
        return land_layer(git, forge, config, stack, 0, opts)
            .await
            .map(|o| vec![o]);
    }

    let mut current_trunk = forge
        .branch_oid(&config.trunk)
        .await?
        .ok_or_else(|| eyre!("no `{}` branch on the remote", config.trunk))?;
    forge.fetch_commit(current_trunk).await?;

    // Snapshot pull requests for all layers up front before mutating anything.
    let mut pr_states: HashMap<usize, LayerPrState> = HashMap::new();
    for (i, layer) in stack.layers.iter().enumerate() {
        if let Some(allowed) = &opts.only_layers
            && !allowed.contains(&i)
        {
            continue;
        }
        if let Some(number) = layer.pr {
            let pr =
                get_synced_pull_request(git, forge, number, i == 0).await?;
            let auto_merge_warning = if pr.state == PrState::Open {
                crate::guardrails::auto_merge_warning(&pr, &config.trunk)
            } else {
                None
            };
            pr_states.insert(
                i,
                LayerPrState {
                    number,
                    branch: pr.head,
                    base: pr.base,
                    base_oid: pr.base_oid,
                    tip: pr.head_oid,
                    merge_commit: pr.merge_commit,
                    state: pr.state,
                    draft: pr.draft,
                    auto_merge_warning,
                    anchor_tip: None,
                },
            );
        } else if stack
            .dependents_of(i)
            .into_iter()
            .any(|d| stack.layers[d].pr.is_some())
        {
            bail!(
                "`{}` has no pull request yet, but a layer above it does; run \
                 `nspr diff` first so it can be retargeted",
                layer.subject()
            );
        }
    }

    for i in 0..stack.layers.len() {
        if let Dep::Layer(dep) = stack.layers[i].dep
            && let Some(dep_tip) = pr_states.get(&dep).map(|s| s.tip)
            && let Some(state) = pr_states.get_mut(&i)
        {
            state.anchor_tip = Some(dep_tip);
        }
    }

    let mut landed_layers: HashSet<usize> = HashSet::new();
    let mut outcomes: Vec<LandOutcome> = Vec::new();
    let mut stop_warning: Option<String> = None;
    let mut first_error: Option<color_eyre::Report> = None;

    for index in 0..stack.layers.len() {
        if let Some(allowed) = &opts.only_layers
            && !allowed.contains(&index)
        {
            continue;
        }
        let dep_ready = match stack.layers[index].dep {
            Dep::Main => true,
            Dep::Layer(dep) => landed_layers.contains(&dep),
            Dep::ExternalPr(_) => false,
        };
        if !dep_ready {
            continue;
        }

        let Some(state) = pr_states.get(&index).cloned() else {
            if first_error.is_none() {
                first_error = Some(eyre!(
                    "`{}` has no pull request yet; run `nspr diff` first",
                    stack.layers[index].subject()
                ));
            }
            continue;
        };

        match state.state {
            PrState::Merged => {
                let squash = state.merge_commit.unwrap_or(current_trunk);
                forge.fetch_commit(squash).await?;
                if let Err(e) = check_already_merged_matches_local(
                    git,
                    &stack.layers[index],
                    state.number,
                    state.base_oid,
                    state.tip,
                    squash,
                    current_trunk,
                    &config.trunk,
                ) {
                    let msg = format!("{e:#}");
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                    stop_warning =
                        Some(format!("stopped at #{}: {msg}", state.number));
                    continue;
                }
                for d_layer in stack.direct_dependents_of(index) {
                    if let Some(d_state) = pr_states.get(&d_layer)
                        && d_state.state == PrState::Open
                        && d_state.base != config.trunk
                    {
                        let d_num = d_state.number;
                        forge
                            .update_pull_request(
                                d_num,
                                PullRequestUpdate {
                                    base: Some(config.trunk.clone()),
                                    ..Default::default()
                                },
                            )
                            .await?;
                        pr_states.get_mut(&d_layer).unwrap().base =
                            config.trunk.clone();
                    }
                }
                let (title, _) =
                    squash_message(&stack.layers[index], state.number, opts);
                print_landed(git, config, state.number, &title, squash)?;
                forge.delete_branch(&state.branch).await?;
                crate::refs::remove(git, state.number)?;

                landed_layers.insert(index);
                if !git.is_ancestor(squash, current_trunk)? {
                    current_trunk = squash;
                }
                outcomes.push(LandOutcome {
                    number: state.number,
                    title,
                    squash: current_trunk,
                    repaired: Vec::new(),
                    warnings: Vec::new(),
                });
                continue;
            }
            PrState::Closed => {
                if first_error.is_none() {
                    first_error = Some(eyre!("#{} is closed.", state.number));
                }
                continue;
            }
            PrState::Open => {}
        }

        if let Some(landed_oid) = find_landed_commit_on_trunk(
            git.repo(),
            current_trunk,
            Oid::ZERO_SHA1,
            state.number,
        )? {
            if let Err(e) = check_already_merged_matches_local(
                git,
                &stack.layers[index],
                state.number,
                state.base_oid,
                state.tip,
                landed_oid,
                current_trunk,
                &config.trunk,
            ) {
                let msg = format!("{e:#}");
                if first_error.is_none() {
                    first_error = Some(e);
                }
                stop_warning =
                    Some(format!("stopped at #{}: {msg}", state.number));
                continue;
            }
            for d_layer in stack.direct_dependents_of(index) {
                if let Some(d_state) = pr_states.get(&d_layer)
                    && d_state.state == PrState::Open
                    && d_state.base != config.trunk
                {
                    let d_num = d_state.number;
                    forge
                        .update_pull_request(
                            d_num,
                            PullRequestUpdate {
                                base: Some(config.trunk.clone()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    pr_states.get_mut(&d_layer).unwrap().base =
                        config.trunk.clone();
                }
            }
            forge
                .update_pull_request(
                    state.number,
                    PullRequestUpdate {
                        state: Some(PrState::Closed),
                        ..Default::default()
                    },
                )
                .await?;
            let (title, _) =
                squash_message(&stack.layers[index], state.number, opts);
            print_landed(git, config, state.number, &title, landed_oid)?;
            forge.delete_branch(&state.branch).await?;
            crate::refs::remove(git, state.number)?;

            landed_layers.insert(index);
            pr_states.get_mut(&index).unwrap().state = PrState::Closed;
            if !git.is_ancestor(landed_oid, current_trunk)? {
                current_trunk = landed_oid;
            }
            let warning = format!(
                "#{} was still open on GitHub, but its changes were already \
                 on `{}` ({}); closed #{} without creating a duplicate \
                 commit.",
                state.number,
                config.trunk,
                git.short_id(landed_oid)?,
                state.number,
            );
            outcomes.push(LandOutcome {
                number: state.number,
                title,
                squash: current_trunk,
                repaired: Vec::new(),
                warnings: vec![warning],
            });
            continue;
        }

        if state.draft {
            if first_error.is_none() {
                first_error = Some(eyre!(
                    "#{} is still a draft; mark it ready for review first.",
                    state.number
                ));
            }
            continue;
        }

        if git.tree_of(stack.layers[index].commit)?
            == git.tree_of(stack.layers[index].parent)?
        {
            let msg = format!(
                "`{}` has no file changes relative to its parent; nothing to land.",
                stack.layers[index].subject()
            );
            if first_error.is_none() {
                first_error = Some(eyre!("{msg}"));
            }
            stop_warning = Some(format!("stopped at #{}: {msg}", state.number));
            continue;
        }

        let cp_index =
            git.cherrypick(stack.layers[index].commit, current_trunk)?;
        if cp_index.has_conflicts() {
            let msg = "this commit no longer applies on top of the trunk. Run \
                       `nspr sync` to rebase, then try again.";
            if first_error.is_none() {
                first_error = Some(eyre!("{msg}"));
            }
            stop_warning = Some(format!("stopped at #{}: {msg}", state.number));
            continue;
        }
        let cherrypicked = git.write_index(cp_index)?;

        let mut current_tip = state.tip;
        let direct_merge_matches = {
            let repo = git.repo();
            let trunk_c = repo.find_commit(current_trunk)?;
            let head_c = repo.find_commit(current_tip)?;
            let merged = repo.merge_commits(&trunk_c, &head_c, None)?;
            !merged.has_conflicts() && git.write_index(merged)? == cherrypicked
        };

        if !direct_merge_matches {
            log::debug!(
                "land_all: #{} (tip {}) does not 3-way merge directly onto trunk {}; checking re-anchored patch",
                state.number,
                current_tip,
                current_trunk
            );
            // Check whether the PR branch has the exact reviewed patch relative
            // to its pre-land dependency anchor, and only conflicts with
            // `current_trunk` because an earlier layer in this stack modified
            // overlapping lines or was amended without restacking this layer.
            let reanchored_matches = if let Some(old_root) = state.anchor_tip {
                let old_base = git.merge_base(old_root, current_tip)?;
                let reanchored = git.merge_trees(
                    git.tree_of(old_base)?,
                    git.tree_of(current_trunk)?,
                    git.tree_of(current_tip)?,
                )?;
                !reanchored.has_conflicts()
                    && git.write_index(reanchored)? == cherrypicked
            } else {
                false
            };

            if !reanchored_matches {
                let msg = "the local commit has changed since the pull request \
                           was last pushed, so landing it would merge something \
                           nobody reviewed. Run `nspr diff` first.";
                if first_error.is_none() {
                    first_error = Some(eyre!("{msg}"));
                }
                stop_warning =
                    Some(format!("stopped at #{}: {msg}", state.number));
                continue;
            }

            if cherrypicked != git.tree_of(current_trunk)? {
                log::debug!(
                    "land_all: #{} patch matches local commit; repairing remaining branches onto trunk {}",
                    state.number,
                    current_trunk
                );
                let last_outcome = outcomes
                    .last_mut()
                    .expect("anchor_tip implies an earlier layer landed");
                let repaired = repair_remaining_dependents(
                    git,
                    forge,
                    config,
                    stack,
                    &landed_layers,
                    current_trunk,
                    &mut pr_states,
                    &mut last_outcome.warnings,
                )
                .await?;
                last_outcome.repaired.extend(repaired);

                let synced_pr =
                    get_synced_pull_request(git, forge, state.number, true)
                        .await?;
                current_tip = synced_pr.head_oid;
                pr_states.get_mut(&index).unwrap().tip = current_tip;
            }
        } else {
            log::debug!(
                "land_all: #{} (tip {}) 3-way merges directly onto trunk {} without restacking",
                state.number,
                current_tip,
                current_trunk
            );
        }

        if cherrypicked == git.tree_of(current_trunk)? {
            let landed_oid = find_matching_patch_on_trunk(
                git,
                &stack.layers[index],
                current_trunk,
                Oid::ZERO_SHA1,
            )?
            .unwrap_or(current_trunk);
            for d_layer in stack.direct_dependents_of(index) {
                if let Some(d_state) = pr_states.get(&d_layer)
                    && d_state.state == PrState::Open
                    && d_state.base != config.trunk
                {
                    let d_num = d_state.number;
                    forge
                        .update_pull_request(
                            d_num,
                            PullRequestUpdate {
                                base: Some(config.trunk.clone()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    pr_states.get_mut(&d_layer).unwrap().base =
                        config.trunk.clone();
                }
            }
            forge
                .update_pull_request(
                    state.number,
                    PullRequestUpdate {
                        state: Some(PrState::Closed),
                        ..Default::default()
                    },
                )
                .await?;
            let (title, _) =
                squash_message(&stack.layers[index], state.number, opts);
            print_landed(git, config, state.number, &title, landed_oid)?;
            forge.delete_branch(&state.branch).await?;
            crate::refs::remove(git, state.number)?;

            landed_layers.insert(index);
            pr_states.get_mut(&index).unwrap().state = PrState::Closed;
            let warning = format!(
                "#{} was still open on GitHub, but its changes were already \
                 on `{}` ({}); closed #{} without creating a duplicate \
                 commit.",
                state.number,
                config.trunk,
                git.short_id(landed_oid)?,
                state.number,
            );
            outcomes.push(LandOutcome {
                number: state.number,
                title,
                squash: current_trunk,
                repaired: Vec::new(),
                warnings: vec![warning],
            });
            continue;
        }

        // Retarget this layer (if needed) and its direct open dependents to
        // trunk before merging so GitHub never auto-closes a dependent.
        let mut retargeted: Vec<(usize, u64, String)> = Vec::new();
        if pr_states[&index].base != config.trunk {
            let old_base = pr_states[&index].base.clone();
            forge
                .update_pull_request(
                    state.number,
                    PullRequestUpdate {
                        base: Some(config.trunk.clone()),
                        ..Default::default()
                    },
                )
                .await?;
            pr_states.get_mut(&index).unwrap().base = config.trunk.clone();
            retargeted.push((index, state.number, old_base));
        }
        for d_layer in stack.direct_dependents_of(index) {
            if let Some(d_state) = pr_states.get(&d_layer)
                && d_state.state == PrState::Open
                && d_state.base != config.trunk
            {
                let d_num = d_state.number;
                let old_base = d_state.base.clone();
                forge
                    .update_pull_request(
                        d_num,
                        PullRequestUpdate {
                            base: Some(config.trunk.clone()),
                            ..Default::default()
                        },
                    )
                    .await?;
                pr_states.get_mut(&d_layer).unwrap().base =
                    config.trunk.clone();
                retargeted.push((d_layer, d_num, old_base));
            }
        }

        let (title, message) =
            squash_message(&stack.layers[index], state.number, opts);
        let merged = forge
            .merge_pull_request(
                state.number,
                SquashMerge {
                    title: title.clone(),
                    message,
                    expected_head: current_tip,
                },
            )
            .await;

        let squash = match merged {
            Ok(oid) => oid,
            Err(e) => {
                let pairs: Vec<(u64, String)> = retargeted
                    .iter()
                    .map(|(_, num, base)| (*num, base.clone()))
                    .collect();
                rollback(forge, &pairs).await;
                for (r_layer, _, old_base) in retargeted {
                    if let Some(st) = pr_states.get_mut(&r_layer) {
                        st.base = old_base;
                    }
                }
                let err_msg = format!("{e:#}");
                if first_error.is_none() {
                    first_error = Some(e.wrap_err(format!(
                        "could not merge #{}",
                        state.number
                    )));
                }
                stop_warning =
                    Some(format!("stopped at #{}: {err_msg}", state.number));
                continue;
            }
        };
        forge.fetch_commit(squash).await?;
        print_landed(git, config, state.number, &title, squash)?;
        forge.delete_branch(&state.branch).await?;
        crate::refs::remove(git, state.number)?;

        landed_layers.insert(index);
        pr_states.get_mut(&index).unwrap().state = PrState::Merged;
        current_trunk = squash;
        outcomes.push(LandOutcome {
            number: state.number,
            title,
            squash,
            repaired: Vec::new(),
            warnings: Vec::new(),
        });
    }

    if outcomes.is_empty() {
        return Err(first_error.unwrap_or_else(|| {
            eyre!(
                "nothing at the bottom of the stack is ready to land. Run \
                 `nspr status` to see why."
            )
        }));
    }

    // Repair any remaining unmerged layers once onto the final squash commit.
    let last_outcome = outcomes.last_mut().expect("at least one layer landed");
    let repaired = repair_remaining_dependents(
        git,
        forge,
        config,
        stack,
        &landed_layers,
        current_trunk,
        &mut pr_states,
        &mut last_outcome.warnings,
    )
    .await?;
    last_outcome.repaired.extend(repaired);
    if let Some(w) = stop_warning {
        last_outcome.warnings.push(w);
    }

    if !opts.keep_local {
        stack
            .rebase_without(git, &landed_layers, current_trunk, false)
            .wrap_err(
                "pull requests landed, but the local stack could not be \
                 rebased onto the trunk. Run `git rebase --onto <trunk>` \
                 manually, then `nspr sync`.",
            )?;
    }

    Ok(outcomes)
}

#[allow(clippy::too_many_arguments)]
async fn repair_remaining_dependents(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    landed_layers: &HashSet<usize>,
    current_trunk: Oid,
    pr_states: &mut HashMap<usize, LayerPrState>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Repair>> {
    let mut repaired = Vec::new();
    let mut push_specs: Vec<PushSpec> = Vec::new();
    let mut ref_updates: Vec<(u64, Oid, Option<Oid>)> = Vec::new();
    let preserve_commit_history = config
        .preserve_commit_history
        .resolve(forge.repo_merge_settings().await?);

    for d_layer in 0..stack.layers.len() {
        if landed_layers.contains(&d_layer) {
            continue;
        }
        let Dep::Layer(dep) = stack.layers[d_layer].dep else {
            continue;
        };
        let direct_dep_landed = landed_layers.contains(&dep);
        let new_root = if direct_dep_landed {
            current_trunk
        } else if let Some(dep_state) = pr_states.get(&dep) {
            dep_state.tip
        } else {
            continue;
        };

        let Some(d_state) = pr_states.get(&d_layer).cloned() else {
            continue;
        };
        if d_state.state != PrState::Open {
            warnings.push(format!(
                "#{} is not open; leaving it alone.",
                d_state.number
            ));
            continue;
        }
        let Some(old_root) = d_state.anchor_tip else {
            continue;
        };
        if old_root == new_root {
            continue;
        }

        if let Some(w) = pr_states
            .get_mut(&d_layer)
            .unwrap()
            .auto_merge_warning
            .take()
        {
            warnings.push(w);
        }

        if direct_dep_landed && d_state.base != config.trunk {
            forge
                .update_pull_request(
                    d_state.number,
                    PullRequestUpdate {
                        base: Some(config.trunk.clone()),
                        ..Default::default()
                    },
                )
                .await?;
            pr_states.get_mut(&d_layer).unwrap().base = config.trunk.clone();
        }

        let clean_msg = crate::engine::branch_initial_message(
            preserve_commit_history,
            &stack.layers[d_layer].message,
        );
        let initial_msg =
            (!preserve_commit_history).then_some(clean_msg.as_str());
        let revisions = branch_revisions(git, d_state.tip, old_root)?;
        let d_snap = Dependent {
            layer: d_layer,
            number: d_state.number,
            branch: d_state.branch.clone(),
            base: d_state.base.clone(),
            tip: d_state.tip,
        };
        let (new_tip, collapsed) = match replay(
            git,
            &revisions,
            new_root,
            initial_msg,
            Some(stack.layers[d_layer].commit),
        )? {
            Some(tip) => (tip, false),
            None => {
                log::debug!(
                    "replaying {} revision(s) of #{} onto {new_root} conflicted; falling back to collapsed commit",
                    revisions.len(),
                    d_state.number
                );
                (
                    collapse(
                        git, &d_snap, old_root, new_root, &clean_msg, warnings,
                    )?,
                    true,
                )
            }
        };

        let before = displayed_patch_id(git.repo(), old_root, d_state.tip)?;
        let after = displayed_patch_id(git.repo(), new_root, new_tip)?;
        if before != after {
            warnings.push(format!(
                "#{}'s diff changed while landing; some inline comments may \
                 be marked outdated. This normally means the trunk moved \
                 underneath you.",
                d_state.number
            ));
        }

        push_specs.push(
            PushSpec::forced(&d_state.branch, new_tip)
                .with_label(format!("#{}", d_state.number)),
        );
        let new_root_commit =
            branch_revisions(git, new_tip, new_root)?.first().copied();
        ref_updates.push((d_state.number, new_tip, new_root_commit));

        let st = pr_states.get_mut(&d_layer).unwrap();
        st.tip = new_tip;
        st.anchor_tip = Some(new_root);

        repaired.push(Repair {
            number: d_state.number,
            branch: d_state.branch,
            old_tip: d_state.tip,
            new_tip,
            revisions: revisions.len(),
            collapsed,
            retargeted: direct_dep_landed,
        });
    }

    if !push_specs.is_empty() {
        forge.push(&push_specs).await?;
        print_repaired(config, &repaired);
    }

    for (dep_num, new_tip, new_root_commit) in ref_updates {
        crate::refs::update(git, dep_num, new_tip)?;
        if let Some(root_commit) = new_root_commit {
            crate::refs::update_root(git, dep_num, root_commit)?;
        }
    }

    Ok(repaired)
}

fn print_landed(
    git: &Git,
    config: &Config,
    number: u64,
    title: &str,
    squash: Oid,
) -> Result<()> {
    println!(
        "{} {} {} as {}",
        console::style("landed").green().bold(),
        config.pull_request_link(number, format!("#{number}")),
        title,
        git.short_id(squash)?
    );
    Ok(())
}

fn print_repaired(config: &Config, repaired: &[Repair]) {
    for repair in repaired {
        let pr = config
            .pull_request_link(repair.number, format!("#{}", repair.number));
        if repair.retargeted {
            println!("  repaired {pr} (retargeted → {})", config.trunk);
        } else {
            println!("  repaired {pr}");
        }
    }
}

/// The lowest layer that is ready to land, if any.
pub fn next_landable(stack: &Stack) -> Option<usize> {
    stack
        .layers
        .iter()
        .position(|l| l.dep == Dep::Main && l.pr.is_some())
}

/// Search the first-parent history of `trunk_tip` (stopping at `stop_at` or
/// after 500 commits) for a commit that already landed `#{number}`.
///
/// GitHub's `PUT /pulls/{number}/merge` endpoint can occasionally create the
/// squash commit on the base branch and advance `refs/heads/<trunk>` before
/// failing to mark the pull request as `Merged` in GitHub's database, leaving
/// the pull request in `PrState::Open`.
pub(crate) fn find_landed_commit_on_trunk(
    repo: &git2::Repository,
    trunk_tip: Oid,
    stop_at: Oid,
    number: u64,
) -> Result<Option<Oid>> {
    let suffix = format!(" (#{number})");
    let mut cur = trunk_tip;
    for _ in 0..500 {
        if cur == stop_at {
            break;
        }
        let Ok(commit) = repo.find_commit(cur) else {
            break;
        };
        let raw = String::from_utf8_lossy(commit.message_bytes());
        let parsed = crate::trailers::CommitMessage::parse(&raw);
        if parsed
            .get(crate::trailers::PULL_REQUEST)
            .and_then(crate::stack::parse_pr_ref)
            == Some(number)
            || parsed.subject.trim_end().ends_with(&suffix)
        {
            return Ok(Some(cur));
        }
        match commit.parent_id(0) {
            Ok(parent) => cur = parent,
            Err(_) => break,
        }
    }
    Ok(None)
}

/// Search the first-parent history of `trunk_tip` (stopping at `stop_at` or
/// after 200 commits) for a commit whose tree patch-id matches `layer`.
pub(crate) fn find_matching_patch_on_trunk(
    git: &Git,
    layer: &crate::stack::Layer,
    trunk_tip: Oid,
    stop_at: Oid,
) -> Result<Option<Oid>> {
    let repo = git.repo();
    let local_patch = crate::patch_id::tree_patch_id(
        repo,
        git.tree_of(layer.parent)?,
        git.tree_of(layer.commit)?,
    )?;
    let mut cur = trunk_tip;
    for _ in 0..200 {
        if cur == stop_at {
            break;
        }
        let Ok(commit) = repo.find_commit(cur) else {
            break;
        };
        let Ok(parent) = commit.parent(0) else {
            break;
        };
        let commit_patch = crate::patch_id::tree_patch_id(
            repo,
            parent.tree_id(),
            commit.tree_id(),
        )?;
        if commit_patch == local_patch {
            return Ok(Some(cur));
        }
        cur = parent.id();
    }
    Ok(None)
}

/// Refuse to land something the reviewers have not seen, and detect when all of
/// the layer's changes are already present on `trunk_tip` (so squash-merging
/// would create a duplicate 0-file empty commit).
///
/// Derived from spr's equivalent check. Cherry-picking the local commit onto
/// the trunk and merging the pull request into the trunk must produce the same
/// tree; if they differ, the local commit has moved on since the last push.
fn check_merge_equals_cherrypick(
    git: &Git,
    layer: &crate::stack::Layer,
    trunk_tip: Oid,
    stop_at: Oid,
    head_oid: Oid,
) -> Result<Option<Oid>> {
    if git.tree_of(layer.commit)? == git.tree_of(layer.parent)? {
        bail!(
            "`{}` has no file changes relative to its parent; nothing to land.",
            layer.subject()
        );
    }
    let index = git.cherrypick(layer.commit, trunk_tip)?;
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
    if cherrypicked == git.tree_of(trunk_tip)? {
        let landed =
            find_matching_patch_on_trunk(git, layer, trunk_tip, stop_at)?
                .unwrap_or(trunk_tip);
        return Ok(Some(landed));
    }
    Ok(None)
}

/// Verify that when a pull request is already `Merged` on the forge, the local
/// commit has no unmerged changes beyond what was in the pull request.
#[allow(clippy::too_many_arguments)]
fn check_already_merged_matches_local(
    git: &Git,
    layer: &crate::stack::Layer,
    number: u64,
    base_oid: Oid,
    head_oid: Oid,
    squash: Oid,
    trunk_tip: Oid,
    trunk_name: &str,
) -> Result<()> {
    for target in [squash, trunk_tip] {
        if let Ok(idx) = git.cherrypick(layer.commit, target)
            && !idx.has_conflicts()
            && git.write_index(idx).ok() == git.tree_of(target).ok()
        {
            return Ok(());
        }
    }

    if head_oid != Oid::ZERO_SHA1
        && git.repo().find_commit(head_oid).is_ok()
        && base_oid != Oid::ZERO_SHA1
        && git.repo().find_commit(base_oid).is_ok()
    {
        let local_patch = crate::patch_id::tree_patch_id(
            git.repo(),
            git.tree_of(layer.parent)?,
            git.tree_of(layer.commit)?,
        )?;
        let pr_patch = displayed_patch_id(git.repo(), base_oid, head_oid)?;
        if local_patch == pr_patch {
            return Ok(());
        }
    }

    bail!(
        "#{number} has already been merged on GitHub, but the local commit has \
         additional changes that are not on `{trunk_name}`. Remove its \
         `Pull-Request:` trailer to submit them as a new pull request, or run \
         `nspr sync`."
    );
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
        if pr.head_oid != Oid::ZERO_SHA1 {
            let _ = forge.fetch_commit(pr.head_oid).await;
        }
        if let Some(mc) = pr.merge_commit {
            let _ = forge.fetch_commit(mc).await;
        }
        return Ok(pr);
    }
    if let Some(recorded_oid) = crate::refs::get(git, number)
        && pr.head_oid != recorded_oid
        && forge.branch_oid(&pr.head).await? == Some(recorded_oid)
    {
        if wait_for_pr_sync {
            for delay_ms in [50_u64, 150, 300, 600, 1000, 1500, 2000, 2000] {
                log::debug!(
                    "waiting {delay_ms}ms for #{number} headRefOid ({}) to catch up to pushed branch tip ({recorded_oid})",
                    pr.head_oid
                );
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
    initial_message: Option<&str>,
    attribution_commit: Option<Oid>,
) -> Result<Option<Oid>> {
    let repo = git.repo();
    let override_author = match attribution_commit {
        Some(attr_oid) => Some(repo.find_commit(attr_oid)?),
        None => None,
    };
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
            match initial_message {
                Some(m) => m.to_string(),
                None => {
                    String::from_utf8_lossy(commit.message_bytes()).into_owned()
                }
            }
        } else {
            String::from_utf8_lossy(commit.message_bytes()).into_owned()
        };

        let old_author = commit.author();
        let author = match &override_author {
            Some(attr)
                if old_author.name_bytes() != attr.author().name_bytes()
                    || old_author.email_bytes()
                        != attr.author().email_bytes() =>
            {
                attr.author()
            }
            _ => old_author,
        };

        tip = repo.commit(
            None,
            &author,
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
    let old_base = git.merge_base(old_root, d.tip)?;
    let index = git.merge_trees(
        git.tree_of(old_base)?,
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
