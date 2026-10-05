//! Finding a GitHub API token.
//!
//! Five sources, first hit wins. Fast local sources (environment variables and
//! git config) are checked before spawning `gh auth token` because `gh` queries
//! the system keyring over D-Bus, which can hang for 60+ seconds in headless or
//! SSH sessions when the keyring daemon is locked or unresponsive.
//!
//! Before spawning `gh` at all, the Secret Service's `Locked` property is
//! read over D-Bus. A locked keyring means `gh` would sit
//! waiting for someone to unlock it in the desktop session: in a terminal we
//! say so and wait a bounded time for that to happen, and without one we
//! fail straight away instead of hanging.
//!
//! An empty value is treated as absent rather than as an answer: CI images
//! routinely export `GITHUB_TOKEN=` when no secret is available, and stopping
//! there would mean failing with "bad credentials" instead of falling through
//! to a token that works.

use std::fmt;
use std::io::{IsTerminal as _, Read as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, bail};
use log::{debug, warn};

/// Checked before `GITHUB_TOKEN` so that a token scoped to nspr can override
/// whatever else the environment happens to be carrying.
pub const TOKEN_ENV_VAR: &str = "NSPR_GITHUB_TOKEN";
const FALLBACK_ENV_VAR: &str = "GITHUB_TOKEN";
/// The variable `gh` itself prefers; when set, `gh auth token` just echoes it
/// without going near the keyring.
const GH_TOKEN_ENV_VAR: &str = "GH_TOKEN";
const GIT_CONFIG_KEY: &str = "nspr.githubAuthToken";
const LEGACY_SPR_GIT_CONFIG_KEY: &str = "spr.githubAuthToken";

const GH_AUTH_ARGS: &[&str] = &["auth", "token"];
/// Maximum time to wait for `gh auth token` before killing it and failing fast.
const GH_AUTH_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for `gh auth token` when the keyring is known to be locked
/// and there is a user at a terminal who can unlock it.
const GH_AUTH_UNLOCK_TIMEOUT: Duration = Duration::from_secs(15);
/// Delay before printing a visible hint that `gh auth token` is taking a while.
const GH_AUTH_WARN_AFTER: Duration = Duration::from_millis(500);
/// Upper bound on the D-Bus query for the keyring's lock state.
const KEYRING_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The token to authenticate with, or an error explaining how to get one.
pub fn github_token() -> Result<String> {
    let mut gh_error: Option<String> = None;
    match first_token(
        |name| std::env::var(name).ok(),
        git_config_token,
        || gh_auth_token(&mut gh_error),
    ) {
        Some(token) => Ok(token),
        None => {
            let detail = match gh_error {
                Some(reason) => format!(" (`gh auth token`: {reason})"),
                None => String::new(),
            };
            bail!(
                "no GitHub token found{detail}. Set `{GIT_CONFIG_KEY}` in your \
                 git config, set ${TOKEN_ENV_VAR}, or run `gh auth login`."
            )
        }
    }
}

/// The source chain, with its inputs injected so it can be tested.
fn first_token<E, C, G>(env: E, git_config: C, gh: G) -> Option<String>
where
    E: Fn(&str) -> Option<String>,
    C: FnOnce() -> Option<String>,
    G: FnOnce() -> Option<String>,
{
    if let Some(t) = env(TOKEN_ENV_VAR).and_then(clean) {
        debug!("using GitHub token from ${TOKEN_ENV_VAR}");
        return Some(t);
    }
    debug!("${TOKEN_ENV_VAR} not set; checking ${FALLBACK_ENV_VAR}");
    if let Some(t) = env(FALLBACK_ENV_VAR).and_then(clean) {
        debug!("using GitHub token from ${FALLBACK_ENV_VAR}");
        return Some(t);
    }
    debug!(
        "${FALLBACK_ENV_VAR} not set; checking git config (`{GIT_CONFIG_KEY}` / `{LEGACY_SPR_GIT_CONFIG_KEY}`)"
    );
    if let Some(t) = git_config().and_then(clean) {
        return Some(t);
    }
    debug!(
        "no GitHub token in environment or git config; falling back to `gh auth token`"
    );
    if let Some(t) = gh().and_then(clean) {
        debug!("using GitHub token from `gh auth token`");
        return Some(t);
    }
    None
}

