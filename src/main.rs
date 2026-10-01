//! The `nspr` command line.
//!
//! Every subcommand follows the same shape: open a [`Session`], discover the
//! stack, do one thing, print what happened. The interesting logic lives in
//! the library; this file is deliberately thin, because everything here is
//! untested by the scenario suite.

use clap::{Args, CommandFactory as _, Parser, Subcommand};
use clap_complete::{ArgValueCandidates, CompleteEnv, CompletionCandidate};
use color_eyre::eyre::{Result, bail, eyre};
use console::style;
use git2::Oid;

use nspr::config::{self, Config};
use nspr::engine::{
    self, AUTO_UPDATE_MESSAGE, LayerAction, LayerOutcome, Prompter, SyncOptions,
};
use nspr::forge::Forge as _;
use nspr::forge::github::GitHubForge;
use nspr::git::Git;
use nspr::stack::Stack;
use nspr::{
    amend, auth, close, forge, guardrails, land, list, patch, stack_comment,
    status, sync,
};

#[derive(Parser)]
#[command(
    name = "nspr",
    version,
    about = "GitHub-native stacked pull requests, without force-pushes",
    long_about = "Each commit on your branch becomes one pull request, based \
                  on the pull request below it. Updating a pull request \
                  appends a commit rather than rewriting the branch, so \
                  review comments and \"changes since your last review\" keep \
                  working."
)]
struct Cli {
    /// The git remote that points at GitHub.
    #[arg(
        long,
        global = true,
        default_value = "origin",
        add = ArgValueCandidates::new(complete_git_remotes)
    )]
    remote: String,

    /// Print every decision, including the ones that led to doing nothing.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create or update the pull requests for the current stack (default).
    Diff(DiffArgs),
    /// Show the stack and what `nspr diff` would do, without touching GitHub.
    Status,
    /// Rebase onto the trunk and reconcile pull requests merged elsewhere.
    Sync,
    /// Squash-merge a pull request and repair the layers above it.
    Land(LandArgs),
    /// Copy titles and descriptions edited on GitHub back into your commits.
    Amend,
    /// Abandon a pull request and restack whatever depended on it.
    Close(CloseArgs),
    /// List open pull request stacks on GitHub.
    List(ListArgs),
    /// Fetch a pull request and its stack dependencies into a local branch.
    Patch(PatchArgs),
    /// Convert existing `spr` pull requests (`[spr]` commits / `Pull Request:` trailers) to native `nspr` stacked pull requests.
    Upgrade(UpgradeArgs),
    /// Generate shell completion scripts for bash, zsh, fish, elvish, or powershell.
    Completions(CompletionsArgs),
}

#[derive(Args)]
struct CompletionsArgs {
    /// Shell to generate completions for (`bash`, `zsh`, `fish`, `elvish`, `powershell`).
    #[arg(value_enum)]
    shell: clap_complete::Shell,

    /// Generate a standalone static completion script instead of a dynamic hook that invokes `nspr`.
    #[arg(long)]
    r#static: bool,
}

#[derive(Args, Default)]
struct UpgradeArgs {
    /// Overwrite each pull request's title and body on GitHub from the local commit.
    #[arg(long)]
    update_message: bool,
}

#[derive(Args, Default)]
struct ListArgs {
    /// List pull requests from all authors, not just yourself.
    #[arg(short, long)]
    all: bool,
}

fn parse_pr_arg(s: &str) -> std::result::Result<u64, String> {
    nspr::stack::parse_pr_ref(s).ok_or_else(|| {
        format!(
            "cannot parse `{s}` as a pull request number (expected e.g. `123`, `#123`, or a GitHub PR URL)"
        )
    })
}

fn complete_git_remotes() -> Vec<CompletionCandidate> {
    let Ok(repo) = git2::Repository::discover(".") else {
        return Vec::new();
    };
    let Ok(remotes) = repo.remotes() else {
        return Vec::new();
    };
    (0..remotes.len())
        .filter_map(|i| remotes.get(i).ok().flatten())
        .map(|name| {
            let mut candidate = CompletionCandidate::new(name);
            if let Ok(remote) = repo.find_remote(name)
                && let Ok(url) = remote.url()
            {
                candidate = candidate.help(Some(url.to_string().into()));
            }
            candidate
        })
        .collect()
}

fn complete_stack_prs() -> Vec<CompletionCandidate> {
    let Ok(repo) = git2::Repository::discover(".") else {
        return Vec::new();
    };
    let git = Git::new(repo);
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();

    let trunk = config::detect_trunk(&git, "origin")
        .unwrap_or_else(|_| "main".to_string());
    let trunk_oid = git
        .resolve_reference(&format!("refs/remotes/origin/{trunk}"))
        .or_else(|_| git.resolve_reference(&format!("refs/heads/{trunk}")));

    if let Ok(base_oid) = trunk_oid
        && let Ok(stack) = Stack::discover(&git, base_oid, &trunk)
    {
        for layer in &stack.layers {
            if let Some(number) = layer.pr
                && seen.insert(number)
            {
                candidates.push(
                    CompletionCandidate::new(number.to_string())
                        .help(Some(layer.subject().to_string().into())),
                );
            }
        }
    }

    if let Ok(ref_prs) = nspr::refs::all(&git) {
        for number in ref_prs {
            if seen.insert(number) {
                let mut candidate =
                    CompletionCandidate::new(number.to_string());
                let msg = nspr::refs::get_message(&git, number).or_else(|| {
                    let oid = git
                        .resolve_reference(&nspr::refs::ref_name(number))
                        .ok()?;
                    git.message_of(oid).ok()
                });
                if let Some(msg) = msg {
                    let parsed = nspr::trailers::CommitMessage::parse(&msg);
                    if !parsed.subject.is_empty() {
                        candidate = candidate.help(Some(parsed.subject.into()));
                    }
                }
                candidates.push(candidate);
            }
        }
    }

    candidates
}

fn write_completions(
    args: &CompletionsArgs,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    if args.r#static {
        let mut cmd = Cli::command();
        clap_complete::generate(args.shell, &mut cmd, "nspr", out);
        return Ok(());
    }
    let shell_name = match args.shell {
        clap_complete::Shell::Bash => "bash",
        clap_complete::Shell::Zsh => "zsh",
        clap_complete::Shell::Fish => "fish",
        clap_complete::Shell::Elvish => "elvish",
        clap_complete::Shell::PowerShell => "powershell",
        _ => bail!("unsupported shell: {}", args.shell),
    };
    let shells = clap_complete::env::Shells::builtins();
    let completer = shells
        .completer(shell_name)
        .ok_or_else(|| eyre!("unsupported shell: {shell_name}"))?;
    completer.write_registration("COMPLETE", "nspr", "nspr", "nspr", out)?;
    Ok(())
}

