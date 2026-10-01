//! Finding a GitHub API token.
//!
//! Five sources, first hit wins. Fast local sources (environment variables and
//! git config) are checked before spawning `gh auth token` because `gh` queries
//! the system keyring over D-Bus, which can hang for 60+ seconds in headless or
//! SSH sessions when the keyring daemon is locked or unresponsive.
//!
//! An empty value is treated as absent rather than as an answer: CI images
//! routinely export `GITHUB_TOKEN=` when no secret is available, and stopping
//! there would mean failing with "bad credentials" instead of falling through
//! to a token that works.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, bail};
use log::{debug, warn};

/// Checked before `GITHUB_TOKEN` so that a token scoped to nspr can override
/// whatever else the environment happens to be carrying.
pub const TOKEN_ENV_VAR: &str = "NSPR_GITHUB_TOKEN";
const FALLBACK_ENV_VAR: &str = "GITHUB_TOKEN";
const GIT_CONFIG_KEY: &str = "nspr.githubAuthToken";
const LEGACY_SPR_GIT_CONFIG_KEY: &str = "spr.githubAuthToken";

/// Maximum time to wait for `gh auth token` before killing it and failing fast.
const GH_AUTH_TIMEOUT: Duration = Duration::from_secs(3);
/// Delay before printing a visible hint that `gh auth token` is taking a while.
const GH_AUTH_WARN_AFTER: Duration = Duration::from_millis(500);

/// The token to authenticate with, or an error explaining how to get one.
pub fn github_token() -> Result<String> {
    let mut gh_error: Option<String> = None;
    match first_token(
        |name| std::env::var(name).ok(),
        git_config_token,
        || gh_auth_token(GH_AUTH_TIMEOUT, &mut gh_error),
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
    if let Some(t) = env(FALLBACK_ENV_VAR).and_then(clean) {
        debug!("using GitHub token from ${FALLBACK_ENV_VAR}");
        return Some(t);
    }
    if let Some(t) = git_config().and_then(clean) {
        return Some(t);
    }
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

fn gh_auth_token(
    timeout: Duration,
    error_out: &mut Option<String>,
) -> Option<String> {
    run_token_command("gh", &["auth", "token"], timeout, error_out)
}

fn run_token_command(
    program: &str,
    args: &[&str],
    timeout: Duration,
    error_out: &mut Option<String>,
) -> Option<String> {
    debug!("running `{program} {}`...", args.join(" "));
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            debug!("could not run `{program}`: {e}");
            *error_out = Some(format!("could not run `{program}`: {e}"));
            return None;
        }
    };

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
                    return Some(stdout);
                }
                let msg = stderr.trim();
                let msg = if msg.is_empty() {
                    format!("exited with {status}")
                } else {
                    msg.to_string()
                };
                debug!("`{program} {}` failed: {msg}", args.join(" "));
                *error_out = Some(msg);
                return None;
            }
            Ok(None) => {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let msg = format!(
                        "timed out after {:.1}s (system keyring / D-Bus may be locked or unresponsive)",
                        timeout.as_secs_f32()
                    );
                    warn!("`{program} {}` {msg}", args.join(" "));
                    *error_out = Some(msg);
                    return None;
                }
                if !warned_slow && elapsed >= GH_AUTH_WARN_AFTER {
                    warned_slow = true;
                    eprintln!(
                        "{}",
                        console::style(
                            "waiting for `gh auth token` (system keyring / D-Bus may be locked)..."
                        )
                        .dim()
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                debug!("error waiting for `{program}`: {e}");
                *error_out = Some(e.to_string());
                return None;
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
    for key in [GIT_CONFIG_KEY, LEGACY_SPR_GIT_CONFIG_KEY] {
        if let Ok(val) = config.get_string(key)
            && let Some(cleaned) = clean(val)
        {
            debug!("using GitHub token from git config `{key}`");
            return Some(cleaned);
        }
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
    fn hung_token_command_times_out_quickly() {
        let mut err = None;
        let start = Instant::now();
        let res = run_token_command(
            "sleep",
            &["10"],
            Duration::from_millis(100),
            &mut err,
        );
        assert!(res.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "expected fast timeout, took {:?}",
            start.elapsed()
        );
        let msg = err.expect("expected timeout error message");
        assert!(
            msg.contains("timed out"),
            "expected timeout message, got: {msg}"
        );
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
