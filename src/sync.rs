//! `nspr sync`: bring the local stack in line with the remote trunk.
//!
//! This is the command you run when the world moved underneath you — somebody
//! else landed something, or one of your own pull requests was merged or
//! closed through the GitHub UI rather than through `nspr land`.
//!
//! # Guardrail G3: merges detected by state, never by sha
//!
//! A squash merge puts a **brand new commit** on the trunk. The local commit's
//! id appears nowhere in the trunk's history, so "is my commit an ancestor of
//! the trunk?" answers *no* for a layer that has very much landed. The only
//! reliable signal is the pull request's own state, which is what this module
//! reads.
//!
//! What follows is ordinary: rebasing the stack onto the new trunk makes the
//! landed layer's cherry-pick empty, so it drops out on its own.
//!
//! # Repair is not this module's job
//!
//! Once the local stack is rebased, the surviving layers' branches are stale —
//! their bases point at branches that may be gone, and their diffs would show
//! the landed changes. [`crate::engine::sync_stack`] fixes that on the next
//! run, and does it with *appends* rather than force-pushes, because the
//! declared-tree trick works just as well against a squash commit as against
//! anything else. So `nspr sync` is `sync_trunk` followed by `sync_stack`.

use color_eyre::eyre::{Result, WrapErr as _, eyre};
use git2::Oid;
use log::debug;

use crate::config::Config;
use crate::forge::{Forge, PrState};
use crate::git::Git;
use crate::stack::Stack;
use crate::trailers::PULL_REQUEST;

#[derive(Debug, Clone)]
pub struct SyncReport {
    /// The trunk tip we rebased onto.
    pub trunk: Oid,
    /// Pull requests that were merged without `nspr land`.
    pub merged: Vec<u64>,
    /// Layers whose local commit survived the rebase even though their pull
    /// request had been merged — the author amended them after the merge.
    pub stranded: Vec<u64>,
    pub warnings: Vec<String>,
    /// True if the local stack moved.
    pub rebased: bool,
}

/// Where the trunk is, according to the remote rather than to local memory.
///
/// `refs/remotes/<remote>/<trunk>` is not good enough. nspr fetches objects by
/// id and never by refspec, so it never advances that ref itself, leaving it
/// only as fresh as the user's last manual `git fetch` — and after a land it is
/// certainly stale. A stale trunk keeps the just-merged layer in the stack and,
/// far worse, makes the next `diff` offer to retarget the layer above it back
/// onto the branch that was merged away, undoing the repair `land` performed.
///
/// The ref is moved to match, because having just confirmed what it is supposed
/// to say, leaving it behind would make `git log` disagree with everything nspr
/// prints.
pub async fn resolve_trunk(
    git: &Git,
    forge: &dyn Forge,
    remote: &str,
    trunk: &str,
) -> Result<Oid> {
    let remote_oid = forge.branch_oid(trunk).await;
    resolve_trunk_from_remote_oid(git, forge, remote, trunk, remote_oid).await
}

/// Like [`resolve_trunk`], using a pre-fetched `branch_oid(trunk)` result so
/// callers that combined the branch lookup with another GraphQL query do not
/// issue a second round-trip.
pub async fn resolve_trunk_from_remote_oid(
    git: &Git,
    forge: &dyn Forge,
    remote: &str,
    trunk: &str,
    remote_oid: Result<Option<Oid>>,
) -> Result<Oid> {
    let trunk_ref = format!("refs/remotes/{remote}/{trunk}");

    match remote_oid {
        Ok(Some(oid)) => {
            debug!("resolved remote trunk `{remote}/{trunk}` = {oid}");
            forge.fetch_commit(oid).await?;
            git.set_reference(&trunk_ref, oid, "nspr: observed trunk")?;
            Ok(oid)
        }
        // Offline, or the remote has no such branch. The last thing we saw
        // beats refusing to run, and every command re-checks anyway.
        other => {
            if let Err(error) = other {
                debug!("could not read {trunk} from the remote: {error:#}");
            }
            debug!(
                "remote trunk `{trunk}` unavailable; falling back to local tracking ref `{trunk_ref}`"
            );
            git.resolve_reference(&trunk_ref).map_err(|_| {
                eyre!(
                    "cannot tell where `{trunk}` is: the remote did not \
                     answer, and there is no `{remote}/{trunk}` locally. Run \
                     `git fetch {remote}` first."
                )
            })
        }
    }
}

