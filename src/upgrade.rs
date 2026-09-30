//! Upgrading legacy `spr` pull requests to GitHub-native `nspr` stacked pull
//! requests.
//!
//! `spr` (`spacedentist/spr` and `ejoffe/spr`) creates pull requests with:
//! - A `Pull Request: <url>` trailer (with a space) in the local commit message.
//! - Commits on the remote PR branch whose commit messages start with `[spr]`
//!   (such as `[spr] initial version`).
//! - For stacked commits above the bottom layer, a synthetic base branch on
//!   GitHub (such as `spr/main/master.<id>`) rather than pointing the PR's base
//!   at the parent PR's head branch.
//!
//! [`upgrade_stack`] detects these pull requests and converts them in-place to
//! native `nspr` stacked pull requests while preserving their pull request
//! numbers, review comments, and approvals:
//! - Rewrites each PR's head branch onto its parent PR's head branch tip using
//!   the commit's real message instead of `[spr] initial version`.
//! - Retargets each PR's base branch on GitHub to the parent PR's head branch
//!   (or `main` for root layers).
//! - Deletes `spr`'s orphaned synthetic base branches from the remote.
//! - Normalizes `Pull Request:` trailers in local commits to `Pull-Request:`.

use std::collections::HashSet;

use color_eyre::eyre::{Result, bail};
use git2::Oid;

use crate::config::Config;
use crate::forge::{Forge, PullRequest, PullRequestUpdate, PushSpec};
use crate::git::Git;
use crate::stack::{Dep, Stack};
use crate::trailers::{CommitMessage, DEPENDS_ON, PULL_REQUEST};

#[derive(Debug, Clone)]
pub struct UpgradedLayer {
    pub index: usize,
    pub number: u64,
    pub branch: String,
    pub old_base: String,
    pub new_base: String,
    pub deleted_synthetic_base: Option<String>,
    pub tip: Oid,
}

/// True if any commit on `head_oid` down to `stop_oid` carries the tell-tale
/// `[spr]` commit message prefix (`[spr] initial version`, `[spr] updated`, etc.).
pub fn has_spr_commit(git: &Git, head_oid: Oid, stop_oid: Oid) -> Result<bool> {
    let repo = git.repo();
    if repo.find_commit(head_oid).is_err() {
        return Ok(false);
    }
    let mut cur = head_oid;
    for _ in 0..64 {
        if cur == stop_oid || git.is_ancestor(cur, stop_oid)? {
            break;
        }
        let msg = git.message_of(cur)?;
        let trimmed = msg.trim_start();
        if trimmed.starts_with("[spr]")
            || trimmed.starts_with("[\u{1d5ee}\u{1d5ed}\u{1d5ff}]")
            || msg.contains("Created using spr")
        {
            return Ok(true);
        }
        let commit = repo.find_commit(cur)?;
        match commit.parent_id(0) {
            Ok(parent) => cur = parent,
            Err(_) => break,
        }
    }
    Ok(false)
}

/// True if layer `i` (and its open pull request `pr`) was created by `spr` and
/// has not yet been upgraded to `nspr`.
pub fn is_spr_layer(
    git: &Git,
    stack: &Stack,
    i: usize,
    pr: &PullRequest,
    prs: &[Option<PullRequest>],
    config: &Config,
) -> Result<bool> {
    if stack.layers[i].message.has_legacy_spr_trailer() {
        return Ok(true);
    }
    if has_spr_commit(git, pr.head_oid, stack.base)? {
        return Ok(true);
    }
    let expected_base = match stack.layers[i].dep {
        Dep::Main | Dep::ExternalPr(_) => config.trunk.as_str(),
        Dep::Layer(j) => prs[j]
            .as_ref()
            .map(|p| p.head.as_str())
            .unwrap_or(config.trunk.as_str()),
    };
    if (pr.base.starts_with("spr/") || pr.base.contains("/spr/"))
        && pr.base != expected_base
    {
        return Ok(true);
    }
    Ok(false)
}

/// Refuse to mutate a stack that contains un-upgraded `spr` pull requests,
/// ensuring `spr -> nspr` migration only ever happens when the user explicitly
/// runs `nspr upgrade`.
pub async fn reject_if_legacy_spr(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
) -> Result<()> {
    let prs = crate::engine::gather(forge, stack).await?;
    reject_if_legacy_spr_with_prs(git, config, stack, &prs, None, None)
}

pub fn reject_if_legacy_spr_with_prs(
    git: &Git,
    config: &Config,
    stack: &Stack,
    prs: &[Option<PullRequest>],
    only_layer: Option<usize>,
    only_layers: Option<&HashSet<usize>>,
) -> Result<()> {
    for (i, layer) in stack.layers.iter().enumerate() {
        if !Stack::is_layer_selected(i, only_layer, only_layers) {
            continue;
        }
        let is_spr = match &prs[i] {
            Some(pr) => is_spr_layer(git, stack, i, pr, prs, config)?,
            None => layer.message.has_legacy_spr_trailer(),
        };
        if is_spr {
            let pr_label = match layer.pr {
                Some(n) => format!("#{n} (`{}`)", layer.subject()),
                None => format!("`{}`", layer.subject()),
            };
            bail!(
                "pull request {pr_label} was created by `spr` (detected `[spr]` commit or `Pull Request:` trailer).\n\
                 `nspr` will not modify `spr` pull requests automatically. Run `nspr upgrade` to convert them to native stacked pull requests."
            );
        }
    }
    Ok(())
}

