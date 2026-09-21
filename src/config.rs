//! Repository and user configuration.

use color_eyre::eyre::{Result, bail};

use crate::forge::RepoMergeSettings;
use crate::utils::slugify;

/// Controls whether `nspr` pushes incremental `[nspr]` revision commits
/// (`true`) or rewrites each PR branch as a single commit with force-pushes
/// (`false`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreserveCommitHistory {
    /// Use incremental `[nspr]` commits when the repository is configured for
    /// squash-only merging with `PR_TITLE` + `PR_BODY`, and fall back to
    /// single-commit force-pushes (with a CLI warning) otherwise.
    #[default]
    Auto,
    /// Always push incremental `[nspr]` commits without force-pushing.
    True,
    /// Always rewrite each PR branch as a single commit and force-push.
    False,
}

impl PreserveCommitHistory {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "true" | "yes" | "1" | "on" => Ok(Self::True),
            "false" | "no" | "0" | "off" => Ok(Self::False),
            other => bail!(
                "invalid value `{other}` for `nspr.preserveCommitHistory`: \
                 expected `auto`, `true`, or `false`"
            ),
        }
    }

    pub fn resolve(self, merge_settings: RepoMergeSettings) -> bool {
        match self {
            Self::Auto => merge_settings.is_squash_only(),
            Self::True => true,
            Self::False => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub owner: String,
    pub repo: String,
    /// Trunk branch name, e.g. `main`.
    pub trunk: String,
    /// The authenticated user's GitHub login.
    pub login: String,
    /// Prefix for generated branch names. Defaults to `users/<login>/`.
    pub branch_prefix: String,
    /// Emit `.git/gh-stack` metadata for GitHub's native stack tooling.
    pub emit_gh_stack_metadata: bool,
    /// Post a stack-table comment on each pull request.
    pub stack_comments: bool,
    /// `nspr.preserveCommitHistory` (`auto`, `true`, `false`).
    pub preserve_commit_history: PreserveCommitHistory,
}

impl Config {
    pub fn new(
        owner: String,
        repo: String,
        trunk: String,
        login: String,
    ) -> Self {
        let branch_prefix = format!("users/{login}/");
        Self {
            owner,
            repo,
            trunk,
            login,
            branch_prefix,
            emit_gh_stack_metadata: true,
            stack_comments: true,
            preserve_commit_history: PreserveCommitHistory::Auto,
        }
    }

    pub fn pull_request_url(&self, number: u64) -> String {
        format!(
            "https://github.com/{}/{}/pull/{number}",
            self.owner, self.repo
        )
    }

    /// Preferred branch name for a layer with the given subject.
    pub fn branch_name_for(&self, subject: &str) -> String {
        format!("{}{}", self.branch_prefix, slugify(subject))
    }
}

/// Split an `owner/repo` string.
pub fn parse_repo_slug(slug: &str) -> Result<(String, String)> {
    let (owner, repo) = slug.split_once('/').ok_or_else(|| {
        color_eyre::eyre::eyre!("expected `owner/repo`, got `{slug}`")
    })?;
    Ok((owner.to_string(), repo.trim_end_matches(".git").to_string()))
}

/// Pull `owner/repo` out of a git remote URL.
///
/// GitHub hands out four shapes of URL and people paste all of them, so this
/// accepts the lot rather than making the user configure the slug by hand.
pub fn parse_remote_url(url: &str) -> Result<(String, String)> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))
        .ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "`{url}` does not look like a GitHub remote. Set \
                 `nspr.repository` to `owner/repo` if you are using a GitHub \
                 Enterprise host."
            )
        })?;
    parse_repo_slug(rest.trim_end_matches('/'))
}

/// Which GitHub repository this checkout belongs to.
///
/// Separate from [`detect`] because the caller needs this *before* it can talk
/// to GitHub, and it cannot learn its own login until it has.
pub fn detect_repo(
    git: &crate::git::Git,
    remote: &str,
) -> Result<(String, String)> {
    let cfg = git.repo().config()?;
    if let Ok(slug) = cfg.get_string("nspr.repository")
        && !slug.is_empty()
    {
        return parse_repo_slug(&slug);
    }
    let url = cfg.get_string(&format!("remote.{remote}.url")).map_err(
        |_| {
            color_eyre::eyre::eyre!(
                "no `{remote}` remote. Add one, or set `nspr.repository` to \
                 `owner/repo`."
            )
        },
    )?;
    parse_remote_url(&url)
}

/// Work out the configuration from the repository, so the common case needs no
/// setup at all.
///
/// Every field can be overridden through git config; `login` cannot be
/// detected locally and must be supplied by the caller, which gets it from the
/// forge.
pub fn detect(
    git: &crate::git::Git,
    login: String,
    remote: &str,
) -> Result<Config> {
    let cfg = git.repo().config()?;
    let get = |key: &str| cfg.get_string(key).ok().filter(|v| !v.is_empty());

    let (owner, repo) = detect_repo(git, remote)?;

    // Prefer what the remote says its default branch is over guessing `main`:
    // plenty of repositories are still on `master`, and some use neither.
    let trunk = get("nspr.trunk")
        .or_else(|| {
            let head = format!("refs/remotes/{remote}/HEAD");
            let reference = git.repo().find_reference(&head).ok()?;
            // git2 0.21 reports a non-UTF-8 target as an error rather than
            // folding it into `None`; either way we have nothing usable.
            reference
                .symbolic_target()
                .ok()
                .flatten()?
                .strip_prefix(&format!("refs/remotes/{remote}/"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "main".to_string());

    let mut config = Config::new(owner, repo, trunk, login);
    if let Some(prefix) = get("nspr.branchPrefix") {
        config.branch_prefix = prefix;
    }
    if let Ok(v) = cfg.get_bool("nspr.emitGhStackMetadata") {
        config.emit_gh_stack_metadata = v;
    }
    if let Ok(v) = cfg.get_bool("nspr.stackComments") {
        config.stack_comments = v;
    }
    if let Some(raw) = get("nspr.preserveCommitHistory") {
        config.preserve_commit_history = PreserveCommitHistory::parse(&raw)?;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_use_the_users_prefix() {
        let c = Config::new(
            "o".into(),
            "r".into(),
            "main".into(),
            "arichardson".into(),
        );
        assert_eq!(
            c.branch_name_for("Add the widget cache!"),
            "users/arichardson/add-the-widget-cache"
        );
    }

    #[test]
    fn parses_every_shape_of_github_remote() {
        let expected = ("o".to_string(), "r".to_string());
        for url in [
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "https://github.com/o/r.git",
            "https://github.com/o/r",
            "git://github.com/o/r.git",
            "  https://github.com/o/r/  ",
        ] {
            assert_eq!(parse_remote_url(url).unwrap(), expected, "{url}");
        }

        // A non-GitHub host is not silently mangled into a wrong slug.
        assert!(parse_remote_url("git@gitlab.com:o/r.git").is_err());
    }

    #[test]
    fn parses_repo_slugs() {
        assert_eq!(
            parse_repo_slug("o/r.git").unwrap(),
            ("o".to_string(), "r".to_string())
        );
        assert!(parse_repo_slug("nope").is_err());
    }
}
