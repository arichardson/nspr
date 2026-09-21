//! Warnings about repository settings that interact badly with stacking.
//!
//! None of these are `nspr` bugs and none of them can be worked around from
//! here — they are choices the repository's admins made, whose consequences
//! are easy to miss when your pull requests are stacked. The job is to say so
//! before you find out the expensive way.

use color_eyre::eyre::Result;

use crate::config::{Config, PreserveCommitHistory};
use crate::forge::{Forge, PullRequest, RepoMergeSettings};
use crate::stack::Stack;

#[derive(Debug, Clone, Default)]
pub struct Guardrails {
    pub warnings: Vec<String>,
    /// The trunk requires branches to be up to date before merging, so a
    /// "behind" layer really is unmergeable and is worth refreshing even
    /// though its displayed diff is fine.
    pub refresh_when_behind: bool,
}

fn describe_non_squash_reasons(s: RepoMergeSettings) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if !s.allow_squash_merge {
        reasons.push(
            "Squash and Merge is disabled (`allow_squash_merge = false`)",
        );
    }
    if s.allow_merge_commit {
        reasons.push("merge commits are enabled (`allow_merge_commit = true`)");
    }
    if s.allow_rebase_merge {
        reasons.push("rebase merges are enabled (`allow_rebase_merge = true`)");
    }
    if !s.squash_uses_pr_description {
        reasons.push(
            "default squash commit message is `COMMIT_MESSAGES` instead of `PR_TITLE` + `PR_BODY`",
        );
    }
    reasons
}

/// **G2.** Auto-merge on a stacked pull request merges it into the layer
/// below, not the trunk.
///
/// The result is one pull request silently swallowing another's changes,
/// which then shows up as an enormous unreviewed diff on whatever is left.
/// Easy to enable by reflex and very hard to spot afterwards.
pub fn auto_merge_warning(pr: &PullRequest, trunk: &str) -> Option<String> {
    (pr.auto_merge && pr.base != trunk).then(|| {
        format!(
            "#{} has auto-merge enabled. It will merge into `{}`, not `{}`, \
             flattening it into the layer below. Turn it off unless that is \
             what you want.",
            pr.number, pr.base, trunk,
        )
    })
}

/// Probe the repository's settings and report anything that will bite.
///
/// `pushing` marks the layers about to be pushed, so the approval warning is
/// only raised for work that is actually going to be disturbed.
pub async fn probe(
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    prs: &[Option<PullRequest>],
    pushing: &[bool],
) -> Result<Guardrails> {
    let mut warnings = Vec::new();

    let merge_settings = forge.repo_merge_settings().await?;
    let reasons = describe_non_squash_reasons(merge_settings);
    if !reasons.is_empty() {
        let joined = reasons.join("; ");
        match config.preserve_commit_history {
            PreserveCommitHistory::Auto => {
                warnings.push(format!(
                    "repository settings are not configured for multi-commit squash merging ({joined}).\n\
                     `nspr.preserveCommitHistory` is `auto`, so nspr is falling back to force-pushing a single commit per PR branch to prevent `[nspr]` revision commits from polluting repository history if merged via the GitHub Web UI.\n\
                     To configure this behavior or silence this warning:\n\
                       - Run `git config nspr.preserveCommitHistory false` to always force-push a single commit per PR without this warning.\n\
                       - Run `git config nspr.preserveCommitHistory true` to push incremental `[nspr]` commits anyway (merge via `nspr land`, or manually select \"Squash and merge\" with the PR title/description in the Web UI).\n\
                       - Or in GitHub Settings -> General -> Pull Requests, enable only \"Allow squash merging\" with default commit message \"Pull request title and description\"."
                ));
            }
            PreserveCommitHistory::True => {
                warnings.push(format!(
                    "`nspr.preserveCommitHistory` is set to `true`, but repository settings are not configured for clean Web UI squash merging ({joined}). \
                     Merge PRs using `nspr land`, or ensure you select \"Squash and merge\" and use the PR title and description when merging via the GitHub Web UI."
                ));
            }
            PreserveCommitHistory::False => {}
        }
    }

    let refresh_when_behind = forge
        .branch_protection(&config.trunk)
        .await?
        .is_some_and(|p| p.require_up_to_date);

    for (i, pr) in prs.iter().enumerate() {
        let Some(pr) = pr else { continue };

        if let Some(w) = auto_merge_warning(pr, &config.trunk) {
            warnings.push(w);
        }

        if !pushing.get(i).copied().unwrap_or(false) {
            continue;
        }

        // **G1.** `dismiss_stale_reviews` is a property of the branch being
        // merged *into*, so in a stack it usually only bites the bottom layer:
        // the layers above target other people's topic branches, which are
        // rarely protected. Checking each layer's actual base rather than just
        // the trunk means we neither cry wolf nor stay quiet when an
        // organisation protects `users/**` too.
        let dismisses = forge
            .branch_protection(&pr.base)
            .await?
            .is_some_and(|p| p.dismiss_stale_reviews);
        if dismisses {
            warnings.push(format!(
                "pushing to #{} will dismiss its approvals, because `{}` is \
                 set to dismiss stale reviews.",
                pr.number, pr.base,
            ));
        }
    }

    if refresh_when_behind && stack.layers.len() > 1 {
        warnings.push(format!(
            "`{}` requires branches to be up to date before merging, so every \
             layer must be refreshed whenever one below it changes. Expect the \
             whole stack to re-run CI on each push.",
            config.trunk,
        ));
    }

    Ok(Guardrails {
        warnings,
        refresh_when_behind,
    })
}