#[derive(Args)]
struct PatchArgs {
    /// The pull request number to check out (e.g. `123`, `#123`, or URL).
    #[arg(
        value_parser = parse_pr_arg,
        add = ArgValueCandidates::new(complete_stack_prs)
    )]
    number: u64,

    /// Name for the local branch. Defaults to `pr/<number>`.
    #[arg(short, long)]
    branch: Option<String>,

    /// Create the branch but do not check it out.
    #[arg(long)]
    no_checkout: bool,
}

#[derive(Args)]
struct CloseArgs {
    /// The pull request to close (e.g. `123`, `#123`, or URL).
    #[arg(
        value_parser = parse_pr_arg,
        add = ArgValueCandidates::new(complete_stack_prs)
    )]
    number: u64,
}

#[derive(Args, Default)]
struct DiffArgs {
    /// Push all stacks on the branch, not just the current stack.
    #[arg(short, long)]
    all: bool,

    /// Show what `nspr diff` would do without mutating local commits or GitHub.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Submit only the HEAD commit as an independent pull request targeting trunk (`Depends-On: main`).
    #[arg(short = 'c', long)]
    cherry_pick: bool,

    /// Start a new independent stack on trunk (`Depends-On: main`) at the first unsubmitted commit (or HEAD if all commits already have pull requests).
    #[arg(long, conflicts_with = "cherry_pick")]
    new_stack: bool,

    /// Description for this update, instead of being prompted.
    #[arg(short, long)]
    message: Option<String>,

    /// Overwrite each pull request's title and body from the local commit.
    #[arg(long)]
    update_message: bool,

    /// Open new pull requests as drafts.
    #[arg(long)]
    draft: bool,

    /// Never prompt; use the default update message.
    #[arg(long)]
    no_prompt: bool,
}

#[derive(Args, Default)]
struct LandArgs {
    /// Specific pull request to land (e.g. `--pr=1234`, `--pr #1234`, or URL).
    #[arg(
        long = "pr",
        value_name = "PR",
        value_parser = parse_pr_arg,
        add = ArgValueCandidates::new(complete_stack_prs)
    )]
    pr: Option<u64>,

    /// Specific pull request number to land (positional alias for `--pr`).
    #[arg(
        value_name = "PR",
        value_parser = parse_pr_arg,
        conflicts_with = "pr",
        add = ArgValueCandidates::new(complete_stack_prs)
    )]
    number: Option<u64>,

    /// Land the independent HEAD pull request (`Depends-On: main` / `--cherry-pick`).
    #[arg(short = 'c', long)]
    cherry_pick: bool,

    /// Land the lowest ready pull request at the bottom of the stack.
    #[arg(short, long)]
    bottom: bool,

    /// Keep landing while the bottom of the stack is approved and landable.
    #[arg(short, long)]
    all: bool,

    /// Body for the squash commit, instead of the commit's own message.
    #[arg(short, long)]
    message: Option<String>,
}

impl LandArgs {
    fn target_pr(&self) -> Option<u64> {
        self.pr.or(self.number)
    }
}

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();
    color_eyre::install()?;
    let cli = Cli::parse();
    if let Some(Command::Completions(args)) = &cli.command {
        return write_completions(args, &mut std::io::stdout());
    }
    let default_filter = if cli.verbose {
        "nspr=debug,warn"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(default_filter),
    )
    .format_timestamp_millis()
    .init();

    // `Forge` is `?Send` — the test fake holds `RefCell`s — so everything runs
    // on one thread inside a `LocalSet`.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let fetch_remote_trunk =
        !matches!(cli.command, Some(Command::Status | Command::List(_)));
    let mut session = Session::open(&cli.remote, fetch_remote_trunk).await?;
    match cli.command.unwrap_or(Command::Diff(DiffArgs::default())) {
        Command::Diff(args) => session.diff(args, cli.verbose).await,
        Command::Status => session.status(cli.verbose).await,
        Command::Sync => session.sync().await,
        Command::Land(args) => session.land(args).await,
        Command::Amend => session.amend().await,
        Command::Close(args) => session.close(args).await,
        Command::List(args) => session.list(args).await,
        Command::Patch(args) => session.patch(args).await,
        Command::Upgrade(args) => session.upgrade(args).await,
        Command::Completions(_) => unreachable!("handled in main"),
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    git: Git,
    forge: GitHubForge,
    config: Config,
    /// The trunk tip as of the last fetch. The stack is everything between
    /// this and `HEAD`.
    trunk_oid: Oid,
    remote: String,
}

impl Session {
    async fn open(remote: &str, fetch_remote_trunk: bool) -> Result<Self> {
        let repo = git2::Repository::discover(".").map_err(|e| {
            eyre!("cannot open a git repository here: {}", e.message())
        })?;
        log::debug!("opened git repository at {}", repo.path().display());
        let git = Git::new(repo);

        // The slug has to come from local config: we need it to build the API
        // client that would otherwise tell us the login.
        let (owner, name) = config::detect_repo(&git, remote)?;
        let trunk = config::detect_trunk(&git, remote)?;
        log::debug!(
            "detected repository {owner}/{name} on remote `{remote}` (trunk `{trunk}`)"
        );
        let token = auth::github_token()?;
        let forge = GitHubForge::new(git.repo().clone(), &owner, &name, token)?;

        let trunk_ref = format!("refs/remotes/{remote}/{trunk}");
        let local_trunk = git.resolve_reference(&trunk_ref).ok();

        let (login, trunk_oid) = match (!fetch_remote_trunk, local_trunk) {
            (true, Some(oid)) => {
                // For read-only commands (`nspr status`, `nspr list`), use the
                // local tracking ref (`refs/remotes/<remote>/<trunk>`) rather
                // than running a full `git fetch` on `<trunk>` every time a new
                // commit lands upstream, and defer `viewer_login()` until a
                // command actually needs the authenticated username.
                (String::new(), oid)
            }
            _ => {
                let (login, remote_oid) =
                    forge.viewer_login_and_branch_oid(&trunk).await?;
                let oid = sync::resolve_trunk_from_remote_oid(
                    &git,
                    &forge,
                    remote,
                    &trunk,
                    Ok(remote_oid),
                )
                .await?;
                (login, oid)
            }
        };
        let config = config::detect(&git, login, remote)?;

        Ok(Self {
            git,
            forge,
            config,
            trunk_oid,
            remote: remote.to_string(),
        })
    }

