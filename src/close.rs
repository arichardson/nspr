//! `nspr close`: abandon a layer without abandoning the ones above it.
//!
//! This is the one operation in nspr that cannot be made diff-neutral, and the
//! reason is worth stating plainly. Landing a layer preserves its changes: the
//! squash commit has the same tree, so the dependents' diffs are unaffected
//! and their review comments survive. Closing a layer *removes* its changes,
//! so every dependent's diff genuinely changes, and comments anchored to lines
//! that came from the closed layer will be outdated. There is no clever
//! encoding that avoids this; it is what the author asked for.
//!
//! So `close` warns, and otherwise does the minimum: it does **not** delete the
//! head branch. GitHub only lets you reopen a closed pull request while its
//! branch still exists, and someone who closes a layer by mistake should be
//! able to undo it.

use color_eyre::eyre::{Result, bail};

use crate::config::Config;
use crate::forge::{Forge, PrState, PullRequestUpdate};
use crate::git::Git;
use crate::stack::{Dep, DepSpec, Stack};
use crate::trailers::{CommitMessage, DEPENDS_ON};

#[derive(Debug, Clone)]
pub struct CloseOutcome {
    pub number: u64,
    pub title: String,
    /// Dependents whose base branch was moved down to take its place.
    pub retargeted: Vec<u64>,
    pub warnings: Vec<String>,
}

/// Close layer `index`'s pull request and restack whatever depended on it.
///
/// The local commit is dropped. Pushing the restacked dependents is left to
/// the caller's usual sync, so that the decision of what to push stays in one
/// place.
pub async fn close_layer(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    index: usize,
) -> Result<CloseOutcome> {
    git.check_no_uncommitted_changes()?;
    crate::upgrade::reject_if_legacy_spr(git, forge, config, stack).await?;

    let layer = &stack.layers[index];
    let Some(number) = layer.pr else {
        bail!(
            "`{}` has no pull request, so there is nothing to close. Drop the \
             commit with `git rebase -i` instead.",
            layer.subject()
        );
    };

    let pr = forge.get_pull_request(number).await?;
    match pr.state {
        PrState::Open => {}
        PrState::Closed => bail!("#{number} is already closed."),
        PrState::Merged => bail!(
            "#{number} has already been merged. Run `nspr sync` to bring your \
             local stack back in line."
        ),
    }

    // Whatever this layer sat on is what its dependents will sit on.
    let (inherited_base, inherited_spec) = match layer.dep {
        Dep::Main => (config.trunk.clone(), config.trunk.clone()),
        Dep::Layer(j) => {
            let below = &stack.layers[j];
            let Some(below_pr) = below.pr else {
                bail!(
                    "`{}` depends on `{}`, which has no pull request yet. Run \
                     `nspr diff` first so there is something to reparent onto.",
                    stack.layers[index].subject(),
                    below.subject()
                );
            };
            let below_pr = forge.get_pull_request(below_pr).await?;
            (below_pr.head.clone(), format!("#{}", below_pr.number))
        }
        Dep::ExternalPr(n) => bail!(
            "`{}` declares `{DEPENDS_ON}: #{n}`, which is not resolved. Run \
             `nspr diff` first.",
            layer.subject()
        ),
    };

    let dependents: Vec<usize> = (0..stack.layers.len())
        .filter(|&i| stack.layers[i].dep == Dep::Layer(index))
        .collect();

    // Retarget before closing, for the same reason `land` does: a pull request
    // whose base branch disappears out from under it can be closed by GitHub.
    let mut retargeted = Vec::new();
    for &i in &dependents {
        let Some(dep_pr) = stack.layers[i].pr else {
            continue;
        };
        forge
            .update_pull_request(
                dep_pr,
                PullRequestUpdate {
                    base: Some(inherited_base.clone()),
                    ..Default::default()
                },
            )
            .await?;
        retargeted.push(dep_pr);
    }

    forge
        .update_pull_request(
            number,
            PullRequestUpdate {
                state: Some(PrState::Closed),
                ..Default::default()
            },
        )
        .await?;

    // Rewrite any trailer that named the closed pull request, otherwise the
    // next `nspr diff` would try to resolve a reference to something closed
    // and refuse to do anything at all.
    let mut messages: Vec<CommitMessage> = stack
        .layers
        .iter()
        .map(|l| CommitMessage::parse(&git.message_of(l.commit).unwrap()))
        .collect();
    for &i in &dependents {
        if stack.layers[i].dep_spec == Some(DepSpec::Pr(number)) {
            messages[i].set(DEPENDS_ON, &inherited_spec);
        }
    }

    let rewrites: Vec<(git2::Oid, String)> = stack
        .layers
        .iter()
        .zip(&messages)
        .map(|(l, m)| (l.commit, m.render()))
        .collect();
    let rewritten = git.rewrite_messages(stack.base, &rewrites)?;

    // Drop the closed layer from the local chain.
    let keep: Vec<git2::Oid> = rewritten
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, oid)| *oid)
        .collect();
    git.rebase_commits(&keep, stack.base)?;

    crate::refs::remove(git, number)?;

    let mut warnings = Vec::new();
    if !retargeted.is_empty() {
        let list: Vec<String> =
            retargeted.iter().map(|n| format!("#{n}")).collect();
        warnings.push(format!(
            "{} no longer contain #{number}'s changes, so their diffs have \
             changed and review comments on those lines will be marked \
             outdated. This is unavoidable when a layer is abandoned rather \
             than landed.",
            list.join(", ")
        ));
    }
    warnings.push(format!(
        "#{number}'s branch `{}` was left in place so the pull request can \
         still be reopened. Delete it yourself once you are sure.",
        pr.head
    ));

    Ok(CloseOutcome {
        number,
        title: pr.title,
        retargeted,
        warnings,
    })
}
