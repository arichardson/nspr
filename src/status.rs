//! `nspr status`: what the stack looks like, and what `nspr diff` would do.
//!
//! The staleness column is not computed here. It comes from running the real
//! [`crate::engine`] passes and stopping short of executing them, so `status`
//! can never disagree with what `diff` actually does. A reimplementation would
//! be a second copy of the subtlest logic in the codebase, free to drift.

use color_eyre::eyre::Result;

use crate::config::Config;
use crate::engine::{self, SyncOptions};
use crate::forge::{
    CheckCounts, Forge, MergeState, Mergeable, PrState, ReviewSummary,
};
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
    pub github_message_edited: bool,
    /// Ready to land: depends on nothing but the trunk.
    pub landable: bool,
    /// The layer this one is stacked on, if any.
    pub dep: Dep,
    /// All layer indices this layer directly depends on.
    pub layer_deps: Vec<usize>,
    /// Short human-readable labels for `layer_deps` (e.g. `["#102", "#103"]`).
    pub dep_labels: Vec<String>,
    pub checks: Option<CheckCounts>,
    pub reviews: ReviewSummary,
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
    let mut stack = stack.clone();
    let _ = engine::resolve_external_deps(
        forge,
        &mut stack,
        &SyncOptions::default(),
    )
    .await;
    let trees = stack.all_trees_lenient(git)?;
    let prs = engine::gather(forge, &stack).await?;
    let preserve_commit_history = match config.preserve_commit_history {
        crate::config::PreserveCommitHistory::Auto => config
            .preserve_commit_history
            .resolve(forge.repo_merge_settings().await?),
        crate::config::PreserveCommitHistory::True => true,
        crate::config::PreserveCommitHistory::False => false,
    };
    let opts = SyncOptions {
        preserve_commit_history,
        ..Default::default()
    };
    let decision = engine::decide(git, &stack, &prs, &trees, &opts)?;
    from_parts(git, config, &stack, &prs, &decision, false)
}