    fn discover(&self) -> Result<Stack> {
        let stack =
            Stack::discover(&self.git, self.trunk_oid, &self.config.trunk)?;
        if stack.layers.is_empty() {
            bail!(
                "no commits between {}/{} and HEAD — nothing to submit.",
                self.remote,
                self.config.trunk
            );
        }
        Ok(stack)
    }

    async fn diff(&self, args: DiffArgs, verbose: bool) -> Result<()> {
        let mut stack = self.discover()?;

        let only_layer = if args.cherry_pick {
            let head_idx = stack.layers.len() - 1;
            if stack.layers[head_idx].dep != nspr::stack::Dep::Main {
                if args.dry_run {
                    stack.layers[head_idx]
                        .message
                        .set(nspr::trailers::DEPENDS_ON, &self.config.trunk);
                    stack.layers[head_idx].dep_spec =
                        Some(nspr::stack::DepSpec::Main);
                    stack.layers[head_idx].dep = nspr::stack::Dep::Main;
                } else {
                    let mut msg = stack.layers[head_idx].message.clone();
                    msg.set(nspr::trailers::DEPENDS_ON, &self.config.trunk);
                    let pairs: Vec<(git2::Oid, String)> = stack
                        .layers
                        .iter()
                        .enumerate()
                        .map(|(i, l)| {
                            if i == head_idx {
                                (l.commit, msg.render())
                            } else {
                                (l.commit, l.message.render())
                            }
                        })
                        .collect();
                    self.git.rewrite_messages(stack.base, &pairs)?;
                    stack = self.discover()?;
                }
            }
            Some(head_idx)
        } else if args.new_stack {
            let target_idx = stack
                .layers
                .iter()
                .position(|l| l.pr.is_none())
                .unwrap_or(stack.layers.len() - 1);
            if target_idx > 0
                && stack.layers[target_idx].dep != nspr::stack::Dep::Main
            {
                if args.dry_run {
                    stack.layers[target_idx]
                        .message
                        .set(nspr::trailers::DEPENDS_ON, &self.config.trunk);
                    stack.layers[target_idx].dep_spec =
                        Some(nspr::stack::DepSpec::Main);
                    stack.layers[target_idx].dep = nspr::stack::Dep::Main;
                } else {
                    let mut msg = stack.layers[target_idx].message.clone();
                    msg.set(nspr::trailers::DEPENDS_ON, &self.config.trunk);
                    let pairs: Vec<(git2::Oid, String)> = stack
                        .layers
                        .iter()
                        .enumerate()
                        .map(|(i, l)| {
                            if i == target_idx {
                                (l.commit, msg.render())
                            } else {
                                (l.commit, l.message.render())
                            }
                        })
                        .collect();
                    self.git.rewrite_messages(stack.base, &pairs)?;
                    stack = self.discover()?;
                }
            }
            None
        } else {
            None
        };

        let components = stack.components();
        let only_layers = if !args.all
            && !args.cherry_pick
            && components.len() > 1
        {
            let head_idx = stack.layers.len() - 1;
            let current_comp = stack.component_of(head_idx);
            let commit_word = if current_comp.len() == 1 {
                "commit"
            } else {
                "commits"
            };
            eprintln!(
                "{} branch has {} independent stacks; only updating the current stack ({} {commit_word}). Use `nspr diff --all` to push all stacks.",
                style("warning:").yellow().bold(),
                components.len(),
                current_comp.len(),
            );
            Some(current_comp.into_iter().collect())
        } else {
            None
        };

        let mut opts = SyncOptions {
            sync_all: false,
            message: args.message.clone(),
            update_message: args.update_message,
            draft: args.draft,
            only_layer,
            only_layers,
            ..Default::default()
        };

        let fixed = FixedPrompter(AUTO_UPDATE_MESSAGE.to_string());
        let interactive = InteractivePrompter;
        let prompter: &dyn Prompter =
            if args.no_prompt || args.message.is_some() || args.dry_run {
                &fixed
            } else {
                &interactive
            };

        if !args.dry_run {
            engine::recover_missing_pr_trailers(
                &self.git,
                &self.forge,
                &self.config,
                &mut stack,
                &mut opts,
                prompter,
            )
            .await?;
        }

        // A preflight round of queries, before anything is mutated: renders the
        // stack plan up-front and emits guardrail warnings before any prompt or
        // network push runs.
        let plan = self.preflight(&mut stack, &mut opts, args.dry_run).await?;
        if args.dry_run {
            self.report_dry_run(&plan);
            return Ok(());
        }

        let outcomes = engine::sync_stack(
            &self.git,
            &self.forge,
            &self.config,
            &mut stack,
            &opts,
            prompter,
        )
        .await?;

        self.report(&stack, &opts, &outcomes, verbose).await?;

        if self.config.stack_comments {
            let updated = stack_comment::update_for_opts(
                &self.forge,
                &self.config,
                &stack,
                &opts,
            )
            .await?;
            if verbose {
                println!("  {} stack comment(s) written", updated);
            }
        }
        Ok(())
    }

