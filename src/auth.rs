//! Finding a GitHub API token.
//!
//! Four sources, first hit wins. The environment comes first so that CI and
//! one-off overrides do not have to touch any configuration; `gh auth token`
//! comes before git config because almost everyone already has `gh` logged in,
//! and a token that `gh` refreshes is one the user never has to rotate by hand.
//!
//! An empty value is treated as absent rather than as an answer: CI images
//! routinely export `GITHUB_TOKEN=` when no secret is available, and stopping
//! there would mean failing with "bad credentials" instead of falling through
//! to a token that works.

use std::process::Command;

use color_eyre::eyre::{Result, bail};
use log::debug;

/// Checked before `GITHUB_TOKEN` so that a token scoped to nspr can override
/// whatever else the environment happens to be carrying.
pub const TOKEN_ENV_VAR: &str = "NSPR_GITHUB_TOKEN";
const FALLBACK_ENV_VAR: &str = "GITHUB_TOKEN";
const GIT_CONFIG_KEY: &str = "nspr.githubAuthToken";

/// The token to authenticate with, or an error explaining how to get one.
pub fn github_token() -> Result<String> {
    match first_token(
        |name| std::env::var(name).ok(),
        gh_auth_token,
        git_config_token,
    ) {
        Some(token) => Ok(token),
        None => bail!(
            "no GitHub token found. Run `gh auth login`, or set \
             ${TOKEN_ENV_VAR}, or set `{GIT_CONFIG_KEY}` in your git config."
        ),
    }
}

/// The source chain, with its inputs injected so it can be tested.
fn first_token<E, G, C>(env: E, gh: G, git_config: C) -> Option<String>
where
    E: Fn(&str) -> Option<String>,
    G: FnOnce() -> Option<String>,
    C: FnOnce() -> Option<String>,
{
    env(TOKEN_ENV_VAR)
        .and_then(clean)
        .or_else(|| env(FALLBACK_ENV_VAR).and_then(clean))
        .or_else(|| gh().and_then(clean))
        .or_else(|| git_config().and_then(clean))
}

/// Trailing newlines come with `gh auth token`, and a token with one attached
/// produces an HTTP header GitHub rejects as malformed.
fn clean(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn gh_auth_token() -> Option<String> {
    let output = Command::new("gh").args(["auth", "token"]).output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout).ok()
        }
        Ok(output) => {
            debug!(
                "`gh auth token` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            None
        }
        Err(e) => {
            debug!("could not run `gh`: {e}");
            None
        }
    }
}

/// Read the key from the repository's configuration, which git resolves
/// against the global and system files too, so a token set either way is
/// found.
fn git_config_token() -> Option<String> {
    let config = git2::Repository::discover(".")
        .and_then(|repo| repo.config())
        .or_else(|_| git2::Config::open_default())
        .ok()?;
    config.get_string(GIT_CONFIG_KEY).ok()
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
            || Some("gh".to_string()),
            || Some("config".to_string()),
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
                || Some("gh".into()),
                || Some("config".into())
            )
            .as_deref(),
            Some("env")
        );
        assert_eq!(
            first_token(
                |_| None,
                || Some("gh".into()),
                || Some("config".into())
            )
            .as_deref(),
            Some("gh")
        );
        assert_eq!(
            first_token(|_| None, none, || Some("config".into())).as_deref(),
            Some("config")
        );
        assert_eq!(first_token(|_| None, none, none), None);
    }

    #[test]
    fn treats_a_blank_source_as_absent() {
        let blank_env =
            |name: &str| (name == FALLBACK_ENV_VAR).then(|| "  \n".to_string());
        assert_eq!(
            first_token(blank_env, || Some("gh\n".into()), none).as_deref(),
            Some("gh")
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
