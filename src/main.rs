//! The `nspr` command line.
//!
//! Every subcommand follows the same shape: open a [`Session`], discover the
//! stack, do one thing, print what happened. The interesting logic lives in
//! the library; this file is deliberately thin, because everything here is
//! untested by the scenario suite.

use clap::{Args, Parser, Subcommand};
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
    #[arg(long, global = true, default_value = "origin")]
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
    Upgrade,
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

#[derive(Args)]
struct PatchArgs {
    /// The pull request number to check out (e.g. `123`, `#123`, or URL).
    #[arg(value_parser = parse_pr_arg)]
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
    #[arg(value_parser = parse_pr_arg)]
    number: u64,
}

#[derive(Args, Default)]
struct DiffArgs {
    /// Push every layer, even ones whose displayed diff has not changed.
    #[arg(short, long)]
    all: bool,

    /// Submit only the HEAD commit as an independent pull request targeting trunk (`Depends-On: main`).
    #[arg(short = 'c', long)]
    cherry_pick: bool,

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
    #[arg(long = "pr", value_name = "PR", value_parser = parse_pr_arg)]
    pr: Option<u64>,

    /// Specific pull request number to land (positional alias for `--pr`).
    #[arg(value_name = "PR", value_parser = parse_pr_arg, conflicts_with = "pr")]
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
    color_eyre::install()?;
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn"),
    )
    .init();

    let cli = Cli::parse();

    // `Forge` is `?Send` — the test fake holds `RefCell`s — so everything runs
    // on one thread inside a `LocalSet`.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let mut session = Session::open(&cli.remote).await?;
    match cli.command.unwrap_or(Command::Diff(DiffArgs::default())) {
        Command::Diff(args) => session.diff(args, cli.verbose).await,
        Command::Status => session.status().await,
        Command::Sync => session.sync().await,
        Command::Land(args) => session.land(args).await,
        Command::Amend => session.amend().await,
        Command::Close(args) => session.close(args).await,
        Command::List(args) => session.list(args).await,
        Command::Patch(args) => session.patch(args).await,
        Command::Upgrade => session.upgrade().await,
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
    async fn open(remote: &str) -> Result<Self> {
        let repo = git2::Repository::discover(".").map_err(|e| {
            // libgit2 cannot read reftable repositories, and recent git
            // versions create them. The raw message names an "extension",
            // which tells the user nothing about what to do.
            if e.message().contains("refstorage") {
                eyre!(
                    "this repository stores its refs in the `reftable` \
                     format, which the git library nspr uses cannot read \
                     yet.\nConvert it with `git refs migrate \
                     --ref-format=files`, or clone with `git clone \
                     --ref-format=files`."
                )
            } else {
                eyre!("cannot open a git repository here: {}", e.message())
            }
        })?;
        let git = Git::new(repo);

        // The slug has to come from local config: we need it to build the API
        // client that would otherwise tell us the login.
        let (owner, name) = config::detect_repo(&git, remote)?;
        let token = auth::github_token()?;
        let forge = GitHubForge::new(git.repo().clone(), &owner, &name, token)?;
        let login = forge.viewer_login().await?;
        let config = config::detect(&git, login, remote)?;

        let trunk_oid =
            sync::resolve_trunk(&git, &forge, remote, &config.trunk).await?;

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
        if !args.cherry_pick {
            nspr::upgrade::reject_if_legacy_spr(
                &self.git,
                &self.forge,
                &self.config,
                &stack,
            )
            .await?;
        }

        let only_layer = if args.cherry_pick {
            let head_idx = stack.layers.len() - 1;
            if stack.layers[head_idx].dep != nspr::stack::Dep::Main {
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
            Some(head_idx)
        } else {
            None
        };

        // A preflight round of queries, before anything is mutated. It costs a
        // second pass over the pull requests, but a warning that arrives after
        // the push that dismissed six approvals is worthless.
        let refresh_when_behind = self.preflight(&stack).await?;

        let opts = SyncOptions {
            sync_all: args.all,
            message: args.message.clone(),
            update_message: args.update_message,
            draft: args.draft,
            refresh_when_behind,
            only_layer,
        };

        let fixed = FixedPrompter(AUTO_UPDATE_MESSAGE.to_string());
        let interactive = InteractivePrompter;
        let prompter: &dyn Prompter =
            if args.no_prompt || args.message.is_some() {
                &fixed
            } else {
                &interactive
            };

        let outcomes = engine::sync_stack(
            &self.git,
            &self.forge,
            &self.config,
            &mut stack,
            &opts,
            prompter,
        )
        .await?;

        self.report(&outcomes, verbose);

        if self.config.stack_comments {
            let updated =
                stack_comment::update_all(&self.forge, &self.config, &stack)
                    .await?;
            if verbose {
                println!("  {} stack comment(s) written", updated);
            }
        }
        Ok(())
    }

    /// Emit guardrail warnings and report whether "behind" layers are worth
    /// refreshing. Read-only.
    async fn preflight(&self, stack: &Stack) -> Result<bool> {
        let trees = stack.all_trees(&self.git)?;
        let prs = engine::gather(&self.forge, stack).await?;
        let decision = engine::decide(
            &self.git,
            stack,
            &prs,
            &trees,
            &SyncOptions::default(),
        )?;
        let rails = guardrails::probe(
            &self.forge,
            &self.config,
            stack,
            &prs,
            &decision.push,
        )
        .await?;
        for warning in &rails.warnings {
            eprintln!("{} {warning}", style("warning:").yellow().bold());
        }
        Ok(rails.refresh_when_behind)
    }

    fn report(&self, outcomes: &[LayerOutcome], verbose: bool) {
        let mut quiet = true;
        for outcome in outcomes {
            let (verb, colour) = match outcome.action {
                LayerAction::Created => ("created", style("created").green()),
                LayerAction::Updated => ("updated", style("updated").cyan()),
                LayerAction::Refreshed => {
                    ("refreshed", style("refreshed").blue())
                }
                LayerAction::Skipped => ("", style("unchanged").dim()),
            };
            if verb.is_empty() && !verbose {
                continue;
            }
            quiet = false;
            println!(
                "  {colour:<10} {}  {}",
                self.config.pull_request_url(outcome.number),
                outcome.branch,
            );
            if outcome.retargeted {
                println!("             rebased onto {}", outcome.base);
            }
        }
        if quiet {
            println!("Everything is already up to date.");
        }
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

        let prs = engine::gather(&self.forge, &stack).await?;
        for (i, _layer) in stack.layers.iter().enumerate() {
            if let Some(pr) = &prs[i] {
                let is_stacked = stack.is_layer_stacked(i);
                let body = nspr::pr_body::splice_warning(&pr.body, is_stacked);
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
        }
        Ok(())
    }

    async fn close(&self, args: CloseArgs) -> Result<()> {
        let stack = self.discover()?;
        let Some(index) =
            stack.layers.iter().position(|l| l.pr == Some(args.number))
        else {
            bail!(
                "#{} is not in this stack. `nspr close` only manages pull \
                 requests whose commits are on your current branch.",
                args.number
            );
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
            "{} #{} {}",
            style("closed").red().bold(),
            outcome.number,
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
            println!("  #{}  {} -> {}", a.number, a.old_subject, a.new_subject);
        }
        Ok(())
    }

    async fn status(&self) -> Result<()> {
        let stack = self.discover()?;
        let report =
            status::status(&self.git, &self.forge, &self.config, &stack)
                .await?;
        print!("{}", report.render());
        Ok(())
    }

    async fn upgrade(&self) -> Result<()> {
        let mut stack = self.discover()?;
        let upgraded = nspr::upgrade::upgrade_stack(
            &self.git,
            &self.forge,
            &self.config,
            &mut stack,
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
        }
        Ok(())
    }

    async fn sync(&mut self) -> Result<()> {
        let stack = self.discover()?;
        let report =
            sync::sync_trunk(&self.git, &self.forge, &self.config, &stack)
                .await?;
        self.trunk_oid = report.trunk;

        for warning in &report.warnings {
            eprintln!("{} {warning}", style("warning:").yellow().bold());
        }
        if !report.merged.is_empty() {
            let list: Vec<String> =
                report.merged.iter().map(|n| format!("#{n}")).collect();
            println!("Merged elsewhere: {}", list.join(", "));
        }
        for number in &report.stranded {
            eprintln!(
                "{} #{number} was merged, but your local commit for it still \
                 has changes. Drop it by hand once you have salvaged them.",
                style("warning:").yellow().bold()
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
        loop {
            let stack = self.discover()?;
            let index = resolve_land_target(&stack, &args, &self.config.trunk)?;

            let opts = land::LandOptions {
                message: args.message.clone(),
                keep_local: false,
            };
            let outcome = land::land_layer(
                &self.git,
                &self.forge,
                &self.config,
                &stack,
                index,
                &opts,
            )
            .await?;

            self.trunk_oid = outcome.squash;

            for warning in &outcome.warnings {
                eprintln!("{} {warning}", style("warning:").yellow().bold());
            }
            println!(
                "{} #{} {} as {}",
                style("landed").green().bold(),
                outcome.number,
                outcome.title,
                self.git.short_id(outcome.squash)?
            );
            for repair in &outcome.repaired {
                println!("  repaired #{}", repair.number);
            }

            if !args.all || args.target_pr().is_some() || args.cherry_pick {
                self.refresh_remaining_metadata().await?;
                return Ok(());
            }
            // `land --all` stops at the first layer that is not landable
            // rather than erroring: having landed three of five is a success.
            if let Ok(stack) = self.discover()
                && land::next_landable(&stack).is_some()
            {
                continue;
            }
            self.refresh_remaining_metadata().await?;
            return Ok(());
        }
    }

    async fn list(&self, args: ListArgs) -> Result<()> {
        let author = if args.all {
            None
        } else {
            Some(self.config.login.as_str())
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
}

/// Asks what changed, but only when the engine has decided the reviewer will
/// actually see a difference — so this does not fire on every `nspr diff`.
struct InteractivePrompter;

impl Prompter for InteractivePrompter {
    fn update_message(&self, subject: &str) -> Result<String> {
        if !console::user_attended() {
            return Ok(AUTO_UPDATE_MESSAGE.to_string());
        }
        let answer: String = dialoguer::Input::new()
            .with_prompt(format!("What changed in \"{subject}\"?"))
            .allow_empty(true)
            .interact_text()?;
        Ok(if answer.trim().is_empty() {
            AUTO_UPDATE_MESSAGE.to_string()
        } else {
            answer
        })
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
            base: git2::Oid::ZERO_SHA1,
            layers: vec![dummy_layer("Single commit", Some(101), Dep::Main)],
        };
        let args = LandArgs::default();
        assert_eq!(resolve_land_target(&stack, &args, "main").unwrap(), 0);
    }

    #[test]
    fn resolve_land_target_rejects_ambiguous_bare_land_on_multi_layer_stack() {
        let stack = Stack {
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
}