    /// Render the pre-push stack plan, emit guardrail warnings, and resolve
    /// `opts.preserve_commit_history` and `opts.refresh_when_behind`. Read-only.
    async fn preflight(
        &self,
        stack: &mut Stack,
        opts: &mut SyncOptions,
        dry_run: bool,
    ) -> Result<status::StackStatus> {
        engine::resolve_external_deps(&self.forge, stack, opts).await?;
        let trees = stack.trees_for(
            &self.git,
            opts.only_layer,
            opts.only_layers.as_ref(),
        )?;
        let prs = engine::gather_for(&self.forge, stack, opts).await?;
        engine::reject_unusable_for(
            &prs,
            opts.only_layer,
            opts.only_layers.as_ref(),
        )?;
        nspr::upgrade::reject_if_legacy_spr_with_prs(
            &self.git,
            &self.config,
            stack,
            &prs,
            opts.only_layer,
            opts.only_layers.as_ref(),
        )?;
        let merge_settings = self.forge.repo_merge_settings().await?;
        opts.preserve_commit_history =
            self.config.preserve_commit_history.resolve(merge_settings);
        let initial_decision =
            engine::decide(&self.git, stack, &prs, &trees, opts)?;
        let rails = guardrails::probe(
            &self.forge,
            &self.config,
            stack,
            &prs,
            &initial_decision,
            opts.update_message,
        )
        .await?;
        opts.refresh_when_behind = rails.refresh_when_behind;
        let decision = engine::decide(&self.git, stack, &prs, &trees, opts)?;
        let mut plan = status::from_parts(
            &self.git,
            &self.config,
            stack,
            &prs,
            &decision,
            opts.update_message,
        )?;
        plan.layers.retain(|l| opts.is_layer_selected(l.index));
        print!("{}", plan.render_plan(opts.update_message));
        for warning in &rails.warnings {
            eprintln!("{} {warning}", style("warning:").yellow().bold());
        }
        let any_will_push = decision.push.iter().any(|&p| p)
            || plan
                .layers
                .iter()
                .any(|l| l.state == status::LayerState::Modified);
        if !dry_run && any_will_push {
            println!();
        }
        Ok(plan)
    }

    fn report_dry_run(&self, plan: &status::StackStatus) {
        let created = plan
            .layers
            .iter()
            .filter(|l| l.state == status::LayerState::New)
            .count();
        let updated = plan
            .layers
            .iter()
            .filter(|l| l.state == status::LayerState::Modified)
            .count();
        let refreshed = plan
            .layers
            .iter()
            .filter(|l| l.state == status::LayerState::NeedsRestack)
            .count();
        let retargeted = plan
            .layers
            .iter()
            .filter(|l| l.base.as_deref().is_some_and(|b| b != l.wanted_base))
            .count();

        let mut parts = Vec::new();
        if created > 0 {
            parts.push(format!("{created} to create"));
        }
        if updated > 0 {
            parts.push(format!("{updated} to update"));
        }
        if refreshed > 0 {
            parts.push(format!("{refreshed} to restack"));
        }
        if retargeted > 0 {
            parts.push(format!("{retargeted} to retarget"));
        }

        if parts.is_empty() {
            let total = plan.layers.len();
            let noun = if total == 1 { "PR" } else { "PRs" };
            println!(
                "{} Dry run (all {total} {noun} up to date)",
                style("✓").green().bold()
            );
        } else {
            println!(
                "{} Dry run ({})",
                style("✓").green().bold(),
                parts.join(", ")
            );
        }
    }

    async fn report(
        &self,
        stack: &Stack,
        opts: &SyncOptions,
        outcomes: &[LayerOutcome],
        verbose: bool,
    ) -> Result<()> {
        let created = outcomes
            .iter()
            .filter(|o| o.action == LayerAction::Created)
            .count();
        let updated = outcomes
            .iter()
            .filter(|o| o.action == LayerAction::Updated)
            .count();
        let refreshed = outcomes
            .iter()
            .filter(|o| o.action == LayerAction::Refreshed)
            .count();
        let retargeted = outcomes.iter().filter(|o| o.retargeted).count();

        if verbose {
            let mut report = status::status_for(
                &self.git,
                &self.forge,
                &self.config,
                stack,
                opts,
            )
            .await?;
            report.layers.retain(|l| opts.is_layer_selected(l.index));
            print!("{}", report.render_diff(outcomes));
        } else {
            for o in
                outcomes.iter().filter(|o| o.action == LayerAction::Created)
            {
                let subject = stack
                    .layers
                    .get(o.index)
                    .map(|l| l.subject())
                    .unwrap_or("");
                let url = self.config.pull_request_url(o.number);
                let num = self.config.pull_request_link(
                    o.number,
                    style(format!("#{}", o.number)).bold(),
                );
                println!(
                    "  {} {}  {}  {}",
                    style("○").green().bold(),
                    num,
                    subject,
                    style(url).dim(),
                );
            }
        }

        let mut parts = Vec::new();
        if created > 0 {
            parts.push(format!("{created} created"));
        }
        if updated > 0 {
            parts.push(format!("{updated} updated"));
        }
        if refreshed > 0 {
            parts.push(format!("{refreshed} restacked"));
        }
        if retargeted > 0 {
            parts.push(format!("{retargeted} retargeted"));
        }

        if parts.is_empty() {
            let total = (0..stack.layers.len())
                .filter(|&i| opts.is_layer_selected(i))
                .count();
            let noun = if total == 1 { "PR" } else { "PRs" };
            println!(
                "{} Done (all {total} {noun} up to date)",
                style("✓").green().bold()
            );
        } else {
            println!(
                "{} Done ({})",
                style("✓").green().bold(),
                parts.join(", ")
            );
        }
        Ok(())
    }

