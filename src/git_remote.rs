/*
 * Portions of this file are derived from spr (https://github.com/spacedentist/spr)
 * Copyright (c) Radical HQ Limited, MIT licensed.
 * Modified for nspr: the surface is narrowed to the three operations the forge
 * needs, fetches skip objects already in the object database, pushes take raw
 * refspecs so that the caller — not this module — decides whether a `+` is
 * warranted, and credentials are delegated to `auth-git2` rather than
 * hand-rolled (see below).
 */

//! Authenticated git transport to the GitHub remote.
//!
//! Neither REST nor GraphQL can move git objects, so every push and every
//! object fetch goes through libgit2, which needs a credentials callback.
//!
//! # Why `auth-git2` and not our own callback
//!
//! spr answers the callback itself: ssh-agent for `git@` remotes, the API
//! token as an HTTP password otherwise. That is not enough, and the failure is
//! not hypothetical — it hung this tool against a real repository:
//!
//! * libgit2 re-invokes the callback until it either authenticates or the
//!   callback returns an error. Answering "ssh-agent" forever, when there is no
//!   agent, is an infinite loop rather than a diagnosis.
//! * It never tries the key files in `~/.ssh`, so a user with a perfectly good
//!   `id_ed25519` but no running agent cannot push.
//! * It ignores `credential.helper`. Anyone who has run `gh auth setup-git`
//!   has one configured, and it is the credential they expect to be used.
//! * `pushInsteadOf` means the fetch and push URLs can use different
//!   transports, so "which credential" is not a property of the remote we were
//!   configured with.
//!
//! `auth-git2` handles all of that, and bounds its attempts.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use auth_git2::GitAuthenticator;
use color_eyre::eyre::{Result, WrapErr as _, bail};
use git2::{Direction, Oid, PushOptions, RemoteCallbacks, Repository};
use log::{trace, warn};

use crate::ssh_agent::{self, SshAgentStatus};

#[derive(Clone)]
pub struct GitRemote {
    repo: Arc<Repository>,
    url: String,
    auth_token: String,
    ssh_agent_timeout: Duration,
    ssh_auth_sock_override: Option<PathBuf>,
}

impl GitRemote {
    pub fn new(repo: Arc<Repository>, url: String, auth_token: String) -> Self {
        Self {
            repo,
            url,
            auth_token,
            ssh_agent_timeout: ssh_agent::DEFAULT_SSH_AGENT_TIMEOUT,
            ssh_auth_sock_override: None,
        }
    }

    /// Override the SSH agent probe timeout (useful for fast-failing tests).
    pub fn with_ssh_agent_timeout(mut self, timeout: Duration) -> Self {
        self.ssh_agent_timeout = timeout;
        self
    }

