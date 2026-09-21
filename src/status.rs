//! `nspr status`: what the stack looks like, and what `nspr diff` would do.
//!
//! The staleness column is not computed here. It comes from running the real
//! [`crate::engine`] passes and stopping short of executing them, so `status`
//! can never disagree with what `diff` actually does. A reimplementation would
//! be a second copy of the subtlest logic in the codebase, free to drift.

use color_eyre::eyre::Result;

use crate::config::Config;
use crate::engine::{self, SyncOptions};
use crate::forge::{Forge, MergeState, Mergeable, PrState};
use crate::git::Git;
use crate::stack::{Dep, Stack};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerState {
    /// No pull request yet.
    New,
    /// Up to date on the forge.
    Current,
    /// The local commit says something different from what reviewers see.
    Modified,
    /// The patch is unchanged, but the branch must move anyway — usually
    /// because something it depends on did.
    NeedsRestack,
    /// Created by `spr`; needs `nspr upgrade` to convert to native stacking.
    LegacySpr,
    Merged,
    Closed,
}

impl LayerState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Current => "ok",
            Self::Modified => "modified",
            Self::NeedsRestack => "restack",
            Self::LegacySpr => "spr (run `nspr upgrade`)",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayerStatus {
    pub index: usize,
    pub subject: String,
    pub number: Option<u64>,
    pub url: Option<String>,
    pub branch: Option<String>,
    /// The base branch on the forge right now, which may not be the base the
    /// local stack implies.
    pub base: Option<String>,
    /// What the base *should* be.
    pub wanted_base: String,
    pub state: LayerState,
    pub draft: bool,
    pub auto_merge: bool,
    pub conflicting: bool,
    pub behind: bool,
    /// Ready to land: depends on nothing but the trunk.
    pub landable: bool,
    /// The layer this one is stacked on, if any.
    pub dep: Dep,
}

#[derive(Debug, Clone)]
pub struct StackStatus {
    pub trunk: String,
    pub layers: Vec<LayerStatus>,
}

pub async fn status(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
) -> Result<StackStatus> {
    let trees = stack.all_trees(git)?;
    let prs = engine::gather(forge, stack).await?;
    let merge_settings = forge.repo_merge_settings().await?;
    let opts = SyncOptions {
        preserve_commit_history: config
            .preserve_commit_history
            .resolve(merge_settings),
        ..Default::default()
    };
    let decision = engine::decide(git, stack, &prs, &trees, &opts)?;

    let mut layers = Vec::with_capacity(stack.layers.len());
    for (i, layer) in stack.layers.iter().enumerate() {
        let wanted_base = match layer.dep {
            Dep::Main | Dep::ExternalPr(_) => config.trunk.clone(),
            Dep::Layer(j) => stack.layers[j]
                .pr
                .and_then(|n| {
                    prs[j]
                        .as_ref()
                        .filter(|p| p.number == n)
                        .map(|p| p.head.clone())
                })
                .unwrap_or_else(|| "?".to_string()),
        };

        let state = match &prs[i] {
            None => LayerState::New,
            Some(pr) => match pr.state {
                PrState::Merged => LayerState::Merged,
                PrState::Closed => LayerState::Closed,
                PrState::Open
                    if crate::upgrade::is_spr_layer(
                        git, stack, i, pr, &prs, config,
                    )? =>
                {
                    LayerState::LegacySpr
                }
                PrState::Open if decision.patch_changed[i] => {
                    LayerState::Modified
                }
                PrState::Open if decision.push[i] => LayerState::NeedsRestack,
                PrState::Open => LayerState::Current,
            },
        };

        layers.push(LayerStatus {
            index: i,
            subject: layer.subject().to_string(),
            number: layer.pr,
            url: layer.pr.map(|n| config.pull_request_url(n)),
            branch: prs[i].as_ref().map(|p| p.head.clone()),
            base: prs[i].as_ref().map(|p| p.base.clone()),
            wanted_base,
            state,
            draft: prs[i].as_ref().is_some_and(|p| p.draft),
            auto_merge: prs[i].as_ref().is_some_and(|p| p.auto_merge),
            conflicting: prs[i]
                .as_ref()
                .is_some_and(|p| p.mergeable == Mergeable::Conflicting),
            behind: prs[i]
                .as_ref()
                .is_some_and(|p| p.merge_state == MergeState::Behind),
            landable: layer.dep == Dep::Main && layer.pr.is_some(),
            dep: layer.dep,
        });
    }

    Ok(StackStatus {
        trunk: config.trunk.clone(),
        layers,
    })
}

impl StackStatus {
    /// Render top-down, the way a stack is usually drawn, with the trunk at
    /// the bottom.
    pub fn render(&self) -> String {
        let mut out = String::new();

        for layer in self.layers.iter().rev() {
            let number = match layer.number {
                Some(n) => format!("#{n}"),
                None => "—".to_string(),
            };

            let mut notes = vec![layer.state.label().to_string()];
            if layer.draft {
                notes.push("draft".into());
            }
            if layer.conflicting {
                notes.push("conflicts".into());
            }
            if layer.behind {
                notes.push("behind".into());
            }
            if layer.auto_merge {
                // Worth shouting about: auto-merge on a stacked pull request
                // merges it into the layer below, not the trunk.
                notes.push("AUTO-MERGE".into());
            }
            if layer.base.as_ref().is_some_and(|b| b != &layer.wanted_base) {
                notes.push(format!("retarget→{}", layer.wanted_base));
            }
            if layer.landable {
                notes.push("landable".into());
            }

            out.push_str(&format!(
                "  {number:>6}  {:<10}  {}\n",
                notes.join(","),
                layer.subject,
            ));
        }

        out.push_str(&format!("  {:>6}  {:<10}  {}\n", "", "", self.trunk));
        out
    }
}