/// Fetch the trunk, report anything that changed behind our back, and rebase
/// the local stack onto it.
///
/// The caller must re-discover the stack afterwards; commit ids have changed
/// and landed layers are gone.
pub async fn sync_trunk(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
) -> Result<SyncReport> {
    git.check_no_uncommitted_changes()?;
    crate::upgrade::reject_if_legacy_spr(git, forge, config, stack).await?;

    let trunk = forge
        .branch_oid(&config.trunk)
        .await?
        .ok_or_else(|| eyre!("no `{}` branch on the remote", config.trunk))?;
    forge.fetch_commit(trunk).await?;

    let mut merged = Vec::new();
    let mut cleanly_merged = std::collections::HashSet::new();
    let mut warnings = Vec::new();
    let trunk_tree = git.tree_of(trunk)?;

    let mut needed_indices = Vec::new();
    let mut needed_numbers = Vec::new();
    for (i, layer) in stack.layers.iter().enumerate() {
        if let Some(number) = layer.pr {
            needed_indices.push(i);
            needed_numbers.push(number);
        }
    }
    let prs = forge.get_pull_requests(&needed_numbers).await?;

    for (i, pr) in needed_indices.into_iter().zip(prs) {
        let layer = &stack.layers[i];
        let number = pr.number;
        match pr.state {
            PrState::Merged => {
                merged.push(number);
                if let Ok(idx) = git.cherrypick(layer.commit, trunk)
                    && !idx.has_conflicts()
                    && git.write_index(idx).ok() == Some(trunk_tree)
                {
                    cleanly_merged.insert(i);
                }
            }
            PrState::Closed => warnings.push(format!(
                "#{number} (`{}`) was closed without merging. Delete the \
                 commit, or remove its `{PULL_REQUEST}:` trailer to open a \
                 fresh pull request.",
                layer.subject(),
            )),
            PrState::Open => {
                let mut landed_oid = crate::land::find_landed_commit_on_trunk(
                    git.repo(),
                    trunk,
                    Oid::ZERO_SHA1,
                    number,
                )?;
                let patch_on_trunk = git
                    .cherrypick(layer.commit, trunk)
                    .ok()
                    .filter(|idx| !idx.has_conflicts())
                    .and_then(|idx| git.write_index(idx).ok())
                    == Some(trunk_tree);
                if landed_oid.is_none() && patch_on_trunk && trunk != stack.base
                {
                    landed_oid = crate::land::find_matching_patch_on_trunk(
                        git, layer, trunk, stack.base,
                    )?;
                }
                if let Some(landed_oid) = landed_oid {
                    merged.push(number);
                    if patch_on_trunk {
                        cleanly_merged.insert(i);
                        let _ = forge
                            .update_pull_request(
                                number,
                                crate::forge::PullRequestUpdate {
                                    state: Some(PrState::Closed),
                                    ..Default::default()
                                },
                            )
                            .await;
                        warnings.push(format!(
                            "#{number} was still open on GitHub, but its \
                             changes were already on `{}` ({}); closed \
                             #{number}.",
                            config.trunk,
                            git.short_id(landed_oid)?,
                        ));
                    }
                }
            }
        }
    }

    let rebased = trunk != stack.base;
    if rebased {
        let commits =
            stack.rewrite_deps_for_removal(git, &cleanly_merged, false)?;
        git.rebase_commits(&commits, trunk).wrap_err(
            "could not rebase the stack onto the trunk. Resolve the conflict \
             with `git rebase` and run `nspr sync` again.",
        )?;
    }

    // A merged layer whose commit is still here after the rebase was amended
    // after it merged, so the amendment never made it to the trunk. Say so
    // loudly: this is the one case where `sync` cannot do the right thing on
    // its own.
    let survivors = Stack::discover(git, trunk, &config.trunk)?;
    let stranded: Vec<u64> = survivors
        .layers
        .iter()
        .filter_map(|l| l.pr)
        .filter(|n| merged.contains(n))
        .collect();
    for number in &stranded {
        warnings.push(format!(
            "#{number} was merged, but its commit still has changes that are \
             not on the trunk. Remove its `{PULL_REQUEST}:` trailer to submit \
             them as a new pull request, or drop the commit."
        ));
    }

    Ok(SyncReport {
        trunk,
        merged,
        stranded,
        warnings,
        rebased,
    })
}