/// Trailing newlines come with `gh auth token`, and a token with one attached
/// produces an HTTP header GitHub rejects as malformed.
fn clean(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Lock state of the default Secret Service collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyringState {
    Locked,
    Unlocked,
    /// No Secret Service, no `busctl`, or an answer we could not parse.
    Unknown,
}

/// How to go about running `gh auth token`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhAuthPlan {
    /// Run it with the normal short timeout: `gh` either won't touch the
    /// keyring, or the keyring is not known to be locked.
    Run,
    /// The keyring is locked but someone is at a terminal: tell them, and give
    /// them a bounded amount of time to unlock it.
    WaitForUnlock,
    /// The keyring is locked and nobody is around to unlock it, so running
    /// `gh` could only end in a timeout.
    Skip,
}

/// Decide how to run `gh auth token`. The keyring is only probed when `gh`
/// would actually have to read it.
fn plan_gh_auth(
    gh_has_token_without_keyring: bool,
    keyring: impl FnOnce() -> KeyringState,
    interactive: bool,
) -> GhAuthPlan {
    if gh_has_token_without_keyring {
        return GhAuthPlan::Run;
    }
    match keyring() {
        KeyringState::Locked if interactive => GhAuthPlan::WaitForUnlock,
        KeyringState::Locked => GhAuthPlan::Skip,
        KeyringState::Unlocked | KeyringState::Unknown => GhAuthPlan::Run,
    }
}

fn gh_auth_token(error_out: &mut Option<String>) -> Option<String> {
    let interactive =
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let plan = plan_gh_auth(
        gh_has_token_without_keyring(),
        secret_service_keyring_state,
        interactive,
    );
    debug!("`gh auth token` plan: {plan:?}");
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".into());
    let result = match plan {
        GhAuthPlan::Run => run_command_with_timeout(
            "gh",
            GH_AUTH_ARGS,
            GH_AUTH_TIMEOUT,
            Some(
                "waiting for `gh auth token` (system keyring / D-Bus may be locked)...",
            ),
        ),
        GhAuthPlan::WaitForUnlock => {
            eprintln!(
                "{}",
                console::style(format!(
                    "The system keyring is locked. Unlock it in your desktop \
                     session (DISPLAY={display}); waiting up to {}s...",
                    GH_AUTH_UNLOCK_TIMEOUT.as_secs()
                ))
                .yellow()
            );
            run_command_with_timeout(
                "gh",
                GH_AUTH_ARGS,
                GH_AUTH_UNLOCK_TIMEOUT,
                None,
            )
        }
        GhAuthPlan::Skip => {
            let msg = format!(
                "not run because the system keyring is locked and there is no \
                 terminal to wait for it to be unlocked; unlock it in your \
                 desktop session (DISPLAY={display})"
            );
            warn!("`gh auth token` {msg}");
            *error_out = Some(msg);
            return None;
        }
    };
    match result {
        Ok(token) => Some(token),
        Err(CommandError::Timeout(after)) => {
            let msg = format!(
                "timed out after {:.1}s (system keyring / D-Bus may be locked or unresponsive)",
                after.as_secs_f32()
            );
            warn!("`gh auth token` {msg}");
            *error_out = Some(msg);
            None
        }
        Err(CommandError::Failed(msg)) => {
            *error_out = Some(msg);
            None
        }
    }
}

/// Whether `gh` can produce a token without reading the keyring: it prefers
/// `$GH_TOKEN`, and a token stored with `--insecure-storage` lives in
/// `hosts.yml`.
fn gh_has_token_without_keyring() -> bool {
    if std::env::var(GH_TOKEN_ENV_VAR)
        .ok()
        .and_then(clean)
        .is_some()
    {
        debug!("${GH_TOKEN_ENV_VAR} is set; `gh` will not need the keyring");
        return true;
    }
    let Some(path) = gh_hosts_file() else {
        return false;
    };
    let plaintext = std::fs::read_to_string(&path)
        .is_ok_and(|content| hosts_yml_has_plaintext_token(&content));
    if plaintext {
        debug!(
            "{} holds a plaintext token; `gh` will not need the keyring",
            path.display()
        );
    }
    plaintext
}

/// Where `gh` keeps `hosts.yml`, following its own lookup order.
fn gh_hosts_file() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let dir = match var("GH_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                var("HOME").map(|home| PathBuf::from(home).join(".config"))
            })?
            .join("gh"),
    };
    Some(dir.join("hosts.yml"))
}

fn hosts_yml_has_plaintext_token(content: &str) -> bool {
    content
        .lines()
        .any(|line| line.trim_start().starts_with("oauth_token:"))
}