    /// Override the SSH agent socket path (useful for tests).
    pub fn with_ssh_agent_socket(mut self, path: PathBuf) -> Self {
        self.ssh_auth_sock_override = Some(path);
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn check_ssh_agent(&self) -> SshAgentStatus {
        if let Some(path) = &self.ssh_auth_sock_override {
            ssh_agent::probe_ssh_agent(path, self.ssh_agent_timeout)
        } else {
            ssh_agent::check_ssh_agent(self.ssh_agent_timeout)
        }
    }

    /// The credential chain.
    ///
    /// `auth-git2` picks by transport, not by preference order here: an ssh
    /// remote is never offered a password, so it tries the agent and then the
    /// key files in `~/.ssh`; an https remote is never offered a key. Within
    /// https it does try our token before the credential helper, which is what
    /// we want — the token is the identity that opened the pull request over
    /// the API, so the push should carry the same one.
    ///
    /// To prevent hanging indefinitely on wedged or unresponsive SSH agents,
    /// `ssh-agent` is only offered if our bounded probe verified that the
    /// agent is responsive.
    fn authenticator(&self, ssh_status: &SshAgentStatus) -> GitAuthenticator {
        let mut auth = GitAuthenticator::default()
            .try_cred_helper(true)
            .add_default_ssh_keys()
            .prompt_ssh_key_password(false)
            .add_plaintext_credentials("github.com", "nspr", &self.auth_token);

        if ssh_status.is_available() {
            auth = auth.try_ssh_agent(true);
        }

        auth
    }

    fn with_remote<F, T>(
        &self,
        dir: Direction,
        action_desc: &str,
        func: F,
    ) -> Result<T>
    where
        F: FnOnce(&mut git2::Remote, RemoteCallbacks) -> Result<T>,
    {
        use std::io::Write as _;

        let config = self.repo.config()?;
        let effective_url = ssh_agent::resolve_effective_url(
            &config,
            &self.url,
            dir == Direction::Push,
        );
        let is_ssh = ssh_agent::is_ssh_url(&effective_url);
        let ssh_status = if is_ssh {
            self.check_ssh_agent()
        } else {
            SshAgentStatus::NotConfigured
        };

        eprintln!("git {action_desc} ({effective_url})...");
        let _ = std::io::stderr().flush();

        let mut remote = self.repo.remote_anonymous(&self.url)?;
        let auth = self.authenticator(&ssh_status);
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(auth.credentials(&config));

        func(&mut remote, callbacks).wrap_err_with(|| {
            if is_ssh {
                ssh_agent::format_ssh_auth_error(
                    &effective_url,
                    &self.url,
                    &ssh_status,
                )
            } else {
                format!(
                    "could not connect to {}. Check `gh auth status`, and \
                     that you can reach the repository. Note that `git \
                     remote -v` may show a different push URL: \
                     `url.*.pushInsteadOf` in your git config rewrites the \
                     transport, and credentials that work for one transport \
                     do not apply to the other.",
                    &self.url
                )
            }
        })
    }

    /// Every branch on the remote and the commit it points at.
    pub fn branches(&self) -> Result<HashMap<String, Oid>> {
        self.with_remote(Direction::Fetch, "ls-remote", |remote, callbacks| {
            let mut conn =
                remote.connect_auth(Direction::Fetch, Some(callbacks), None)?;
            Ok(conn
                .remote()
                .list()?
                .iter()
                .filter(|head| !head.oid().is_zero())
                .filter_map(|head| {
                    head.name()
                        .strip_prefix("refs/heads/")
                        .map(|branch| (branch.to_string(), head.oid()))
                })
                .collect())
        })
    }

    /// Make `oids` readable from the local object database.
    pub fn fetch_objects(&self, oids: &[Oid]) -> Result<()> {
        let wanted: Vec<String> = oids
            .iter()
            .filter(|&&oid| !self.has_object(oid))
            .map(Oid::to_string)
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }

        let desc = format!("fetch: {} commit(s)", wanted.len());
        self.with_remote(Direction::Fetch, &desc, |remote, callbacks| {
            let mut conn =
                remote.connect_auth(Direction::Fetch, Some(callbacks), None)?;
            let mut options = git2::FetchOptions::new();
            options
                .update_fetchhead(false)
                .download_tags(git2::AutotagOption::None);
            conn.remote().download(&wanted, Some(&mut options))?;
            Ok(())
        })?;

        for &oid in oids {
            if !self.has_object(oid) {
                bail!(
                    "{oid} is not reachable on {}. If it was just merged, \
                     wait a moment and run `nspr sync`.",
                    self.url
                );
            }
        }
        Ok(())
    }

    fn has_object(&self, oid: Oid) -> bool {
        self.repo.find_object(oid, None).is_ok()
    }

    /// Push raw refspecs, failing if the remote rejects any of them.
    pub fn push(&self, refspecs: &[String]) -> Result<()> {
        if refspecs.is_empty() {
            return Ok(());
        }
        let specs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        let desc = format!("push: {}", describe_push_refspecs(refspecs));
        self.with_remote(Direction::Push, &desc, |remote, mut callbacks| {
            callbacks.push_update_reference(|reference, status| match status {
                Some(status) => {
                    warn!("{reference} rejected: {status}");
                    Err(git2::Error::from_str(&format!(
                        "{reference} rejected: {status}"
                    )))
                }
                None => {
                    trace!("pushed {reference}");
                    Ok(())
                }
            });
            let mut options = PushOptions::new();
            options.remote_callbacks(callbacks);
            Ok(remote.push(&specs, Some(&mut options))?)
        })
    }
}

