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
    /// What the base *should* be (branch name on the forge).
    pub wanted_base: String,
    /// Short, human-readable label for `wanted_base` (e.g. `#225127` or `main`).
    pub wanted_base_label: String,
    pub state: LayerState,
    pub draft: bool,
    pub auto_merge: bool,
    pub conflicting: bool,
    pub behind: bool,
    pub message_differs: bool,
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
    from_parts(git, config, stack, &prs, &decision, false)
}

pub fn from_parts(
    git: &Git,
    config: &Config,
    stack: &Stack,
    prs: &[Option<crate::forge::PullRequest>],
    decision: &engine::Decision,
    update_message: bool,
) -> Result<StackStatus> {
    let mut layers = Vec::with_capacity(stack.layers.len());
    for (i, layer) in stack.layers.iter().enumerate() {
        let (wanted_base, wanted_base_label) = match layer.dep {
            Dep::Main => (config.trunk.clone(), config.trunk.clone()),
            Dep::ExternalPr(n) => (config.trunk.clone(), format!("#{n}")),
            Dep::Layer(j) => {
                let branch = stack.layers[j]
                    .pr
                    .and_then(|n| {
                        prs[j]
                            .as_ref()
                            .filter(|p| p.number == n)
                            .map(|p| p.head.clone())
                    })
                    .unwrap_or_else(|| "?".to_string());
                let label = match stack.layers[j].pr {
                    Some(n) => format!("#{n}"),
                    None => format!("layer {}", j + 1),
                };
                (branch, label)
            }
        };

        let message_differs = prs[i]
            .as_ref()
            .is_some_and(|p| engine::pr_message_differs_from(p, &layer.message));

        let state = match &prs[i] {
            None => LayerState::New,
            Some(pr) => match pr.state {
                PrState::Merged => LayerState::Merged,
                PrState::Closed => LayerState::Closed,
                PrState::Open
                    if crate::upgrade::is_spr_layer(
                        git, stack, i, pr, prs, config,
                    )? =>
                {
                    LayerState::LegacySpr
                }
                PrState::Open
                    if decision.patch_changed[i]
                        || (decision.message_changed[i]
                            && (!decision.github_message_edited[i]
                                || update_message))
                        || (update_message && message_differs) =>
                {
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
            wanted_base_label,
            state,
            draft: prs[i].as_ref().is_some_and(|p| p.draft),
            auto_merge: prs[i].as_ref().is_some_and(|p| p.auto_merge),
            conflicting: prs[i]
                .as_ref()
                .is_some_and(|p| p.mergeable == Mergeable::Conflicting),
            behind: prs[i]
                .as_ref()
                .is_some_and(|p| p.merge_state == MergeState::Behind),
            message_differs,
            landable: layer.dep == Dep::Main && layer.pr.is_some(),
            dep: layer.dep,
        });
    }

    Ok(StackStatus {
        trunk: config.trunk.clone(),
        layers,
    })
}

fn supports_unicode() -> bool {
    if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return false;
    }
    for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            let upper = val.to_ascii_uppercase();
            return upper.contains("UTF-8") || upper.contains("UTF8");
        }
    }
    true
}

impl StackStatus {
    /// Render top-down, the way a stack is usually drawn, with the trunk at
    /// the bottom. Automatically uses Unicode glyphs, colors, and terminal
    /// width truncation when stdout is a smart terminal.
    pub fn render(&self) -> String {
        self.render_plan(false)
    }

    /// Render the pre-push stack plan before `nspr diff` runs. When
    /// `update_message` is true, layers whose PR title/body differ from the
    /// local commit show `update message` rather than `message differs`.
    pub fn render_plan(&self, update_message: bool) -> String {
        let term = console::Term::stdout();
        let is_tty = term.is_term();
        let use_color = is_tty && console::colors_enabled();
        let use_unicode = is_tty && supports_unicode();
        let term_width = if is_tty {
            term.size_checked().map(|(_rows, cols)| cols as usize)
        } else {
            None
        };
        self.render_table_inner(
            None,
            update_message,
            use_unicode,
            use_color,
            term_width,
        )
    }