/// Ask the Secret Service whether its default collection is locked, giving up
/// after `KEYRING_PROBE_TIMEOUT`.
///
/// The query runs on a thread of its own with a private runtime: the caller is
/// synchronous code inside the main tokio runtime, and if the bus is wedged
/// the thread can simply be abandoned rather than holding anything up.
#[cfg(target_os = "linux")]
fn secret_service_keyring_state() -> KeyringState {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("keyring-probe".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|rt| {
                    rt.block_on(default_collection_locked())
                        .map_err(|e| e.to_string())
                });
            let _ = tx.send(result);
        });
    if let Err(e) = spawned {
        debug!("could not start the keyring probe thread: {e}");
        return KeyringState::Unknown;
    }
    let state = match rx.recv_timeout(KEYRING_PROBE_TIMEOUT) {
        Ok(Ok(true)) => KeyringState::Locked,
        Ok(Ok(false)) => KeyringState::Unlocked,
        Ok(Err(e)) => {
            debug!("could not query the Secret Service lock state: {e}");
            KeyringState::Unknown
        }
        Err(_) => {
            debug!(
                "Secret Service lock state query timed out after {:.1}s",
                KEYRING_PROBE_TIMEOUT.as_secs_f32()
            );
            KeyringState::Unknown
        }
    };
    debug!("Secret Service keyring state: {state:?}");
    state
}

#[cfg(not(target_os = "linux"))]
fn secret_service_keyring_state() -> KeyringState {
    KeyringState::Unknown
}

/// The `Locked` property of the collection `gh` reads its token from.
#[cfg(target_os = "linux")]
async fn default_collection_locked() -> zbus::Result<bool> {
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
        .destination("org.freedesktop.secrets")?
        .path("/org/freedesktop/secrets/aliases/default")?
        .interface("org.freedesktop.Secret.Collection")?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    proxy.get_property("Locked").await
}

#[derive(Debug)]
enum CommandError {
    /// Killed after running for this long.
    Timeout(Duration),
    /// Could not be started, or exited unsuccessfully (with its stderr).
    Failed(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::Timeout(after) => {
                write!(f, "timed out after {:.1}s", after.as_secs_f32())
            }
            CommandError::Failed(msg) => f.write_str(msg),
        }
    }
}

/// Run a command with no stdin and return its stdout, killing it if it is
/// still running after `timeout`. `slow_hint` is printed if it takes longer
/// than `GH_AUTH_WARN_AFTER`.
fn run_command_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
    slow_hint: Option<&str>,
) -> Result<String, CommandError> {
    debug!("running `{program} {}`...", args.join(" "));
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            debug!("could not run `{program}`: {e}");
            CommandError::Failed(format!("could not run `{program}`: {e}"))
        })?;

    let start = Instant::now();
    let mut warned_slow = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr);
                }
                if status.success() {
                    return Ok(stdout);
                }
                let msg = stderr.trim();
                let msg = if msg.is_empty() {
                    format!("exited with {status}")
                } else {
                    msg.to_string()
                };
                debug!("`{program} {}` failed: {msg}", args.join(" "));
                return Err(CommandError::Failed(msg));
            }
            Ok(None) => {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    debug!(
                        "`{program} {}` timed out after {:.1}s",
                        args.join(" "),
                        timeout.as_secs_f32()
                    );
                    return Err(CommandError::Timeout(timeout));
                }
                if let Some(hint) = slow_hint
                    && !warned_slow
                    && elapsed >= GH_AUTH_WARN_AFTER
                {
                    warned_slow = true;
                    eprintln!("{}", console::style(hint).dim());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                debug!("error waiting for `{program}`: {e}");
                return Err(CommandError::Failed(e.to_string()));
            }
        }
    }
}

