//! `nspr` — GitHub-native stacked pull requests without force-pushes.
//!
//! # How it works
//!
//! The stack is the linear run of commits between `origin/main` and `HEAD`.
//! Each commit becomes one pull request, and each pull request's base branch is
//! the head branch of the layer below — GitHub-native stacking.
//!
//! Updates **append** a commit whose tree is declared outright (see
//! [`git::synthesis`]), so pushes are fast-forward: review comments keep their
//! anchors and reviewers get a genuine incremental diff. The one deliberate
//! exception is land-time cleanup, which is provably diff-neutral.
//!
//! [`review_diff`] reproduces GitHub's three-dot rendering locally, which is
//! both how the push gate decides whether work is needed and how the tests
//! verify correctness without a network.

pub mod amend;
pub mod auth;
pub mod close;
pub mod config;
pub mod engine;
pub mod forge;
pub mod git;
pub mod git_remote;
pub mod guardrails;
pub mod land;
pub mod list;
pub mod patch;
pub mod patch_id;
pub mod pr_body;
pub mod refs;
pub mod review_diff;
pub mod ssh_agent;
pub mod stack;
pub mod stack_comment;
pub mod status;
pub mod sync;
pub mod trailers;
pub mod upgrade;
pub mod utils;

#[cfg(test)]
mod scenarios;
#[cfg(test)]
mod testutil;