    async fn refresh_remaining_metadata(&self) -> Result<()> {
        let Ok(stack) =
            Stack::discover(&self.git, self.trunk_oid, &self.config.trunk)
        else {
            return Ok(());
        };
        if stack.layers.is_empty() {
            return Ok(());
        }

        self.forge.sync_stacks(&stack.pr_chains()).await?;

        if self.config.stack_comments {
            stack_comment::update_all(&self.forge, &self.config, &stack)
                .await?;
        }

        let merge_settings = self.forge.repo_merge_settings().await?;
        let preserve_commit_history =
            self.config.preserve_commit_history.resolve(merge_settings);
        let warn_merge_strategy =
            preserve_commit_history && !merge_settings.is_squash_only();
        let prs = engine::gather(&self.forge, &stack).await?;
        for pr in prs.into_iter().flatten() {
            let body =
                nspr::pr_body::splice_warning(&pr.body, warn_merge_strategy);
            if pr.body != body {
                self.forge
                    .update_pull_request(
                        pr.number,
                        forge::PullRequestUpdate {
                            body: Some(body),
                            ..Default::default()
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn close(&self, args: CloseArgs) -> Result<()> {
        let stack = self.discover()?;
        let Some(index) =
            stack.layers.iter().position(|l| l.pr == Some(args.number))
        else {
            // If the commit was already squashed or dropped locally (for
            // example, via `git rebase -i --autosquash`), close the pull
            // request on GitHub and retarget any layer in the current stack
            // whose remote base still points at its branch.
            let pr = self.forge.get_pull_request(args.number).await?;
            if pr.state != forge::PrState::Open {
                bail!("#{} is already closed or merged.", args.number);
            }
            let prs = engine::gather(&self.forge, &stack).await?;
            let mut retargeted_any = false;
            for other in prs.into_iter().flatten() {
                if other.base == pr.head {
                    self.forge
                        .update_pull_request(
                            other.number,
                            forge::PullRequestUpdate {
                                base: Some(pr.base.clone()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    retargeted_any = true;
                }
            }
            self.forge
                .update_pull_request(
                    args.number,
                    forge::PullRequestUpdate {
                        state: Some(forge::PrState::Closed),
                        ..Default::default()
                    },
                )
                .await?;
            let _ = nspr::refs::remove(&self.git, args.number);
            println!(
                "{} {} {}",
                style("closed").red().bold(),
                self.config.pull_request_link(
                    args.number,
                    format!("#{}", args.number)
                ),
                pr.title
            );
            if retargeted_any {
                println!("Restacking...");
                self.diff(
                    DiffArgs {
                        all: true,
                        no_prompt: true,
                        ..Default::default()
                    },
                    false,
                )
                .await?;
            } else {
                self.refresh_remaining_metadata().await?;
            }
            return Ok(());
        };

        let outcome = close::close_layer(
            &self.git,
            &self.forge,
            &self.config,
            &stack,
            index,
        )
        .await?;

        println!(
            "{} {} {}",
            style("closed").red().bold(),
            self.config.pull_request_link(
                outcome.number,
                format!("#{}", outcome.number)
            ),
            outcome.title
        );
        for warning in &outcome.warnings {
            eprintln!("{} {warning}", style("warning:").yellow().bold());
        }

        // The dependents were retargeted but still carry the closed layer's
        // changes. Leaving that on GitHub would show reviewers a diff nobody
        // intended, so push the repair now rather than waiting for the next
        // `nspr diff`.
        if !outcome.retargeted.is_empty() {
            println!("Restacking...");
            self.diff(
                DiffArgs {
                    no_prompt: true,
                    ..Default::default()
                },
                false,
            )
            .await?;
        } else {
            self.refresh_remaining_metadata().await?;
        }
        Ok(())
    }

    async fn amend(&self) -> Result<()> {
        let stack = self.discover()?;
        let changed = amend::amend(&self.git, &self.forge, &stack).await?;
        if changed.is_empty() {
            println!("Your commit messages already match GitHub.");
            return Ok(());
        }
        for a in &changed {
            println!(
                "  {}  {} -> {}",
                self.config
                    .pull_request_link(a.number, format!("#{}", a.number)),
                a.old_subject,
                a.new_subject
            );
        }
        Ok(())
    }

    async fn status(&self, verbose: bool) -> Result<()> {
        let stack = self.discover()?;
        let report =
            status::status(&self.git, &self.forge, &self.config, &stack)
                .await?;
        print!("{}", report.render_verbose(verbose));
        Ok(())
    }

    async fn upgrade(&self, args: UpgradeArgs) -> Result<()> {
        let mut stack = self.discover()?;
        let upgraded = nspr::upgrade::upgrade_stack_with_options(
            &self.git,
            &self.forge,
            &self.config,
            &mut stack,
            args.update_message,
        )
        .await?;

        if upgraded.is_empty() {
            println!(
                "All pull requests in this stack already use native nspr stacking."
            );
            return Ok(());
        }

        for item in &upgraded {
            println!(
                "  {} {}  {}",
                style("upgraded").green().bold(),
                self.config.pull_request_url(item.number),
                item.branch,
            );
            if item.old_base != item.new_base {
                println!(
                    "             retargeted base {} -> {}",
                    item.old_base, item.new_base
                );
            }
            if let Some(deleted) = &item.deleted_synthetic_base {
                println!(
                    "             deleted synthetic spr base branch {}",
                    deleted
                );
            }
            if let Some(warning) = &item.warning {
                eprintln!("{} {warning}", style("warning:").yellow().bold());
            }
        }
        Ok(())
    }

    async fn sync(&mut self) -> Result<()> {
        let stack = self.discover()?;
        let report =
            sync::sync_trunk(&self.git, &self.forge, &self.config, &stack)
                .await?;
        self.trunk_oid = report.trunk;
        let _ = self.git.set_reference(
            &format!("refs/remotes/{}/{}", self.remote, self.config.trunk),
            self.trunk_oid,
            "nspr: update trunk tracking ref after sync",
        );

        for warning in &report.warnings {
            eprintln!("{} {warning}", style("warning:").yellow().bold());
        }
        if !report.merged.is_empty() {
            let list: Vec<String> = report
                .merged
                .iter()
                .map(|&n| self.config.pull_request_link(n, format!("#{n}")))
                .collect();
            println!("Merged elsewhere: {}", list.join(", "));
        }
        for &number in &report.stranded {
            eprintln!(
                "{} {} was merged, but your local commit for it still \
                 has changes. Drop it by hand once you have salvaged them.",
                style("warning:").yellow().bold(),
                self.config.pull_request_link(number, format!("#{number}")),
            );
        }
        if report.rebased {
            println!("Rebased onto {}.", self.git.short_id(report.trunk)?);
            if let Ok(remaining) =
                Stack::discover(&self.git, self.trunk_oid, &self.config.trunk)
                && !remaining.layers.is_empty()
            {
                println!("Restacking...");
                self.diff(
                    DiffArgs {
                        all: true,
                        no_prompt: true,
                        ..Default::default()
                    },
                    false,
                )
                .await?;
            }
        } else {
            println!("Already up to date with {}.", self.config.trunk);
        }
        Ok(())
    }

    async fn land(&mut self, args: LandArgs) -> Result<()> {
        let stack = self.discover()?;
        let mut opts = land::LandOptions {
            message: args.message.clone(),
            keep_local: false,
            only_layers: None,
        };

        let outcomes = if args.all && !args.cherry_pick {
            if let Some(pr_num) = args.target_pr() {
                let idx = stack
                    .layers
                    .iter()
                    .position(|l| l.pr == Some(pr_num))
                    .ok_or_else(|| {
                        eyre!(
                            "#{pr_num} is not in this stack. Run `nspr status` to see your stack."
                        )
                    })?;
                opts.only_layers =
                    Some(stack.component_of(idx).into_iter().collect());
            }
            land::land_all(&self.git, &self.forge, &self.config, &stack, &opts)
                .await?
        } else {
            let index = resolve_land_target(&stack, &args, &self.config.trunk)?;
            let outcome = land::land_layer(
                &self.git,
                &self.forge,
                &self.config,
                &stack,
                index,
                &opts,
            )
            .await?;
            vec![outcome]
        };

        if let Some(last) = outcomes.last() {
            self.trunk_oid = last.squash;
            let _ = self.git.set_reference(
                &format!("refs/remotes/{}/{}", self.remote, self.config.trunk),
                self.trunk_oid,
                "nspr: update trunk tracking ref after land",
            );
        }

        for outcome in &outcomes {
            for warning in &outcome.warnings {
                eprintln!("{} {warning}", style("warning:").yellow().bold());
            }
        }

        self.refresh_remaining_metadata().await?;
        println!(
            "{} ({} landed)",
            style("✓ Done").green().bold(),
            outcomes.len()
        );
        Ok(())
    }

    async fn list(&self, args: ListArgs) -> Result<()> {
        let login;
        let author = if args.all {
            None
        } else if !self.config.login.is_empty() {
            Some(self.config.login.as_str())
        } else {
            login = self.forge.viewer_login().await?;
            Some(login.as_str())
        };
        let prs = self.forge.list_pull_requests(author).await?;
        let stacks = list::build_stacks(prs, &self.config.trunk);
        print!("{}", list::format_stacks(&stacks));
        Ok(())
    }

    async fn patch(&self, args: PatchArgs) -> Result<()> {
        let outcome = patch::patch_layer(
            &self.git,
            &self.forge,
            &self.config,
            self.trunk_oid,
            args.number,
            args.branch.as_deref(),
            args.no_checkout,
        )
        .await?;

        println!(
            "Created branch {} with {} commit{}.",
            style(&outcome.branch).bold(),
            outcome.commits.len(),
            if outcome.commits.len() == 1 { "" } else { "s" }
        );
        if outcome.checked_out {
            println!("Checked out {}.", style(&outcome.branch).bold());
        }
        match outcome.target_state {
            forge::PrState::Merged => {
                println!(
                    "{}",
                    style("Note: pull request is already merged.").yellow()
                );
            }
            forge::PrState::Closed => {
                println!(
                    "{}",
                    style("Note: pull request was closed without merging.")
                        .yellow()
                );
            }
            forge::PrState::Open => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Prompting
// ---------------------------------------------------------------------------

/// Never prompts. Used with `--no-prompt`, and for updates that do not change
/// the displayed diff.
struct FixedPrompter(String);

impl Prompter for FixedPrompter {
    fn update_message(&self, _subject: &str) -> Result<String> {
        Ok(self.0.clone())
    }

    fn confirm_relink_existing_pr(
        &self,
        subject: &str,
        existing_pr_number: u64,
        existing_pr_title: &str,
        branch: &str,
    ) -> Result<bool> {
        eprintln!(
            "{} commit \"{}\" has no `Pull-Request:` trailer, but open PR #{} (\"{}\") already uses branch `{}`; linking to #{}.",
            style("warning:").yellow().bold(),
            subject,
            existing_pr_number,
            existing_pr_title,
            branch,
            existing_pr_number,
        );
        Ok(true)
    }
}

/// Asks what changed, but only when the engine has decided the reviewer will
/// actually see a difference — so this does not fire on every `nspr diff`.
struct InteractivePrompter;

impl Prompter for InteractivePrompter {
    fn update_message(&self, label: &str) -> Result<String> {
        if !console::user_attended() {
            return Ok(AUTO_UPDATE_MESSAGE.to_string());
        }
        let prompt = if label.starts_with('#') {
            format!("What changed in {label}?")
        } else {
            format!("What changed in \"{label}\"?")
        };
        let answer: String = dialoguer::Input::new()
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()?;
        Ok(if answer.trim().is_empty() {
            AUTO_UPDATE_MESSAGE.to_string()
        } else {
            answer
        })
    }

    fn confirm_relink_existing_pr(
        &self,
        subject: &str,
        existing_pr_number: u64,
        existing_pr_title: &str,
        branch: &str,
    ) -> Result<bool> {
        eprintln!(
            "{} commit \"{}\" has no `Pull-Request:` trailer, but open PR #{} (\"{}\") already uses branch `{}`.",
            style("warning:").yellow().bold(),
            subject,
            existing_pr_number,
            existing_pr_title,
            branch,
        );
        if !console::user_attended() {
            return Ok(true);
        }
        let link = dialoguer::Confirm::new()
            .with_prompt(format!(
                "Link this commit to existing PR #{existing_pr_number} instead of opening a new PR?"
            ))
            .default(true)
            .interact()?;
        Ok(link)
    }
}

fn resolve_land_target(
    stack: &Stack,
    args: &LandArgs,
    trunk: &str,
) -> Result<usize> {
    if stack.layers.is_empty() {
        bail!(
            "nothing to land: your branch has no commits ahead of `{trunk}`."
        );
    }
    if args.cherry_pick {
        let head_idx = stack.layers.len() - 1;
        let head = &stack.layers[head_idx];
        if head.dep != nspr::stack::Dep::Main {
            bail!(
                "HEAD commit `{}` is stacked on another commit (`Depends-On: {}` is not set).\n\
                 Use `nspr land --bottom` or `nspr land --all` to land from the bottom up, \
                 or run `nspr diff --cherry-pick` first to make HEAD independent.",
                head.subject(),
                trunk,
            );
        }
        if head.pr.is_none() {
            bail!(
                "HEAD commit `{}` has no pull request yet; run `nspr diff --cherry-pick` first.",
                head.subject(),
            );
        }
        return Ok(head_idx);
    }
    if let Some(pr_num) = args.target_pr() {
        return stack
            .layers
            .iter()
            .position(|l| l.pr == Some(pr_num))
            .ok_or_else(|| {
                eyre!(
                    "#{} is not in this stack. Run `nspr status` to see your stack.",
                    pr_num
                )
            });
    }
    if args.bottom || args.all || stack.layers.len() == 1 {
        return land::next_landable(stack).ok_or_else(|| {
            eyre!(
                "nothing at the bottom of the stack is ready to land. Run \
                 `nspr status` to see why."
            )
        });
    }

    // If there is only a single open PR across the entire local stack (e.g. an
    // independent PR created via `nspr diff --cherry-pick` surrounded by local
    // WIP commits without PRs), and that PR is a root (`Dep::Main`), land it!
    let open_prs: Vec<usize> = stack
        .layers
        .iter()
        .enumerate()
        .filter(|(_, l)| l.pr.is_some())
        .map(|(i, _)| i)
        .collect();
    if open_prs.len() == 1 {
        let only_idx = open_prs[0];
        if stack.layers[only_idx].dep == nspr::stack::Dep::Main {
            return Ok(only_idx);
        }
    }

    // Also, if there is only a single landable root PR in the stack AND that
    // root PR is HEAD itself (e.g. HEAD was created with `--cherry-pick` while
    // lower commits don't have root PRs), land HEAD!
    let landable_roots: Vec<usize> = stack
        .layers
        .iter()
        .enumerate()
        .filter(|(_, l)| l.dep == nspr::stack::Dep::Main && l.pr.is_some())
        .map(|(i, _)| i)
        .collect();
    if landable_roots.len() == 1 && landable_roots[0] == stack.layers.len() - 1
    {
        return Ok(landable_roots[0]);
    }

    let head = stack.layers.last().unwrap();
    let head_label = match head.pr {
        Some(n) => format!("#{n} (`{}`)", head.subject()),
        None => format!("`{}`", head.subject()),
    };
    let roots: Vec<String> = stack
        .layers
        .iter()
        .filter(|l| l.dep == nspr::stack::Dep::Main)
        .map(|l| match l.pr {
            Some(n) => format!("#{n} (`{}`)", l.subject()),
            None => format!("`{}`", l.subject()),
        })
        .collect();
    let bottom_pr = stack
        .layers
        .iter()
        .find(|l| l.dep == nspr::stack::Dep::Main)
        .and_then(|l| l.pr);
    let pr_example = bottom_pr
        .map(|n| format!("--pr={n}"))
        .unwrap_or_else(|| "--pr=<NUMBER>".to_string());
    let cherry_pick_hint = if head.dep == nspr::stack::Dep::Main {
        "\n• `nspr land --cherry-pick`  Land the independent HEAD pull request"
    } else {
        ""
    };
    bail!(
        "your branch has {} commits stacked above `{trunk}` (`HEAD` is {head_label}; bottom of stack is {}).\n\
         Because stacked pull requests must be merged from the bottom up into `{trunk}`, specify what to land:\n\
         • `nspr land {pr_example}`   Land a specific root pull request and rebase remaining commits\n\
         • `nspr land --bottom`   Land the lowest ready pull request in the stack{cherry_pick_hint}\n\
         • `nspr land --all`      Land all ready pull requests from the bottom up",
        stack.layers.len(),
        roots.join(", "),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use nspr::stack::{Dep, Layer};
    use nspr::trailers::CommitMessage;

    fn dummy_layer(subject: &str, pr: Option<u64>, dep: Dep) -> Layer {
        Layer {
            commit: git2::Oid::ZERO_SHA1,
            parent: git2::Oid::ZERO_SHA1,
            message: CommitMessage::parse(subject),
            pr,
            dep_spec: None,
            dep,
        }
    }

    #[test]
    fn cli_close_accepts_number_hash_and_url() {
        let cli = Cli::try_parse_from(["nspr", "close", "123"]).unwrap();
        match cli.command {
            Some(Command::Close(args)) => assert_eq!(args.number, 123),
            _ => panic!("expected Close"),
        }

        let cli = Cli::try_parse_from(["nspr", "close", "#456"]).unwrap();
        match cli.command {
            Some(Command::Close(args)) => assert_eq!(args.number, 456),
            _ => panic!("expected Close"),
        }

        let cli = Cli::try_parse_from([
            "nspr",
            "close",
            "https://github.com/owner/repo/pull/789",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Close(args)) => assert_eq!(args.number, 789),
            _ => panic!("expected Close"),
        }
    }

    #[test]
    fn cli_patch_accepts_number_hash_and_url() {
        let cli = Cli::try_parse_from(["nspr", "patch", "#101"]).unwrap();
        match cli.command {
            Some(Command::Patch(args)) => assert_eq!(args.number, 101),
            _ => panic!("expected Patch"),
        }
    }

    #[test]
    fn cli_land_accepts_pr_flag_positional_and_bottom() {
        let cli = Cli::try_parse_from(["nspr", "land"]).unwrap();
        match cli.command {
            Some(Command::Land(args)) => {
                assert_eq!(args.target_pr(), None);
                assert!(!args.bottom);
                assert!(!args.all);
            }
            _ => panic!("expected Land"),
        }

        let cli = Cli::try_parse_from(["nspr", "land", "--pr=1234"]).unwrap();
        match cli.command {
            Some(Command::Land(args)) => {
                assert_eq!(args.target_pr(), Some(1234))
            }
            _ => panic!("expected Land"),
        }

        let cli =
            Cli::try_parse_from(["nspr", "land", "--pr", "#202"]).unwrap();
        match cli.command {
            Some(Command::Land(args)) => {
                assert_eq!(args.target_pr(), Some(202))
            }
            _ => panic!("expected Land"),
        }

        let cli = Cli::try_parse_from(["nspr", "land", "#303"]).unwrap();
        match cli.command {
            Some(Command::Land(args)) => {
                assert_eq!(args.target_pr(), Some(303))
            }
            _ => panic!("expected Land"),
        }

        let cli = Cli::try_parse_from(["nspr", "land", "--bottom"]).unwrap();
        match cli.command {
            Some(Command::Land(args)) => {
                assert_eq!(args.target_pr(), None);
                assert!(args.bottom);
            }
            _ => panic!("expected Land"),
        }

        let cli = Cli::try_parse_from(["nspr", "land", "--all"]).unwrap();
        match cli.command {
            Some(Command::Land(args)) => {
                assert_eq!(args.target_pr(), None);
                assert!(args.all);
            }
            _ => panic!("expected Land"),
        }
    }

    #[test]
    fn resolve_land_target_allows_single_layer_without_flags() {
        let stack = Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![dummy_layer("Single commit", Some(101), Dep::Main)],
        };
        let args = LandArgs::default();
        assert_eq!(resolve_land_target(&stack, &args, "main").unwrap(), 0);
    }

    #[test]
    fn resolve_land_target_rejects_ambiguous_bare_land_on_multi_layer_stack() {
        let stack = Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![
                dummy_layer("Bottom commit", Some(101), Dep::Main),
                dummy_layer("Middle commit", Some(102), Dep::Layer(0)),
                dummy_layer("Top HEAD commit", Some(103), Dep::Layer(1)),
            ],
        };
        let args = LandArgs::default();
        let err = resolve_land_target(&stack, &args, "main")
            .unwrap_err()
            .to_string();
        assert!(err.contains("HEAD` is #103 (`Top HEAD commit`)"));
        assert!(err.contains("bottom of stack is #101 (`Bottom commit`)"));
        assert!(err.contains("nspr land --pr=101"));
        assert!(err.contains("nspr land --bottom"));
        assert!(err.contains("nspr land --all"));
    }

    #[test]
    fn resolve_land_target_honors_explicit_pr_or_bottom_on_multi_layer_stack() {
        let stack = Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![
                dummy_layer("Bottom commit", Some(101), Dep::Main),
                dummy_layer("Top HEAD commit", Some(102), Dep::Layer(0)),
            ],
        };

        let bottom_args = LandArgs {
            bottom: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_land_target(&stack, &bottom_args, "main").unwrap(),
            0
        );

        let pr_args = LandArgs {
            pr: Some(102),
            ..Default::default()
        };
        assert_eq!(resolve_land_target(&stack, &pr_args, "main").unwrap(), 1);
    }

    #[test]
    fn resolve_land_target_allows_single_cherry_picked_pr_among_wip_commits() {
        let stack = Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![
                dummy_layer("WIP commit 1", None, Dep::Main),
                dummy_layer("WIP commit 2", None, Dep::Layer(0)),
                dummy_layer("Cherry-picked bugfix", Some(105), Dep::Main),
                dummy_layer("WIP commit 3", None, Dep::Layer(2)),
            ],
        };
        let args = LandArgs::default();
        assert_eq!(resolve_land_target(&stack, &args, "main").unwrap(), 2);
    }

    #[test]
    fn resolve_land_target_honors_cherry_pick_flag_and_rejects_stacked_head() {
        let stack = Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![
                dummy_layer("Bottom PR", Some(101), Dep::Main),
                dummy_layer("Independent HEAD PR", Some(105), Dep::Main),
            ],
        };
        let cp_args = LandArgs {
            cherry_pick: true,
            ..Default::default()
        };
        assert_eq!(resolve_land_target(&stack, &cp_args, "main").unwrap(), 1);

        let stacked_stack = Stack {
            trunk: "main".into(),
            base: git2::Oid::ZERO_SHA1,
            layers: vec![
                dummy_layer("Bottom PR", Some(101), Dep::Main),
                dummy_layer("Stacked HEAD PR", Some(102), Dep::Layer(0)),
            ],
        };
        let err = resolve_land_target(&stacked_stack, &cp_args, "main")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is stacked on another commit"));
    }

    #[test]
    fn cli_diff_accepts_cherry_pick_flag() {
        let cli =
            Cli::try_parse_from(["nspr", "diff", "--cherry-pick"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => assert!(args.cherry_pick),
            _ => panic!("expected Diff"),
        }

        let cli = Cli::try_parse_from(["nspr", "diff", "-c"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => assert!(args.cherry_pick),
            _ => panic!("expected Diff"),
        }
    }

    #[test]
    fn cli_diff_accepts_dry_run_and_all_flags() {
        let cli = Cli::try_parse_from(["nspr", "diff", "--dry-run"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => {
                assert!(args.dry_run);
                assert!(!args.all);
            }
            _ => panic!("expected Diff"),
        }

        let cli = Cli::try_parse_from(["nspr", "diff", "-n", "-a"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => {
                assert!(args.dry_run);
                assert!(args.all);
            }
            _ => panic!("expected Diff"),
        }
    }

    #[test]
    fn cli_upgrade_accepts_update_message_flag() {
        let cli = Cli::try_parse_from(["nspr", "upgrade"]).unwrap();
        match cli.command {
            Some(Command::Upgrade(args)) => assert!(!args.update_message),
            _ => panic!("expected Upgrade"),
        }

        let cli = Cli::try_parse_from(["nspr", "upgrade", "--update-message"])
            .unwrap();
        match cli.command {
            Some(Command::Upgrade(args)) => assert!(args.update_message),
            _ => panic!("expected Upgrade"),
        }
    }

    #[test]
    fn cli_completions_generates_bash_and_zsh_scripts() {
        for (shell, expected_dynamic, expected_static) in [
            (
                clap_complete::Shell::Bash,
                "_clap_complete_nspr",
                "complete -F _nspr",
            ),
            (
                clap_complete::Shell::Zsh,
                "_clap_dynamic_completer_nspr",
                "#compdef nspr",
            ),
        ] {
            let mut dynamic_out = Vec::new();
            write_completions(
                &CompletionsArgs {
                    shell,
                    r#static: false,
                },
                &mut dynamic_out,
            )
            .unwrap();
            let dynamic_str = String::from_utf8(dynamic_out).unwrap();
            assert!(
                dynamic_str.contains(expected_dynamic),
                "dynamic {shell} script missing `{expected_dynamic}`:\n{dynamic_str}"
            );

            let mut static_out = Vec::new();
            write_completions(
                &CompletionsArgs {
                    shell,
                    r#static: true,
                },
                &mut static_out,
            )
            .unwrap();
            let static_str = String::from_utf8(static_out).unwrap();
            assert!(
                static_str.contains(expected_static),
                "static {shell} script missing `{expected_static}`:\n{static_str}"
            );
        }
    }

    #[test]
    fn dynamic_completion_engine_completes_subcommands_and_flags() {
        let mut cmd = Cli::command();
        cmd.build();

        let subcommands = clap_complete::engine::complete(
            &mut cmd,
            vec!["nspr".into(), "".into()],
            1,
            None,
        )
        .unwrap();
        let sub_names: Vec<String> = subcommands
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        for expected in [
            "diff",
            "status",
            "sync",
            "land",
            "amend",
            "close",
            "list",
            "patch",
            "upgrade",
            "completions",
        ] {
            assert!(
                sub_names.contains(&expected.to_string()),
                "expected subcommand `{expected}` in {sub_names:?}"
            );
        }

        let diff_flags = clap_complete::engine::complete(
            &mut cmd,
            vec!["nspr".into(), "diff".into(), "--".into()],
            2,
            None,
        )
        .unwrap();
        let flag_names: Vec<String> = diff_flags
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        for expected in [
            "--cherry-pick",
            "--new-stack",
            "--update-message",
            "--dry-run",
            "--draft",
        ] {
            assert!(
                flag_names.contains(&expected.to_string()),
                "expected flag `{expected}` in {flag_names:?}"
            );
        }
    }
}