/// Read the key from the repository's configuration, which git resolves
/// against the global and system files too, so a token set either way is
/// found. Falls back to `spr.githubAuthToken` if `nspr.githubAuthToken` is not
/// set.
fn git_config_token() -> Option<String> {
    let config = git2::Repository::discover(".")
        .and_then(|repo| repo.config())
        .or_else(|_| git2::Config::open_default())
        .ok()?;
    if let Ok(val) = config.get_string(GIT_CONFIG_KEY)
        && let Some(cleaned) = clean(val)
    {
        debug!("using GitHub token from git config `{GIT_CONFIG_KEY}`");
        return Some(cleaned);
    }
    if let Ok(val) = config.get_string(LEGACY_SPR_GIT_CONFIG_KEY)
        && let Some(cleaned) = clean(val)
    {
        debug!(
            "`{GIT_CONFIG_KEY}` not set; falling back to legacy git config `{LEGACY_SPR_GIT_CONFIG_KEY}`"
        );
        return Some(cleaned);
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn none() -> Option<String> {
        None
    }

    #[test]
    fn prefers_the_nspr_variable_over_everything_else() {
        let token = first_token(
            |name| Some(name.to_string()),
            || Some("config".to_string()),
            || Some("gh".to_string()),
        );
        assert_eq!(token.as_deref(), Some(TOKEN_ENV_VAR));
    }

    #[test]
    fn falls_back_through_the_chain_in_order() {
        let github_token_only =
            |name: &str| (name == FALLBACK_ENV_VAR).then(|| "env".to_string());
        assert_eq!(
            first_token(
                github_token_only,
                || Some("config".into()),
                || Some("gh".into()),
            )
            .as_deref(),
            Some("env")
        );
        assert_eq!(
            first_token(
                |_| None,
                || Some("config".into()),
                || Some("gh".into()),
            )
            .as_deref(),
            Some("config")
        );
        assert_eq!(
            first_token(|_| None, none, || Some("gh".into())).as_deref(),
            Some("gh")
        );
        assert_eq!(first_token(|_| None, none, none), None);
    }

    #[test]
    fn treats_a_blank_source_as_absent() {
        let blank_env =
            |name: &str| (name == FALLBACK_ENV_VAR).then(|| "  \n".to_string());
        assert_eq!(
            first_token(
                blank_env,
                || Some("  \n".into()),
                || { Some("gh\n".into()) }
            )
            .as_deref(),
            Some("gh")
        );
    }

    #[test]
    fn hung_command_times_out_quickly() {
        let start = Instant::now();
        let res = run_command_with_timeout(
            "sleep",
            &["10"],
            Duration::from_millis(100),
            None,
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "expected fast timeout, took {:?}",
            start.elapsed()
        );
        assert!(
            matches!(res, Err(CommandError::Timeout(_))),
            "expected a timeout, got: {res:?}"
        );
    }

    #[test]
    fn missing_command_is_reported_as_a_failure() {
        let res = run_command_with_timeout(
            "nspr-test-no-such-command",
            &[],
            Duration::from_secs(1),
            None,
        );
        assert!(
            matches!(&res, Err(CommandError::Failed(msg)) if msg.contains("could not run")),
            "expected a spawn failure, got: {res:?}"
        );
    }

    #[test]
    fn keyring_is_not_probed_when_gh_has_a_token_of_its_own() {
        let probe =
            || -> KeyringState { panic!("keyring should not be probed") };
        assert_eq!(plan_gh_auth(true, probe, false), GhAuthPlan::Run);
    }

    #[test]
    fn locked_keyring_waits_in_a_terminal_and_is_skipped_otherwise() {
        let locked = || KeyringState::Locked;
        assert_eq!(
            plan_gh_auth(false, locked, true),
            GhAuthPlan::WaitForUnlock
        );
        assert_eq!(plan_gh_auth(false, locked, false), GhAuthPlan::Skip);
    }

    #[test]
    fn unlocked_or_unknown_keyring_runs_gh_normally() {
        for state in [KeyringState::Unlocked, KeyringState::Unknown] {
            for interactive in [true, false] {
                assert_eq!(
                    plan_gh_auth(false, || state, interactive),
                    GhAuthPlan::Run,
                    "{state:?}, interactive={interactive}"
                );
            }
        }
    }

    #[test]
    fn detects_plaintext_tokens_in_gh_hosts_file() {
        let keyring_only = "github.com:\n    git_protocol: ssh\n    users:\n        octocat:\n    user: octocat\n";
        assert!(!hosts_yml_has_plaintext_token(keyring_only));
        let plaintext =
            "github.com:\n    oauth_token: gho_abc\n    user: octocat\n";
        assert!(hosts_yml_has_plaintext_token(plaintext));
    }

    /// Guards the two variables this test writes. `set_var` is `unsafe` as of
    /// edition 2024 because a concurrent reader of the environment sees a
    /// torn view; no other test in this crate reads these two variables, and
    /// this one holds the lock for the whole window in which they are set.
    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn reads_the_real_environment() {
        let _guard = ENV.lock().unwrap();
        // SAFETY: see `ENV`.
        unsafe {
            std::env::set_var(TOKEN_ENV_VAR, "from-nspr-var");
            std::env::set_var(FALLBACK_ENV_VAR, "from-github-var");
        }

        let env = |name: &str| std::env::var(name).ok();
        assert_eq!(
            first_token(env, none, none).as_deref(),
            Some("from-nspr-var")
        );

        // SAFETY: see `ENV`.
        unsafe {
            std::env::remove_var(TOKEN_ENV_VAR);
        }
        let env = |name: &str| std::env::var(name).ok();
        assert_eq!(
            first_token(env, none, none).as_deref(),
            Some("from-github-var")
        );

        // SAFETY: see `ENV`.
        unsafe {
            std::env::remove_var(FALLBACK_ENV_VAR);
        }
    }
}