/// Convert all `spr` pull requests in `stack` into native `nspr` stacked pull
/// requests.
pub async fn upgrade_stack(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &mut Stack,
) -> Result<Vec<UpgradedLayer>> {
    git.check_no_uncommitted_changes()?;

    let trees = stack.all_trees(git)?;
    let prs = crate::engine::gather(forge, stack).await?;
    crate::engine::reject_unusable(&prs)?;

    let mut needs_upgrade = false;
    for (i, layer) in stack.layers.iter().enumerate() {
        if layer.message.has_legacy_spr_trailer() {
            needs_upgrade = true;
            break;
        }
        if let Some(pr) = &prs[i]
            && is_spr_layer(git, stack, i, pr, &prs, config)?
        {
            needs_upgrade = true;
            break;
        }
    }

    if !needs_upgrade {
        return Ok(Vec::new());
    }

    let n = stack.layers.len();
    let head_branches: HashSet<String> =
        prs.iter().flatten().map(|p| p.head.clone()).collect();

    let mut upgraded = Vec::new();
    let mut pass1_tips: Vec<Oid> = Vec::with_capacity(n);
    let mut final_tips: Vec<Oid> = Vec::with_capacity(n);
    let mut branches: Vec<String> = Vec::with_capacity(n);
    let mut base_branches: Vec<String> = Vec::with_capacity(n);
    let mut retargeted_early: Vec<bool> = vec![false; n];
    let mut undraft: Vec<String> = Vec::new();
    let mut messages: Vec<CommitMessage> =
        stack.layers.iter().map(|l| l.message.clone()).collect();
    let mut pass1_pushes: Vec<PushSpec> = Vec::new();
    let mut pass2_pushes: Vec<PushSpec> = Vec::new();

    for (i, pr) in prs.iter().enumerate() {
        let (pass1_parent_tip, final_parent_tip, base_branch) = match stack
            .layers[i]
            .dep
        {
            Dep::Main | Dep::ExternalPr(_) => {
                (stack.base, stack.base, config.trunk.clone())
            }
            Dep::Layer(j) => {
                if branches[j].is_empty() {
                    bail!(
                        "layer `{}` depends on unsubmitted commit `{}`; run `nspr diff` after upgrading or submit the lower commit first.",
                        stack.layers[i].subject(),
                        stack.layers[j].subject(),
                    );
                }
                (pass1_tips[j], final_tips[j], branches[j].clone())
            }
        };
        base_branches.push(base_branch.clone());

        let Some(pr) = pr else {
            pass1_tips.push(stack.base);
            final_tips.push(stack.base);
            branches.push(String::new());
            continue;
        };

        let fallback_base_tip = match stack.layers[i].dep {
            Dep::Main | Dep::ExternalPr(_) => stack.base,
            Dep::Layer(j) => {
                prs[j].as_ref().map(|p| p.head_oid).unwrap_or(stack.base)
            }
        };
        let current_pr_base_tip = if pr.base_oid != Oid::ZERO_SHA1 {
            pr.base_oid
        } else {
            fallback_base_tip
        };

        // When retargeting to `trunk` (for example, a root `spacedentist/spr`
        // pull request whose synthetic base branch `users/<login>/spr/main.<slug>`
        // sits on an older `main` commit `M_0` while `main` has advanced to
        // `M_1`), if `merge_base(old_base, pr.head) == merge_base(main, pr.head)`
        // (both equal `M_0`), retargeting `pr.base` to `main` *before* pushing
        // is 100% diff-neutral and allows pushing directly to `final_tip` in a
        // single pass.
        let retargeted_before_push = if pr.base != base_branch
            && matches!(stack.layers[i].dep, Dep::Main | Dep::ExternalPr(_))
            && git.merge_base(current_pr_base_tip, pr.head_oid)?
                == git.merge_base(stack.base, pr.head_oid)?
        {
            if config.draft_while_retargeting && !pr.draft {
                forge.set_draft(&pr.node_id, true).await?;
                undraft.push(pr.node_id.clone());
            }
            forge
                .update_pull_request(
                    pr.number,
                    PullRequestUpdate {
                        base: Some(base_branch.clone()),
                        ..Default::default()
                    },
                )
                .await?;
            true
        } else {
            false
        };
        retargeted_early[i] = retargeted_before_push;

        // Re-anchoring and retargeting cannot happen atomically: if `pr.head`
        // were pushed onto `new_base_tip` while `pr.base` on GitHub still
        // pointed at an older `spr` synthetic base branch (as happened on
        // llvm/llvm-project#203599), GitHub would compute the three-dot diff
        // across every upstream `main` commit between the old synthetic base
        // and the new `main`, immediately subscribing all `CODEOWNERS` across
        // the repository. Park `pr.head` at `merge_base(old_base, new_base)`
        // first, retarget `pr.base`, and only then advance `pr.head` to
        // `final_tip`.
        let effective_old_base_tip = if let Some((_, pushed_tip)) = branches
            .iter()
            .zip(&pass1_tips)
            .find(|(b, _)| *b == &pr.base)
        {
            git.merge_base(current_pr_base_tip, *pushed_tip)?
        } else {
            current_pr_base_tip
        };
        let raw_anchor = if retargeted_before_push {
            pass1_parent_tip
        } else {
            crate::engine::safe_retarget_anchor(
                git,
                pr,
                &base_branch,
                effective_old_base_tip,
                pass1_parent_tip,
            )?
            .unwrap_or(pass1_parent_tip)
        };

        let clean_msg = stack.layers[i].message.clean_for_branch();
        let final_tip = git.synthesize_initial_commit(
            final_parent_tip,
            trees.effective[i],
            stack.layers[i].commit,
            &clean_msg,
        )?;
        let pass1_tip = if raw_anchor == final_parent_tip {
            final_tip
        } else {
            let staged_tree = crate::engine::stage_tree_onto(
                git,
                trees.dep[i],
                raw_anchor,
                trees.effective[i],
            )?;
            git.synthesize_initial_commit(
                raw_anchor,
                staged_tree,
                stack.layers[i].commit,
                &clean_msg,
            )?
        };

        let pr_label = format!("#{}", pr.number);
        pass1_pushes
            .push(PushSpec::forced(&pr.head, pass1_tip).with_label(&pr_label));
        if pass1_tip != final_tip {
            pass2_pushes.push(
                PushSpec::forced(&pr.head, final_tip).with_label(&pr_label),
            );
        }
        pass1_tips.push(pass1_tip);
        final_tips.push(final_tip);
        branches.push(pr.head.clone());
    }

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

    if !pass1_pushes.is_empty() {
        if !pass2_pushes.is_empty() {
            pass1_pushes[0].context =
                Some("1/2, staging before retarget".to_string());
        }
        forge.push(&pass1_pushes).await?;
    }

    let merge_settings = forge.repo_merge_settings().await?;
    let preserve_commit_history =
        config.preserve_commit_history.resolve(merge_settings);
    let warn_merge_strategy =
        preserve_commit_history && !merge_settings.is_squash_only();
    for (i, pr) in prs.iter().enumerate() {
        let Some(pr) = pr else {
            continue;
        };
        let tip = final_tips[i];
        let base_branch = base_branches[i].clone();
        let subject = stack.layers[i].subject().to_string();
        let body = crate::pr_body::splice_warning(
            &stack.layers[i].message.clean_body_for_pr(),
            warn_merge_strategy,
        );
        let old_base = pr.base.clone();

        let mut update = PullRequestUpdate::default();
        if pr.title != subject {
            update.title = Some(subject);
        }
        if pr.body != body {
            update.body = Some(body);
        }
        if pr.base != base_branch && !retargeted_early[i] {
            update.base = Some(base_branch.clone());
        }
        if !update.is_empty() {
            forge.update_pull_request(pr.number, update).await?;
        }

        // Delete orphaned synthetic base branches created by `spr` (e.g.
        // `spr/main/master.<id>`) AFTER retargeting the pull request's base.
        let mut deleted_synthetic_base = None;
        if old_base != base_branch
            && old_base != config.trunk
            && !head_branches.contains(&old_base)
        {
            forge.delete_branch(&old_base).await?;
            deleted_synthetic_base = Some(old_base.clone());
        }

        crate::refs::update(git, pr.number, tip)?;
        crate::refs::update_root(git, pr.number, tip)?;

        // Normalize `Pull Request:` -> `Pull-Request:` in local commit message.
        messages[i].set(PULL_REQUEST, &config.pull_request_url(pr.number));
        if let Dep::Layer(j) = stack.layers[i].dep
            && stack.layers[i].dep_spec.is_some()
            && let Some(dep_pr) = stack.layers[j].pr
        {
            messages[i].set(DEPENDS_ON, &format!("#{dep_pr}"));
        }

        upgraded.push(UpgradedLayer {
            index: i,
            number: pr.number,
            branch: pr.head.clone(),
            old_base,
            new_base: base_branch,
            deleted_synthetic_base,
            tip,
        });
    }

    if !pass2_pushes.is_empty() {
        pass2_pushes[0].context =
            Some("2/2, restacking after retarget".to_string());
        forge.push(&pass2_pushes).await?;
    }

    for node_id in undraft {
        forge.set_draft(&node_id, false).await?;
    }

    crate::engine::apply_message_edits(git, stack, &messages)?;
    forge.sync_stacks(&stack.pr_chains()).await?;

    if config.stack_comments {
        crate::stack_comment::update_all(forge, config, stack).await?;
    }

    Ok(upgraded)
}