fn describe_push_refspecs(refspecs: &[String]) -> String {
    let items: Vec<String> = refspecs
        .iter()
        .map(|spec| {
            if let Some(branch) = spec.strip_prefix(":refs/heads/") {
                format!("delete {branch}")
            } else if let Some((_, branch)) = spec
                .strip_prefix('+')
                .and_then(|s| s.split_once(":refs/heads/"))
            {
                format!("+{branch}")
            } else if let Some((_, branch)) = spec.split_once(":refs/heads/") {
                branch.to_string()
            } else {
                spec.clone()
            }
        })
        .collect();
    let count = items.len();
    let noun = if count == 1 { "branch" } else { "branches" };
    format!("{count} {noun} ({})", items.join(", "))
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn connection_failure_on_ssh_remote_diagnoses_missing_ssh_agent() {
        let t = crate::testutil::TestRepo::new();
        let repo = Arc::new(t.open());
        let missing_sock =
            PathBuf::from("/tmp/nspr-test-nonexistent-socket.sock");

        let remote = GitRemote::new(
            repo,
            "git@github.com:o/r.git".into(),
            "fake-token".into(),
        )
        .with_ssh_agent_socket(missing_sock)
        .with_ssh_agent_timeout(Duration::from_millis(50));

        let err = remote.branches().unwrap_err().to_string();
        assert!(
            err.contains(
                "SSH authentication failed connecting to git@github.com:o/r.git"
            ),
            "expected SSH authentication error, got: {err}"
        );
        assert!(
            err.contains(
                "could not connect to SSH agent at '/tmp/nspr-test-nonexistent-socket.sock'"
            ),
            "expected missing agent socket diagnosis, got: {err}"
        );
        assert!(err.contains("ssh-add"));
        assert!(err.contains("ssh -T git@github.com"));
    }

    #[test]
    fn connection_failure_on_ssh_remote_diagnoses_timed_out_ssh_agent() {
        let t = crate::testutil::TestRepo::new();
        let repo = Arc::new(t.open());

        let dir = tempfile::tempdir().unwrap();
        let hung_sock = dir.path().join("hung.sock");
        let listener = UnixListener::bind(&hung_sock).unwrap();

        // Spawn mock agent that accepts connection and hangs without replying
        let handle = thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                thread::sleep(Duration::from_millis(500));
            }
        });

        let remote = GitRemote::new(
            repo,
            "git@github.com:o/r.git".into(),
            "fake-token".into(),
        )
        .with_ssh_agent_socket(hung_sock)
        .with_ssh_agent_timeout(Duration::from_millis(50));

        let start = std::time::Instant::now();
        let err = remote.branches().unwrap_err().to_string();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(350),
            "must not hang indefinitely: took {elapsed:?}"
        );
        assert!(
            err.contains("timed out after 0.1s"),
            "expected timeout diagnosis, got: {err}"
        );
        assert!(
            err.contains("Make sure your SSH agent is responsive"),
            "expected responsive advice, got: {err}"
        );

        let _ = handle.join();
    }

    #[test]
    fn push_instead_of_rewrite_is_explained_in_ssh_agent_error() {
        let t = crate::testutil::TestRepo::new();
        let repo = t.open();
        let mut config = repo.config().unwrap();
        config
            .set_str("url.git@github.com:.pushInsteadOf", "https://github.com/")
            .unwrap();
        drop(config);

        let missing_sock =
            PathBuf::from("/tmp/nspr-test-nonexistent-rewrite.sock");
        let remote = GitRemote::new(
            Arc::new(repo),
            "https://github.com/o/r.git".into(),
            "fake-token".into(),
        )
        .with_ssh_agent_socket(missing_sock)
        .with_ssh_agent_timeout(Duration::from_millis(50));

        let err = remote
            .push(&["refs/heads/main".into()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "rewritten from 'https://github.com/o/r.git' via git config"
            ),
            "expected rewrite explanation, got: {err}"
        );
        assert!(
            err.contains("could not connect to SSH agent"),
            "expected agent diagnosis, got: {err}"
        );
    }

    #[test]
    fn describe_push_refspecs_formats_branches_and_actions() {
        assert_eq!(
            describe_push_refspecs(&["abcd:refs/heads/users/me/a".into()]),
            "1 branch (users/me/a)"
        );
        assert_eq!(
            describe_push_refspecs(&[
                "abcd:refs/heads/users/me/a".into(),
                "+ef01:refs/heads/users/me/b".into(),
                ":refs/heads/users/me/old".into(),
            ]),
            "3 branches (users/me/a, +users/me/b, delete users/me/old)"
        );
    }
}