    /// Render the stack table after `nspr diff`, showing the action taken on
    /// each layer (`created`, `updated`, `refreshed`, `ok`) alongside status
    /// badges.
    pub fn render_diff(&self, outcomes: &[engine::LayerOutcome]) -> String {
        let term = console::Term::stdout();
        let is_tty = term.is_term();
        let use_color = is_tty && console::colors_enabled();
        let use_unicode = is_tty && supports_unicode();
        let term_width = if is_tty {
            term.size_checked().map(|(_rows, cols)| cols as usize)
        } else {
            None
        };
        self.render_table(Some(outcomes), use_unicode, use_color, term_width)
    }

    pub fn render_with_options(
        &self,
        use_unicode: bool,
        use_color: bool,
        term_width: Option<usize>,
    ) -> String {
        self.render_table(None, use_unicode, use_color, term_width)
    }

    pub fn render_table(
        &self,
        outcomes: Option<&[engine::LayerOutcome]>,
        use_unicode: bool,
        use_color: bool,
        term_width: Option<usize>,
    ) -> String {
        self.render_table_inner(
            outcomes,
            false,
            use_unicode,
            use_color,
            term_width,
        )
    }

    fn render_table_inner(
        &self,
        outcomes: Option<&[engine::LayerOutcome]>,
        update_message: bool,
        use_unicode: bool,
        use_color: bool,
        term_width: Option<usize>,
    ) -> String {
        use console::{Alignment, measure_text_width, pad_str, style, truncate_str};
        use engine::LayerAction;

        struct RowData<'a> {
            layer: &'a LayerStatus,
            num_plain: String,
            glyph: String,
            state_styled: String,
            badges_joined: String,
        }

        let arrow = if use_unicode { "→" } else { "->" };
        let dash = if use_unicode { "—" } else { "-" };

