//! `nspr patch`: reconstruct a pull request stack locally from GitHub.
//!
//! Because `nspr` maintains invariant I1 (`tree(H_i^tip) == eff_tree(i)`), every
//! pull request head branch already contains the full effective tree of that
//! layer. `patch` walks down from the target pull request through parent base
//! branches to the trunk, then commits those trees in order into a fresh local
//! branch with `Pull-Request:` trailers intact.

use std::collections::HashSet;

use color_eyre::eyre::Result;
use git2::Oid;
use log::debug;

use crate::config::Config;
use crate::forge::{Forge, PrState, PullRequest};
use crate::git::Git;
use crate::trailers::{CommitMessage, PULL_REQUEST};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchOutcome {
    /// Name of the branch created or updated.
    pub branch: String,
    /// New commit OIDs constructed for the stack, bottom to top.
    pub commits: Vec<Oid>,
    /// PR numbers in the stack, bottom to top.
    pub prs: Vec<u64>,
    /// State of the target pull request.
    pub target_state: PrState,
    /// Whether the branch was checked out into the working copy.
    pub checked_out: bool,
}

/// Fetch a pull request and its dependency stack, reconstructing a linear
/// branch of commits locally.
pub async fn patch_layer(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    trunk_oid: Oid,
    number: u64,
    branch_override: Option<&str>,
    no_checkout: bool,
) -> Result<PatchOutcome> {
    if !no_checkout {
        git.check_no_uncommitted_changes()?;
    }

    let target_pr = forge.get_pull_request(number).await?;
    let target_state = target_pr.state;

    let mut chain: Vec<PullRequest> = vec![target_pr.clone()];
    let mut seen = HashSet::new();
    seen.insert(number);

    let mut current = target_pr;
    while current.base != config.trunk {
        match forge.find_pull_request_by_head(&current.base).await? {
            Some(parent) => {
                if !seen.insert(parent.number) {
                    debug!(
                        "cycle detected while traversing PR bases at #{}",
                        parent.number
                    );
                    break;
                }
                current = parent.clone();
                chain.push(parent);
            }
            None => {
                // Base is not an open or recognizable PR head branch.
                break;
            }
        }
    }

    chain.reverse();

    // Ensure all commits in the chain are available locally.
    for pr in &chain {
        forge.fetch_commit(pr.head_oid).await?;
        forge.fetch_commit(pr.base_oid).await?;
    }

    // Determine the base commit to build upon.
    let bottom_pr = &chain[0];
    let mut parent = if bottom_pr.base == config.trunk {
        if git
            .is_ancestor(bottom_pr.base_oid, trunk_oid)
            .unwrap_or(false)
        {
            bottom_pr.base_oid
        } else {
            git.merge_base(bottom_pr.base_oid, trunk_oid)
                .unwrap_or(trunk_oid)
        }
    } else {
        bottom_pr.base_oid
    };

    let mut commits = Vec::new();
    let mut prs = Vec::new();

    for pr in &chain {
        let pr_tree = git.tree_of(pr.head_oid)?;
        let parent_tree = git.tree_of(parent)?;

        if pr_tree == parent_tree {
            // Commit is empty against this parent, skip.
            continue;
        }

        let mut msg =
            CommitMessage::parse(&format!("{}\n\n{}", pr.title, pr.body));
        let pr_url = format!(
            "https://github.com/{}/{}/pull/{}",
            config.owner, config.repo, pr.number
        );
        msg.set(PULL_REQUEST, &pr_url);

        let commit_oid = git.create_derived_commit(
            pr.head_oid,
            &msg.render(),
            pr_tree,
            &[parent],
        )?;

        commits.push(commit_oid);
        prs.push(pr.number);
        parent = commit_oid;
    }

    let branch_name = branch_override
        .map(str::to_string)
        .unwrap_or_else(|| format!("pr/{number}"));

    let branch_ref = format!("refs/heads/{branch_name}");
    git.set_reference(&branch_ref, parent, "nspr: patch")?;

    let checked_out = if !no_checkout {
        git.checkout_branch(&branch_name)?;
        true
    } else {
        false
    };

    Ok(PatchOutcome {
        branch: branch_name,
        commits,
        prs,
        target_state,
        checked_out,
    })
}
