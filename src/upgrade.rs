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
    reject_if_legacy_spr_with_prs(git, config, stack, &prs, None)
}

pub fn reject_if_legacy_spr_with_prs(
    git: &Git,
    config: &Config,
    stack: &Stack,
    prs: &[Option<PullRequest>],
    only_layer: Option<usize>,
) -> Result<()> {
    for (i, layer) in stack.layers.iter().enumerate() {
        if let Some(only) = only_layer
            && i != only
        {
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

    let head_branches: HashSet<String> =
        prs.iter().flatten().map(|p| p.head.clone()).collect();

    let mut upgraded = Vec::new();
    let mut tips: Vec<Oid> = Vec::with_capacity(stack.layers.len());
    let mut branches: Vec<String> = Vec::with_capacity(stack.layers.len());
    let mut base_branches: Vec<String> = Vec::with_capacity(stack.layers.len());
    let mut messages: Vec<CommitMessage> =
        stack.layers.iter().map(|l| l.message.clone()).collect();
    let mut head_pushes: Vec<PushSpec> = Vec::new();

    for (i, pr) in prs.iter().enumerate() {
        let (parent_tip, base_branch) = match stack.layers[i].dep {
            Dep::Main | Dep::ExternalPr(_) => {
                (stack.base, config.trunk.clone())
            }
            Dep::Layer(j) => {
                if branches[j].is_empty() {
                    bail!(
                        "layer `{}` depends on unsubmitted commit `{}`; run `nspr diff` after upgrading or submit the lower commit first.",
                        stack.layers[i].subject(),
                        stack.layers[j].subject(),
                    );
                }
                (tips[j], branches[j].clone())
            }
        };
        base_branches.push(base_branch);

        let Some(pr) = pr else {
            tips.push(stack.base);
            branches.push(String::new());
            continue;
        };

        let clean_msg = stack.layers[i].message.clean_for_branch();
        let tip = git.synthesize_initial_commit(
            parent_tip,
            trees.effective[i],
            stack.layers[i].commit,
            &clean_msg,
        )?;
        head_pushes.push(PushSpec::forced(&pr.head, tip));
        tips.push(tip);
        branches.push(pr.head.clone());
    }

    if !head_pushes.is_empty() {
        forge.push(&head_pushes).await?;
    }

    let mut delete_pushes: Vec<PushSpec> = Vec::new();
    for (i, pr) in prs.iter().enumerate() {
        let Some(pr) = pr else {
            continue;
        };
        let tip = tips[i];
        let base_branch = base_branches[i].clone();
        let subject = stack.layers[i].subject().to_string();
        let is_stacked = stack.is_layer_stacked(i);
        let body = crate::pr_body::splice_warning(
            &stack.layers[i].message.body,
            is_stacked,
        );
        let old_base = pr.base.clone();

        let mut update = PullRequestUpdate::default();
        if pr.title != subject {
            update.title = Some(subject);
        }
        if pr.body != body {
            update.body = Some(body);
        }
        if pr.base != base_branch {
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
            delete_pushes.push(PushSpec::delete(&old_base));
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

    if !delete_pushes.is_empty() {
        forge.push(&delete_pushes).await?;
    }

    crate::engine::apply_message_edits(git, stack, &messages)?;
    forge.sync_stacks(&stack.pr_chains()).await?;

    if config.stack_comments {
        crate::stack_comment::update_all(forge, config, stack).await?;
    }

    Ok(upgraded)
}