        let mut rows = Vec::with_capacity(self.layers.len());
        for layer in self.layers.iter().rev() {
            let outcome = outcomes
                .and_then(|list| list.iter().find(|o| o.index == layer.index));

            let num_plain = match layer.number.or(outcome.map(|o| o.number)) {
                Some(n) => format!("#{n}"),
                None => dash.to_string(),
            };

            let (raw_glyph, state_plain, glyph_styled, state_styled) =
                if let Some(o) = outcome {
                    match o.action {
                        LayerAction::Created => (
                            "○",
                            "created",
                            style("○").green().bold().to_string(),
                            style("created").green().bold().to_string(),
                        ),
                        LayerAction::Updated => (
                            "◉",
                            "updated",
                            style("◉").yellow().bold().to_string(),
                            style("updated").yellow().bold().to_string(),
                        ),
                        LayerAction::Refreshed => (
                            "◎",
                            "restacked",
                            style("◎").cyan().to_string(),
                            style("restacked").cyan().to_string(),
                        ),
                        LayerAction::Skipped => {
                            let g = if layer.draft { "◌" } else { "●" };
                            let gs = if layer.draft {
                                style(g).dim().to_string()
                            } else {
                                style(g).green().to_string()
                            };
                            (g, "ok", gs, style("ok").green().to_string())
                        }
                    }
                } else if outcomes.is_some() {
                    let g = if layer.draft { "◌" } else { "●" };
                    let gs = if layer.draft {
                        style(g).dim().to_string()
                    } else {
                        style(g).green().to_string()
                    };
                    (g, "ok", gs, style("ok").green().to_string())
                } else {
                    let sp = layer.state.label();
                    let raw = match layer.state {
                        LayerState::Current if layer.draft => "◌",
                        LayerState::Current => "●",
                        LayerState::Modified => "◉",
                        LayerState::NeedsRestack => "◎",
                        LayerState::New => "○",
                        LayerState::Merged => "✔",
                        LayerState::Closed => "✕",
                        LayerState::LegacySpr => "⚠",
                    };
                    let gs = match layer.state {
                        LayerState::Current if layer.draft => {
                            style(raw).dim().to_string()
                        }
                        LayerState::Current => style(raw).green().to_string(),
                        LayerState::Modified => style(raw).yellow().to_string(),
                        LayerState::NeedsRestack => style(raw).cyan().to_string(),
                        LayerState::New => style(raw).green().bold().to_string(),
                        LayerState::Merged => style(raw).magenta().to_string(),
                        LayerState::Closed => style(raw).red().to_string(),
                        LayerState::LegacySpr => {
                            style(raw).yellow().bold().to_string()
                        }
                    };
                    let ss = match layer.state {
                        LayerState::Current => style(sp).green().to_string(),
                        LayerState::Modified => {
                            style(sp).yellow().bold().to_string()
                        }
                        LayerState::NeedsRestack => style(sp).cyan().to_string(),
                        LayerState::New => style(sp).green().bold().to_string(),
                        LayerState::Merged => style(sp).magenta().to_string(),
                        LayerState::Closed => style(sp).red().to_string(),
                        LayerState::LegacySpr => {
                            style(sp).yellow().bold().to_string()
                        }
                    };
                    (raw, sp, gs, ss)
                };

            let glyph = if use_unicode {
                if use_color {
                    glyph_styled
                } else {
                    raw_glyph.to_string()
                }
            } else {
                "*".to_string()
            };
            let state_styled = if use_color {
                state_styled
            } else {
                state_plain.to_string()
            };

            let mut badges = Vec::new();
            if layer.draft {
                badges.push(if use_color {
                    style("draft").dim().to_string()
                } else {
                    "draft".to_string()
                });
            }
            if layer.conflicting {
                badges.push(if use_color {
                    style("conflicts").red().bold().to_string()
                } else {
                    "conflicts".to_string()
                });
            }
            if layer.behind {
                badges.push(if use_color {
                    style("behind").yellow().to_string()
                } else {
                    "behind".to_string()
                });
            }
            if layer.auto_merge {
                badges.push(if use_color {
                    style("AUTO-MERGE").red().bold().to_string()
                } else {
                    "AUTO-MERGE".to_string()
                });
            }
            if outcome.is_some_and(|o| o.retargeted) {
                let t = format!("rebased {arrow} {}", layer.wanted_base_label);
                badges.push(if use_color {
                    style(t).magenta().to_string()
                } else {
                    t
                });
            } else if layer.base.as_ref().is_some_and(|b| b != &layer.wanted_base)
            {
                let t = format!("retarget {arrow} {}", layer.wanted_base_label);
                badges.push(if use_color {
                    style(t).magenta().to_string()
                } else {
                    t
                });
            } else if layer.index > 0 && layer.dep == Dep::Main {
                let t = format!("base: {}", self.trunk);
                badges.push(if use_color {
                    style(t).blue().to_string()
                } else {
                    t
                });
            }
            if layer.message_differs {
                if update_message {
                    badges.push(if use_color {
                        style("update message").cyan().to_string()
                    } else {
                        "update message".to_string()
                    });
                } else {
                    badges.push(if use_color {
                        style("message differs").yellow().to_string()
                    } else {
                        "message differs".to_string()
                    });
                }
            }
            if layer.landable {
                badges.push(if use_color {
                    style("landable").green().to_string()
                } else {
                    "landable".to_string()
                });
            }

            rows.push(RowData {
                layer,
                num_plain,
                glyph,
                state_styled,
                badges_joined: badges.join("  "),
            });
        }

        let num_width = rows
            .iter()
            .map(|r| measure_text_width(&r.num_plain))
            .max()
            .unwrap_or(1)
            .max(2);
        let state_width = rows
            .iter()
            .map(|r| measure_text_width(&r.state_styled))
            .max()
            .unwrap_or(2);
        let max_badges_width = rows
            .iter()
            .map(|r| measure_text_width(&r.badges_joined))
            .max()
            .unwrap_or(0);

        let mut out = String::new();