pub async fn status_for(
    git: &Git,
    forge: &dyn Forge,
    config: &Config,
    stack: &Stack,
    opts: &SyncOptions,
) -> Result<StackStatus> {
    let trees =
        stack.trees_for(git, opts.only_layer, opts.only_layers.as_ref())?;
    let prs = engine::gather_for(forge, stack, opts).await?;
    let mut status_opts = opts.clone();
    status_opts.preserve_commit_history = match config.preserve_commit_history {
        crate::config::PreserveCommitHistory::Auto => config
            .preserve_commit_history
            .resolve(forge.repo_merge_settings().await?),
        crate::config::PreserveCommitHistory::True => true,
        crate::config::PreserveCommitHistory::False => false,
    };
    let decision = engine::decide(git, stack, &prs, &trees, &status_opts)?;
    from_parts(git, config, stack, &prs, &decision, opts.update_message)
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
        let layer_deps = layer.layer_deps();
        let dep_labels: Vec<String> = layer_deps
            .iter()
            .map(|&j| match stack.layers[j].pr {
                Some(n) => format!("#{n}"),
                None => format!("layer {}", j + 1),
            })
            .collect();

        let (wanted_base, wanted_base_label) = if layer
            .has_multiple_layer_deps()
        {
            let head_branch = prs[i]
                .as_ref()
                .map(|p| p.head.clone())
                .unwrap_or_else(|| config.branch_name_for(layer.subject()));
            (
                crate::stack::synthetic_base_branch(&head_branch),
                dep_labels.join(" + "),
            )
        } else {
            match layer.dep {
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
            }
        };

        let message_differs = prs[i].as_ref().is_some_and(|p| {
            engine::pr_message_differs_from(p, &layer.message)
        });

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
                        || decision.author_changed[i]
                        || ((decision.message_changed[i]
                            || message_differs)
                            && (!decision.github_message_edited[i]
                                || update_message)) =>
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
            github_message_edited: decision.github_message_edited[i],
            landable: layer.is_root_landable() && layer.pr.is_some(),
            dep: layer.dep,
            layer_deps,
            dep_labels,
            checks: prs[i].as_ref().and_then(|p| p.checks.clone()),
            reviews: prs[i]
                .as_ref()
                .map(|p| p.reviews.clone())
                .unwrap_or_default(),
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
        self.render_verbose(false)
    }

    /// Render the stack status table, optionally printing a second line under
    /// each pull request with reviewer logins and failing check names.
    pub fn render_verbose(&self, verbose: bool) -> String {
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
            false,
            verbose,
            use_unicode,
            use_color,
            term_width,
        )
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
            false,
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

    pub fn render_with_options_verbose(
        &self,
        use_unicode: bool,
        use_color: bool,
        term_width: Option<usize>,
        verbose: bool,
    ) -> String {
        self.render_table_inner(
            None,
            false,
            verbose,
            use_unicode,
            use_color,
            term_width,
        )
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
            false,
            use_unicode,
            use_color,
            term_width,
        )
    }

    /// Group layers into connected components of the dependency graph,
    /// returning indices into `self.layers` in bottom-up order within each
    /// component.
    pub fn components(&self) -> Vec<Vec<usize>> {
        let n = self.layers.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }

        let by_index: std::collections::HashMap<usize, usize> = self
            .layers
            .iter()
            .enumerate()
            .map(|(pos, l)| (l.index, pos))
            .collect();

        for (pos, layer) in self.layers.iter().enumerate() {
            let visual_dep = match layer.dep {
                Dep::Layer(j) => Some(j),
                _ => layer.layer_deps.iter().copied().max(),
            };
            if let Some(j) = visual_dep
                && let Some(&parent_pos) = by_index.get(&j)
            {
                let (a, b) =
                    (find(&mut parent, pos), find(&mut parent, parent_pos));
                parent[a] = b;
            }
        }

        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut root_to_slot: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for pos in 0..n {
            let root = find(&mut parent, pos);
            match root_to_slot.get(&root) {
                Some(&slot) => components[slot].push(pos),
                None => {
                    root_to_slot.insert(root, components.len());
                    components.push(vec![pos]);
                }
            }
        }
        components
    }

    fn url_for_pr(&self, number: u64) -> Option<String> {
        if let Some(url) = self
            .layers
            .iter()
            .find(|l| l.number == Some(number))
            .and_then(|l| l.url.clone())
        {
            return Some(url);
        }
        self.layers
            .iter()
            .find_map(|l| l.url.as_deref())
            .and_then(|u| u.rsplit_once("/pull/"))
            .map(|(prefix, _)| format!("{prefix}/pull/{number}"))
    }

    fn render_table_inner(
        &self,
        outcomes: Option<&[engine::LayerOutcome]>,
        update_message: bool,
        verbose: bool,
        use_unicode: bool,
        use_color: bool,
        term_width: Option<usize>,
    ) -> String {
        use console::{
            Alignment, measure_text_width, pad_str, style, truncate_str,
        };
        use engine::LayerAction;

        struct RowData<'a> {
            layer: &'a LayerStatus,
            effective_num: Option<u64>,
            num_plain: String,
            glyph: String,
            state_styled: String,
            checks_plain: String,
            checks_styled: String,
            reviews_plain: String,
            reviews_styled: String,
            badges_plain: String,
            badges_styled: String,
        }

        let arrow = if use_unicode { "→" } else { "->" };
        let dash = if use_unicode { "—" } else { "-" };

        let components = self.components();
        let mut component_rows: Vec<Vec<RowData<'_>>> =
            Vec::with_capacity(components.len());

        for comp in components.iter().rev() {
            let mut rows = Vec::with_capacity(comp.len());
            for (pos_in_comp, &layer_pos) in comp.iter().enumerate().rev() {
                let layer = &self.layers[layer_pos];
                let outcome = outcomes.and_then(|list| {
                    list.iter().find(|o| o.index == layer.index)
                });

                let effective_num = layer.number.or(outcome.map(|o| o.number));
                let num_plain = match effective_num {
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
                            LayerState::Current => {
                                style(raw).green().to_string()
                            }
                            LayerState::Modified => {
                                style(raw).yellow().to_string()
                            }
                            LayerState::NeedsRestack => {
                                style(raw).cyan().to_string()
                            }
                            LayerState::New => {
                                style(raw).green().bold().to_string()
                            }
                            LayerState::Merged => {
                                style(raw).magenta().to_string()
                            }
                            LayerState::Closed => style(raw).red().to_string(),
                            LayerState::LegacySpr => {
                                style(raw).yellow().bold().to_string()
                            }
                        };
                        let ss = match layer.state {
                            LayerState::Current => {
                                style(sp).green().to_string()
                            }
                            LayerState::Modified => {
                                style(sp).yellow().bold().to_string()
                            }
                            LayerState::NeedsRestack => {
                                style(sp).cyan().to_string()
                            }
                            LayerState::New => {
                                style(sp).green().bold().to_string()
                            }
                            LayerState::Merged => {
                                style(sp).magenta().to_string()
                            }
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

                let mut badges_plain_vec = Vec::new();
                let mut badges_styled_vec = Vec::new();
                let mut push_badge = |plain: String, styled: String| {
                    if use_color {
                        badges_styled_vec.push(styled);
                    } else {
                        badges_styled_vec.push(plain.clone());
                    }
                    badges_plain_vec.push(plain);
                };

                if layer.draft {
                    push_badge(
                        "draft".to_string(),
                        style("draft").dim().to_string(),
                    );
                }
                if layer.conflicting {
                    push_badge(
                        "conflicts".to_string(),
                        style("conflicts").red().bold().to_string(),
                    );
                }
                if layer.behind {
                    push_badge(
                        "behind".to_string(),
                        style("behind").yellow().to_string(),
                    );
                }
                if layer.auto_merge {
                    push_badge(
                        "AUTO-MERGE".to_string(),
                        style("AUTO-MERGE").red().bold().to_string(),
                    );
                }
                let non_adjacent_in_comp = if pos_in_comp == 0 {
                    matches!(layer.dep, Dep::ExternalPr(_))
                } else {
                    let below_idx = self.layers[comp[pos_in_comp - 1]].index;
                    layer.dep != Dep::Layer(below_idx)
                };
                let target_pr_url = layer
                    .wanted_base_label
                    .strip_prefix('#')
                    .and_then(|s| s.parse::<u64>().ok())
                    .and_then(|n| self.url_for_pr(n));
                if outcome.is_some_and(|o| o.retargeted) {
                    let prefix = format!("rebased {arrow} ");
                    let plain = format!("{prefix}{}", layer.wanted_base_label);
                    let styled = match &target_pr_url {
                        Some(url) => format!(
                            "{}{}",
                            style(&prefix).magenta(),
                            crate::utils::osc8_link(
                                url,
                                style(&layer.wanted_base_label).magenta()
                            )
                        ),
                        None => style(&plain).magenta().to_string(),
                    };
                    push_badge(plain, styled);
                } else if layer
                    .base
                    .as_ref()
                    .is_some_and(|b| b != &layer.wanted_base)
                {
                    let prefix = format!("retarget {arrow} ");
                    let plain = format!("{prefix}{}", layer.wanted_base_label);
                    let styled = match &target_pr_url {
                        Some(url) => format!(
                            "{}{}",
                            style(&prefix).magenta(),
                            crate::utils::osc8_link(
                                url,
                                style(&layer.wanted_base_label).magenta()
                            )
                        ),
                        None => style(&plain).magenta().to_string(),
                    };
                    push_badge(plain, styled);
                } else if layer.dep_labels.len() >= 2 {
                    let prefix = "depends on ";
                    let plain =
                        format!("{prefix}{}", layer.dep_labels.join(", "));
                    let styled_parts: Vec<String> = layer
                        .dep_labels
                        .iter()
                        .map(|label| {
                            let url = label
                                .strip_prefix('#')
                                .and_then(|s| s.parse::<u64>().ok())
                                .and_then(|n| self.url_for_pr(n));
                            match url {
                                Some(u) => crate::utils::osc8_link(
                                    &u,
                                    style(label).blue(),
                                ),
                                None => style(label).blue().to_string(),
                            }
                        })
                        .collect();
                    let styled = format!(
                        "{}{}",
                        style(prefix).blue(),
                        styled_parts.join(&style(", ").blue().to_string()),
                    );
                    push_badge(plain, styled);
                } else if non_adjacent_in_comp {
                    let prefix = "base: ";
                    let plain = format!("{prefix}{}", layer.wanted_base_label);
                    let styled = match &target_pr_url {
                        Some(url) => format!(
                            "{}{}",
                            style(prefix).blue(),
                            crate::utils::osc8_link(
                                url,
                                style(&layer.wanted_base_label).blue()
                            )
                        ),
                        None => style(&plain).blue().to_string(),
                    };
                    push_badge(plain, styled);
                }
                if layer.message_differs {
                    if update_message || !layer.github_message_edited {
                        push_badge(
                            "update message".to_string(),
                            style("update message").cyan().to_string(),
                        );
                    } else {
                        push_badge(
                            "message differs".to_string(),
                            style("message differs").yellow().to_string(),
                        );
                    }
                }
                if layer.landable {
                    push_badge(
                        "landable".to_string(),
                        style("landable").green().to_string(),
                    );
                }

                let (checks_plain, checks_styled) = match &layer.checks {
                    Some(c) if c.total() > 0 => {
                        let plain =
                            format!("{}/{} checks", c.passed, c.total());
                        let styled = if use_color {
                            if c.failed > 0 {
                                style(&plain).red().bold().to_string()
                            } else if c.pending > 0 {
                                style(&plain).yellow().to_string()
                            } else {
                                style(&plain).green().to_string()
                            }
                        } else {
                            plain.clone()
                        };
                        (plain, styled)
                    }
                    _ => (String::new(), String::new()),
                };

                let (reviews_plain, reviews_styled) = match (
                    layer.reviews.approved_by.len(),
                    layer.reviews.changes_requested_by.len(),
                ) {
                    (0, 0) => (String::new(), String::new()),
                    (a, 0) => {
                        let plain = format!("{a} approved");
                        let styled = if use_color {
                            style(&plain).green().to_string()
                        } else {
                            plain.clone()
                        };
                        (plain, styled)
                    }
                    (0, c) => {
                        let plain = format!("{c} changes requested");
                        let styled = if use_color {
                            style(&plain).red().bold().to_string()
                        } else {
                            plain.clone()
                        };
                        (plain, styled)
                    }
                    (a, c) => {
                        let a_plain = format!("{a} approved");
                        let c_plain = format!("{c} changes requested");
                        let plain = format!("{a_plain}, {c_plain}");
                        let styled = if use_color {
                            format!(
                                "{}, {}",
                                style(&a_plain).green(),
                                style(&c_plain).red().bold()
                            )
                        } else {
                            plain.clone()
                        };
                        (plain, styled)
                    }
                };

                rows.push(RowData {
                    layer,
                    effective_num,
                    num_plain,
                    glyph,
                    state_styled,
                    checks_plain,
                    checks_styled,
                    reviews_plain,
                    reviews_styled,
                    badges_plain: badges_plain_vec.join("  "),
                    badges_styled: badges_styled_vec.join("  "),
                });
            }
            component_rows.push(rows);
        }

        let num_width = component_rows
            .iter()
            .flatten()
            .map(|r| measure_text_width(&r.num_plain))
            .max()
            .unwrap_or(1)
            .max(2);
        let state_width = component_rows
            .iter()
            .flatten()
            .map(|r| measure_text_width(&r.state_styled))
            .max()
            .unwrap_or(2);
        let max_checks_width = component_rows
            .iter()
            .flatten()
            .map(|r| measure_text_width(&r.checks_plain))
            .max()
            .unwrap_or(0);
        let max_reviews_width = component_rows
            .iter()
            .flatten()
            .map(|r| measure_text_width(&r.reviews_plain))
            .max()
            .unwrap_or(0);
        let max_badges_width = component_rows
            .iter()
            .flatten()
            .map(|r| measure_text_width(&r.badges_plain))
            .max()
            .unwrap_or(0);

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

        let mut out = String::new();
        if component_rows.is_empty() {
            out.push_str(&format!("  {trunk_connector} {styled_trunk}\n"));
            return out;
        }

        for (comp_idx, rows) in component_rows.iter().enumerate() {
            if comp_idx > 0 {
                out.push('\n');
            }
            for row in rows {
                let glyph = &row.glyph;
                let num_pad = num_width
                    .saturating_sub(measure_text_width(&row.num_plain));
                let styled_num = if use_color {
                    if let Some(n) = row.effective_num {
                        let colored = style(&row.num_plain).bold().cyan();
                        if let Some(url) =
                            row.layer.url.clone().or_else(|| self.url_for_pr(n))
                        {
                            format!(
                                "{}{}",
                                " ".repeat(num_pad),
                                crate::utils::osc8_link(&url, colored)
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

                let padded_state = pad_str(
                    &row.state_styled,
                    state_width,
                    Alignment::Left,
                    None,
                );

                let mut prefix_width =
                    2 + 1 + 2 + num_width + 2 + state_width + 2;
                let checks_col = if max_checks_width > 0 {
                    let padded_checks = pad_str(
                        &row.checks_styled,
                        max_checks_width,
                        Alignment::Right,
                        None,
                    );
                    prefix_width += max_checks_width + 2;
                    format!("  {padded_checks}")
                } else {
                    String::new()
                };
                let reviews_col = if max_reviews_width > 0 {
                    let padded_reviews = pad_str(
                        &row.reviews_styled,
                        max_reviews_width,
                        Alignment::Left,
                        None,
                    );
                    prefix_width += max_reviews_width + 2;
                    format!("  {padded_reviews}")
                } else {
                    String::new()
                };
                let badges_col = if max_badges_width > 0 {
                    let badges_pad = max_badges_width
                        .saturating_sub(measure_text_width(&row.badges_plain));
                    prefix_width += max_badges_width + 2;
                    format!("  {}{}", row.badges_styled, " ".repeat(badges_pad))
                } else {
                    String::new()
                };

                let subject = if let Some(cols) = term_width
                    && cols > prefix_width + 8
                {
                    let avail = cols - prefix_width;
                    let ellipsis = if use_unicode { "…" } else { "..." };
                    truncate_str(&row.layer.subject, avail, ellipsis)
                        .into_owned()
                } else {
                    row.layer.subject.clone()
                };

                let styled_subject = if use_color && row.layer.draft {
                    style(&subject).dim().to_string()
                } else {
                    subject
                };

                out.push_str(&format!(
                    "  {glyph}  {styled_num}  {padded_state}{checks_col}{reviews_col}{badges_col}  {styled_subject}\n"
                ));

                if verbose {
                    if !row.layer.reviews.approved_by.is_empty() {
                        let names = row.layer.reviews.approved_by.join(", ");
                        if use_color {
                            out.push_str(&format!(
                                "     {} {}\n",
                                style("approved by:").green(),
                                names
                            ));
                        } else {
                            out.push_str(&format!(
                                "     approved by: {names}\n"
                            ));
                        }
                    }
                    if !row.layer.reviews.changes_requested_by.is_empty() {
                        let names =
                            row.layer.reviews.changes_requested_by.join(", ");
                        if use_color {
                            out.push_str(&format!(
                                "     {} {}\n",
                                style("changes requested by:").red().bold(),
                                names
                            ));
                        } else {
                            out.push_str(&format!(
                                "     changes requested by: {names}\n"
                            ));
                        }
                    }
                    if let Some(checks) = &row.layer.checks
                        && !checks.failed_names.is_empty()
                    {
                        let names = checks.failed_names.join(", ");
                        if use_color {
                            out.push_str(&format!(
                                "     {} {}\n",
                                style("failed checks:").red().bold(),
                                names
                            ));
                        } else {
                            out.push_str(&format!(
                                "     failed checks: {names}\n"
                            ));
                        }
                    }
                }
            }
            out.push_str(&format!("  {trunk_connector} {styled_trunk}\n"));
        }
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
                    github_message_edited: false,
                    landable: true,
                    dep: Dep::Main,
                    layer_deps: Vec::new(),
                    dep_labels: Vec::new(),
                    checks: None,
                    reviews: ReviewSummary::default(),
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
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(0),
                    layer_deps: vec![0],
                    dep_labels: vec!["#225126".into()],
                    checks: None,
                    reviews: ReviewSummary::default(),
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
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(1),
                    layer_deps: vec![1],
                    dep_labels: vec!["#225127".into()],
                    checks: None,
                    reviews: ReviewSummary::default(),
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

    #[test]
    fn render_groups_multiple_independent_stacks_into_separate_blocks() {
        let status = StackStatus {
            trunk: "main".to_string(),
            layers: vec![
                LayerStatus {
                    index: 0,
                    subject: "[ToolA] Part 1".into(),
                    number: Some(101),
                    url: None,
                    branch: Some("users/me/toola-1".into()),
                    base: Some("main".into()),
                    wanted_base: "main".into(),
                    wanted_base_label: "main".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: true,
                    dep: Dep::Main,
                    layer_deps: Vec::new(),
                    dep_labels: Vec::new(),
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 1,
                    subject: "[ToolA] Part 2".into(),
                    number: Some(102),
                    url: None,
                    branch: Some("users/me/toola-2".into()),
                    base: Some("users/me/toola-1".into()),
                    wanted_base: "users/me/toola-1".into(),
                    wanted_base_label: "#101".into(),
                    state: LayerState::Modified,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(0),
                    layer_deps: vec![0],
                    dep_labels: vec!["#101".into()],
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 2,
                    subject: "[ToolB] Part 1".into(),
                    number: Some(201),
                    url: None,
                    branch: Some("users/me/toolb-1".into()),
                    base: Some("main".into()),
                    wanted_base: "main".into(),
                    wanted_base_label: "main".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: true,
                    dep: Dep::Main,
                    layer_deps: Vec::new(),
                    dep_labels: Vec::new(),
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 3,
                    subject: "[ToolB] Part 2".into(),
                    number: Some(202),
                    url: None,
                    branch: Some("users/me/toolb-2".into()),
                    base: Some("users/me/toolb-1".into()),
                    wanted_base: "users/me/toolb-1".into(),
                    wanted_base_label: "#201".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(2),
                    layer_deps: vec![2],
                    dep_labels: vec!["#201".into()],
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
            ],
        };

        let rendered = status.render_with_options(true, false, None);
        let expected = concat!(
            "  ●  #202  ok                  [ToolB] Part 2\n",
            "  ●  #201  ok        landable  [ToolB] Part 1\n",
            "  ┴─ main\n",
            "\n",
            "  ◉  #102  modified            [ToolA] Part 2\n",
            "  ●  #101  ok        landable  [ToolA] Part 1\n",
            "  ┴─ main\n",
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn render_displays_and_colors_checks_column() {
        console::set_colors_enabled(true);
        let status = StackStatus {
            trunk: "main".to_string(),
            layers: vec![
                LayerStatus {
                    index: 0,
                    subject: "All passing".into(),
                    number: Some(101),
                    url: None,
                    branch: Some("users/me/1".into()),
                    base: Some("main".into()),
                    wanted_base: "main".into(),
                    wanted_base_label: "main".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: true,
                    dep: Dep::Main,
                    layer_deps: Vec::new(),
                    dep_labels: Vec::new(),
                    checks: Some(CheckCounts {
                        passed: 10,
                        failed: 0,
                        pending: 0,
                        failed_names: Vec::new(),
                    }),
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 1,
                    subject: "One pending".into(),
                    number: Some(102),
                    url: None,
                    branch: Some("users/me/2".into()),
                    base: Some("users/me/1".into()),
                    wanted_base: "users/me/1".into(),
                    wanted_base_label: "#101".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(0),
                    layer_deps: vec![0],
                    dep_labels: vec!["#101".into()],
                    checks: Some(CheckCounts {
                        passed: 9,
                        failed: 0,
                        pending: 1,
                        failed_names: Vec::new(),
                    }),
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 2,
                    subject: "One failing".into(),
                    number: Some(103),
                    url: None,
                    branch: Some("users/me/3".into()),
                    base: Some("users/me/2".into()),
                    wanted_base: "users/me/2".into(),
                    wanted_base_label: "#102".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(1),
                    layer_deps: vec![1],
                    dep_labels: vec!["#102".into()],
                    checks: Some(CheckCounts {
                        passed: 9,
                        failed: 1,
                        pending: 0,
                        failed_names: vec!["clang-x86_64-debian".to_string()],
                    }),
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 3,
                    subject: "Unsubmitted commit".into(),
                    number: None,
                    url: None,
                    branch: None,
                    base: None,
                    wanted_base: "users/me/3".into(),
                    wanted_base_label: "#103".into(),
                    state: LayerState::New,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(2),
                    layer_deps: vec![2],
                    dep_labels: vec!["#103".into()],
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
            ],
        };

        let plain = status.render_with_options(true, false, None);
        let expected_plain = concat!(
            "  ○     —  new                          Unsubmitted commit\n",
            "  ●  #103  ok    9/10 checks            One failing\n",
            "  ●  #102  ok    9/10 checks            One pending\n",
            "  ●  #101  ok   10/10 checks  landable  All passing\n",
            "  ┴─ main\n",
        );
        assert_eq!(plain, expected_plain);

        let colored = status.render_with_options(true, true, None);
        let lines: Vec<&str> = colored.lines().collect();
        assert!(
            lines[1].contains(
                &console::style("9/10 checks").red().bold().to_string()
            ),
            "failing checks must be bold red: {}",
            lines[1]
        );
        assert!(
            lines[2]
                .contains(&console::style("9/10 checks").yellow().to_string()),
            "pending checks must be yellow: {}",
            lines[2]
        );
        assert!(
            lines[3]
                .contains(&console::style("10/10 checks").green().to_string()),
            "passing checks must be green: {}",
            lines[3]
        );
    }

    #[test]
    fn render_displays_reviews_and_verbose_details() {
        let status = StackStatus {
            trunk: "main".to_string(),
            layers: vec![
                LayerStatus {
                    index: 0,
                    subject: "Approved bottom PR".into(),
                    number: Some(101),
                    url: None,
                    branch: Some("users/me/1".into()),
                    base: Some("main".into()),
                    wanted_base: "main".into(),
                    wanted_base_label: "main".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: true,
                    dep: Dep::Main,
                    layer_deps: Vec::new(),
                    dep_labels: Vec::new(),
                    checks: Some(CheckCounts {
                        passed: 10,
                        failed: 0,
                        pending: 0,
                        failed_names: Vec::new(),
                    }),
                    reviews: ReviewSummary {
                        approved_by: vec![
                            "alice".to_string(),
                            "bob".to_string(),
                        ],
                        changes_requested_by: Vec::new(),
                    },
                },
                LayerStatus {
                    index: 1,
                    subject: "Changes requested and failing check".into(),
                    number: Some(102),
                    url: None,
                    branch: Some("users/me/2".into()),
                    base: Some("users/me/1".into()),
                    wanted_base: "users/me/1".into(),
                    wanted_base_label: "#101".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(0),
                    layer_deps: vec![0],
                    dep_labels: vec!["#101".into()],
                    checks: Some(CheckCounts {
                        passed: 8,
                        failed: 2,
                        pending: 0,
                        failed_names: vec![
                            "clang-x86_64-debian".to_string(),
                            "llvm-bazel".to_string(),
                        ],
                    }),
                    reviews: ReviewSummary {
                        approved_by: vec!["alice".to_string()],
                        changes_requested_by: vec!["carol".to_string()],
                    },
                },
            ],
        };

        let non_verbose =
            status.render_with_options_verbose(true, false, None, false);
        let expected_non_verbose = concat!(
            "  ●  #102  ok   8/10 checks  1 approved, 1 changes requested            Changes requested and failing check\n",
            "  ●  #101  ok  10/10 checks  2 approved                       landable  Approved bottom PR\n",
            "  ┴─ main\n",
        );
        assert_eq!(non_verbose, expected_non_verbose);

        let verbose =
            status.render_with_options_verbose(true, false, None, true);
        let expected_verbose = concat!(
            "  ●  #102  ok   8/10 checks  1 approved, 1 changes requested            Changes requested and failing check\n",
            "     approved by: alice\n",
            "     changes requested by: carol\n",
            "     failed checks: clang-x86_64-debian, llvm-bazel\n",
            "  ●  #101  ok  10/10 checks  2 approved                       landable  Approved bottom PR\n",
            "     approved by: alice, bob\n",
            "  ┴─ main\n",
        );
        assert_eq!(verbose, expected_verbose);
    }

    #[test]
    fn render_wraps_pr_numbers_and_badge_references_in_osc8_links() {
        console::set_colors_enabled(true);
        let status = StackStatus {
            trunk: "main".to_string(),
            layers: vec![
                LayerStatus {
                    index: 0,
                    subject: "Bottom commit".into(),
                    number: Some(227865),
                    url: Some(
                        "https://github.com/llvm/llvm-project/pull/227865"
                            .into(),
                    ),
                    branch: Some("users/me/bottom".into()),
                    base: Some("main".into()),
                    wanted_base: "main".into(),
                    wanted_base_label: "main".into(),
                    state: LayerState::Current,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: true,
                    dep: Dep::Main,
                    layer_deps: Vec::new(),
                    dep_labels: Vec::new(),
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
                LayerStatus {
                    index: 1,
                    subject: "Top commit".into(),
                    number: None,
                    url: None,
                    branch: None,
                    base: Some("main".into()),
                    wanted_base: "users/me/bottom".into(),
                    wanted_base_label: "#227865".into(),
                    state: LayerState::New,
                    draft: false,
                    auto_merge: false,
                    conflicting: false,
                    behind: false,
                    message_differs: false,
                    github_message_edited: false,
                    landable: false,
                    dep: Dep::Layer(0),
                    layer_deps: vec![0],
                    dep_labels: vec!["#227865".into()],
                    checks: None,
                    reviews: ReviewSummary::default(),
                },
            ],
        };

        let outcomes = vec![engine::LayerOutcome {
            index: 1,
            number: 227866,
            branch: "users/me/top".into(),
            tip: git2::Oid::ZERO_SHA1,
            action: engine::LayerAction::Created,
            base: "users/me/bottom".into(),
            retargeted: false,
        }];

        let rendered = status.render_table(Some(&outcomes), true, true, None);
        assert!(
            rendered.contains(
                "\x1b]8;;https://github.com/llvm/llvm-project/pull/227865\x1b\\"
            ),
            "existing PR number and badge reference must be wrapped in OSC 8 link: {rendered:?}"
        );
        assert!(
            rendered.contains(
                "\x1b]8;;https://github.com/llvm/llvm-project/pull/227866\x1b\\"
            ),
            "newly created PR number in diff table must be wrapped in OSC 8 link: {rendered:?}"
        );
    }
}