        for row in &rows {
            let glyph = &row.glyph;
            let num_pad =
                num_width.saturating_sub(measure_text_width(&row.num_plain));
            let styled_num = if use_color {
                if row.layer.number.is_some() {
                    let colored = style(&row.num_plain).bold().cyan();
                    if let Some(url) = &row.layer.url {
                        format!(
                            "{}\x1b]8;;{url}\x1b\\{colored}\x1b]8;;\x1b\\",
                            " ".repeat(num_pad)
                        )
                    } else {
                        format!("{}{colored}", " ".repeat(num_pad))
                    }
                } else {
                    pad_str(
                        &style(&row.num_plain).dim().to_string(),
                        num_width,
                        Alignment::Right,
                        None,
                    )
                    .into_owned()
                }
            } else {
                pad_str(&row.num_plain, num_width, Alignment::Right, None)
                    .into_owned()
            };

            let padded_state =
                pad_str(&row.state_styled, state_width, Alignment::Left, None);

            let (badges_col, prefix_width) = if max_badges_width > 0 {
                let padded_badges = pad_str(
                    &row.badges_joined,
                    max_badges_width,
                    Alignment::Left,
                    None,
                );
                (
                    format!("  {padded_badges}"),
                    2 + 1 + 2 + num_width + 2 + state_width + 2 + max_badges_width + 2,
                )
            } else {
                (
                    String::new(),
                    2 + 1 + 2 + num_width + 2 + state_width + 2,
                )
            };

            let subject = if let Some(cols) = term_width
                && cols > prefix_width + 8
            {
                let avail = cols - prefix_width;
                let ellipsis = if use_unicode { "…" } else { "..." };
                truncate_str(&row.layer.subject, avail, ellipsis).into_owned()
            } else {
                row.layer.subject.clone()
            };

            let styled_subject = if use_color && row.layer.draft {
                style(&subject).dim().to_string()
            } else {
                subject
            };

            out.push_str(&format!(
                "  {glyph}  {styled_num}  {padded_state}{badges_col}  {styled_subject}\n"
            ));
        }

        let trunk_connector = if use_unicode {
            if use_color {
                style("┴─").dim().to_string()
            } else {
                "┴─".to_string()
            }
        } else {
            "\\-".to_string()
        };
        let styled_trunk = if use_color {
            style(&self.trunk).bold().to_string()
        } else {
            self.trunk.clone()
        };
        out.push_str(&format!("  {trunk_connector} {styled_trunk}\n"));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_aligns_columns_and_shortens_retarget_labels() {
        let status = StackStatus {
            trunk: "main".to_string(),
            layers: vec![
                LayerStatus {
                    index: 0,
                    subject: "[cross-project-tests] Avoid requiring packaging for GDB/LLDB version checks".into(),
                    number: Some(225126),
                    url: Some("https://github.com/llvm/llvm-project/pull/225126".into()),
                    branch: Some("users/arichardson/nspr/1".into()),
                    base: Some("main".into()),
                    wanted_base: "main".into(),
                    wanted_base_label: "main".into(),
                    state: LayerState::NeedsRestack,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    landable: true,
                    dep: Dep::Main,
                },
                LayerStatus {
                    index: 1,
                    subject: "[cross-project-tests] Derive tool substitutions from CMake".into(),
                    number: Some(225127),
                    url: Some("https://github.com/llvm/llvm-project/pull/225127".into()),
                    branch: Some("users/arichardson/cross-project-tests-derive-tool-substitutions-from-cmake".into()),
                    base: Some("users/arichardson/nspr/1".into()),
                    wanted_base: "users/arichardson/nspr/1".into(),
                    wanted_base_label: "#225126".into(),
                    state: LayerState::NeedsRestack,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    landable: false,
                    dep: Dep::Layer(0),
                },
                LayerStatus {
                    index: 2,
                    subject: "[RISC-V][LTO] Add baseline tests for LTO inline assembly".into(),
                    number: Some(225129),
                    url: Some("https://github.com/llvm/llvm-project/pull/225129".into()),
                    branch: Some("users/arichardson/nspr/3".into()),
                    base: Some("main".into()),
                    wanted_base: "users/arichardson/cross-project-tests-derive-tool-substitutions-from-cmake".into(),
                    wanted_base_label: "#225127".into(),
                    state: LayerState::Modified,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    landable: false,
                    dep: Dep::Layer(1),
                },
            ],
        };

        let rendered = status.render_with_options(true, false, None);
        let expected = concat!(
            "  ◉  #225129  modified  retarget → #225127  [RISC-V][LTO] Add baseline tests for LTO inline assembly\n",
            "  ◎  #225127  restack                       [cross-project-tests] Derive tool substitutions from CMake\n",
            "  ◎  #225126  restack   landable            [cross-project-tests] Avoid requiring packaging for GDB/LLDB version checks\n",
            "  ┴─ main\n",
        );
        assert_eq!(rendered, expected);
    }
}
