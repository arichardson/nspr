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

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use auth_git2::GitAuthenticator;
use color_eyre::eyre::{Result, bail};
use git2::{Direction, Oid, PushOptions, RemoteCallbacks, Repository};
use log::trace;

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
            .try_cred_helper(self.auth_token.is_empty())
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

        log::debug!("git {action_desc} ({effective_url})...");
        if !log::log_enabled!(log::Level::Debug) && dir == Direction::Push {
            eprintln!(
                "{}",
                console::style(format!(
                    "git {action_desc} ({effective_url})..."
                ))
                .dim()
            );
            let _ = std::io::stderr().flush();
        }

        let mut remote = self.repo.remote_anonymous(&self.url)?;
        let auth = self.authenticator(&ssh_status);
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(auth.credentials(&config));

        // A refused ref update means the connection and credentials worked, so
        // the transport diagnosis below would only mislead.
        let res = func(&mut remote, callbacks).map_err(|e| {
            if e.downcast_ref::<PushRejected>().is_some() {
                return e;
            }
            e.wrap_err(if is_ssh {
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
            })
        });
        if res.is_ok() {
            log::debug!("  -> git {action_desc} finished");
        }
        res
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

    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Make `oids` readable from the local object database.
    pub fn fetch_objects(&self, oids: &[Oid]) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let wanted: Vec<String> = oids
            .iter()
            .filter(|&&oid| {
                oid != Oid::ZERO_SHA1
                    && seen.insert(oid)
                    && !self.has_object(oid)
            })
            .map(Oid::to_string)
            .collect();
        if wanted.is_empty() {
            if !seen.is_empty() {
                log::debug!(
                    "fetch_objects: all {} requested commit(s) already present locally",
                    seen.len()
                );
            }
            return Ok(());
        }

        log::debug!(
            "fetching {} missing commit(s) from remote: {}",
            wanted.len(),
            wanted.join(", ")
        );
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
            if oid != Oid::ZERO_SHA1 && !self.has_object(oid) {
                bail!(
                    "{oid} is not reachable on {}. If it was just merged, \
                     wait a moment and run `nspr sync`.",
                    self.url
                );
            }
        }
        Ok(())
    }

    pub fn has_object(&self, oid: Oid) -> bool {
        self.repo.find_object(oid, None).is_ok()
    }

    /// Push raw refspecs, failing if the remote rejects any of them.
    pub fn push(&self, refspecs: &[String]) -> Result<()> {
        self.push_with_desc(refspecs, None)
    }

    /// Push raw refspecs with an optional custom progress description.
    ///
    /// If the remote refuses the push because it updates too many refs at
    /// once (GitHub's `max_ref_updates` push rule, which is invisible to
    /// non-admins through the API), the refused refspecs are pushed again in
    /// batches of the size the server named.
    pub fn push_with_desc(
        &self,
        refspecs: &[String],
        custom_desc: Option<&str>,
    ) -> Result<()> {
        let err = match self.push_once(refspecs, custom_desc) {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        let Some(rejected) = err.downcast_ref::<PushRejected>() else {
            return Err(err);
        };
        let Some(limit) = rejected.max_ref_updates() else {
            return Err(err);
        };
        let retry = refspecs_to_retry(refspecs, rejected);
        if limit == 0 || retry.len() <= limit {
            return Err(err);
        }
        eprintln!(
            "{}",
            console::style(format!(
                "the remote accepts at most {limit} branch updates per push; \
                 pushing the remaining {} in batches",
                retry.len()
            ))
            .dim()
        );
        for batch in retry.chunks(limit) {
            self.push_once(batch, custom_desc)?;
        }
        Ok(())
    }

    fn push_once(
        &self,
        refspecs: &[String],
        custom_desc: Option<&str>,
    ) -> Result<()> {
        if refspecs.is_empty() {
            return Ok(());
        }
        let specs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        let desc = if log::log_enabled!(log::Level::Debug) {
            format!("push: {}", describe_push_refspecs(refspecs))
        } else if let Some(d) = custom_desc {
            d.to_string()
        } else {
            let count = refspecs.len();
            let noun = if count == 1 { "branch" } else { "branches" };
            format!("push: {count} {noun}")
        };
        let deleted_refs: std::collections::HashSet<String> = refspecs
            .iter()
            .filter_map(|s| s.strip_prefix(':').map(str::to_owned))
            .collect();
        self.with_remote(Direction::Push, &desc, |remote, mut callbacks| {
            // Rejections are collected rather than failing the callback, so
            // that every refused branch is reported together with what the
            // server printed about it (e.g. which repository rule blocked it).
            let rejected: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
            let sideband: Rc<RefCell<Vec<u8>>> = Rc::default();
            let rejected_cb = Rc::clone(&rejected);
            callbacks.push_update_reference(move |reference, status| {
                match status {
                    Some(status) if deleted_refs.contains(reference) => {
                        log::debug!(
                            "delete of {reference} reported `{status}` (already deleted by remote; ignoring)"
                        );
                    }
                    Some(status) => {
                        log::debug!("  -> {reference} rejected: {status}");
                        rejected_cb
                            .borrow_mut()
                            .push((reference.to_string(), status.to_string()));
                    }
                    None => {
                        log::debug!("  -> pushed {reference}: ok");
                        trace!("pushed {reference}");
                    }
                }
                Ok(())
            });
            let sideband_cb = Rc::clone(&sideband);
            callbacks.sideband_progress(move |data| {
                sideband_cb.borrow_mut().extend_from_slice(data);
                true
            });
            let mut options = PushOptions::new();
            options.remote_callbacks(callbacks);
            let pushed = remote.push(&specs, Some(&mut options));
            let mut rejected = rejected.take();
            // libgit2 refuses a non-forced update it can already tell is not a
            // fast-forward without asking the server; that is the same
            // refusal, not a connection problem.
            if let Err(e) = &pushed
                && e.code() == git2::ErrorCode::NotFastForward
            {
                let candidates: Vec<&str> = specs
                    .iter()
                    .filter(|s| !s.starts_with('+') && !s.starts_with(':'))
                    .filter_map(|s| s.split_once(':').map(|(_, dst)| dst))
                    .collect();
                let reference = match candidates.as_slice() {
                    [one] => (*one).to_string(),
                    many => format!("one of {}", many.join(", ")),
                };
                rejected.push((reference, "non-fast-forward".into()));
            }
            if !rejected.is_empty() {
                return Err(PushRejected {
                    rejected,
                    remote_messages: remote_messages(&sideband.borrow()),
                }
                .into());
            }
            Ok(pushed?)
        })
    }
}

/// The remote was reached and authenticated, but refused to update one or
/// more refs.
///
/// Kept distinct from connection failures so that callers do not bury it under
/// advice about SSH keys or credentials that are evidently working.
#[derive(Debug)]
pub struct PushRejected {
    /// `(ref, reason)` for every refused update.
    pub rejected: Vec<(String, String)>,
    /// What the server printed during the push (git shows these as
    /// `remote: ...`), minus progress meters.
    pub remote_messages: Vec<String>,
}

impl PushRejected {
    /// The per-push ref limit, if that is why every ref was refused: GitHub
    /// explains a `max_ref_updates` violation with "Pushes can not update
    /// more than N branches or tags."
    pub fn max_ref_updates(&self) -> Option<usize> {
        let re = lazy_regex::regex!(
            r"(?i)pushes can ?not update more than (\d+) branches or tags"
        );
        let all_rule_violations = self
            .rejected
            .iter()
            .all(|(_, reason)| reason.contains("rule violation"));
        if !all_rule_violations {
            return None;
        }
        self.remote_messages
            .iter()
            .find_map(|line| re.captures(line)?[1].parse().ok())
    }
    /// Whether the refusal is the ordinary "the branch moved under you" kind,
    /// as opposed to e.g. a repository rule or a server-side hook.
    pub fn is_non_fast_forward(&self) -> bool {
        self.rejected.iter().all(|(_, reason)| {
            let reason = reason.to_ascii_lowercase();
            reason.contains("fast-forward")
                || reason.contains("fastforward")
                || reason.contains("fetch first")
                || reason.contains("stale info")
        })
    }
}

impl std::fmt::Display for PushRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the remote refused to update")?;
        for (reference, reason) in &self.rejected {
            let branch =
                reference.strip_prefix("refs/heads/").unwrap_or(reference);
            write!(f, "\n  {branch}: {reason}")?;
        }
        if !self.remote_messages.is_empty() {
            write!(f, "\nThe remote said:")?;
            for line in &self.remote_messages {
                write!(f, "\n  remote: {line}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for PushRejected {}

/// The refspecs whose destination ref the remote refused, in their original
/// order. Refs the remote accepted are not pushed again.
fn refspecs_to_retry(
    refspecs: &[String],
    rejected: &PushRejected,
) -> Vec<String> {
    refspecs
        .iter()
        .filter(|spec| {
            let dst = spec
                .trim_start_matches('+')
                .split_once(':')
                .map_or(spec.as_str(), |(_, dst)| dst);
            rejected
                .rejected
                .iter()
                .any(|(reference, _)| reference == dst)
        })
        .cloned()
        .collect()
}

/// The human-readable part of the server's sideband output: split into lines,
/// strip terminal control sequences, and drop the progress meters
/// (`Resolving deltas:  50% (1/2)`), which arrive on the same channel.
fn remote_messages(raw: &[u8]) -> Vec<String> {
    let control =
        lazy_regex::regex!(r"\x1b\[[0-9;?]*[A-Za-z]|[\x00-\x08\x0b-\x1f\x7f]");
    let progress = lazy_regex::regex!(r"\d{1,3}%\s*\(\d+/\d+\)");
    let text = String::from_utf8_lossy(raw);
    let mut lines: Vec<String> = Vec::new();
    for line in text.split(['\n', '\r']) {
        let line = control.replace_all(line, "");
        let line = line.trim_end();
        if progress.is_match(line) {
            continue;
        }
        // Keep blank lines that separate paragraphs, but not runs of them.
        if line.trim().is_empty()
            && lines.last().is_none_or(|l| l.trim().is_empty())
        {
            continue;
        }
        lines.push(line.to_string());
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
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
            "ssh://git@127.0.0.1:1/o/r.git".into(),
            "fake-token".into(),
        )
        .with_ssh_agent_socket(missing_sock)
        .with_ssh_agent_timeout(Duration::from_millis(50));

        let err = remote.branches().unwrap_err().to_string();
        assert!(
            err.contains(
                "SSH authentication failed connecting to ssh://git@127.0.0.1:1/o/r.git"
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
            "ssh://git@127.0.0.1:1/o/r.git".into(),
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
            .set_str(
                "url.ssh://git@127.0.0.1:1/.pushInsteadOf",
                "https://127.0.0.1:1/",
            )
            .unwrap();
        drop(config);

        let missing_sock =
            PathBuf::from("/tmp/nspr-test-nonexistent-rewrite.sock");
        let remote = GitRemote::new(
            Arc::new(repo),
            "https://127.0.0.1:1/o/r.git".into(),
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
                "rewritten from 'https://127.0.0.1:1/o/r.git' via git config"
            ),
            "expected rewrite explanation, got: {err}"
        );
        assert!(
            err.contains("could not connect to SSH agent"),
            "expected agent diagnosis, got: {err}"
        );
    }

    #[test]
    fn remote_messages_keep_rule_violations_and_drop_progress() {
        let raw = b"Resolving deltas:   0% (0/3)\rResolving deltas: 100% (3/3), completed with 3 local objects.\n\
error: GH013: Repository rule violations found for refs/heads/users/me/a.\n\
Review all repository rules at https://github.com/o/r/rules?ref=refs%2Fheads%2Fusers%2Fme%2Fa\n\
\n\
- Cannot create ref due to creations being restricted.\n\
\n\
\n";
        assert_eq!(
            remote_messages(raw),
            vec![
                "error: GH013: Repository rule violations found for refs/heads/users/me/a.",
                "Review all repository rules at https://github.com/o/r/rules?ref=refs%2Fheads%2Fusers%2Fme%2Fa",
                "",
                "- Cannot create ref due to creations being restricted.",
            ]
        );
    }

    #[test]
    fn remote_messages_drop_progress_wrapped_in_terminal_escapes() {
        let raw = b"\x1b[K  Resolving deltas:   0% (0/27)\x1b[K\n- Pushes can not update more than 5 branches or tags.\x1b[K\n";
        assert_eq!(
            remote_messages(raw),
            vec!["- Pushes can not update more than 5 branches or tags."]
        );
    }

    #[test]
    fn rule_violation_is_reported_with_the_remote_explanation() {
        let rejected = PushRejected {
            rejected: vec![(
                "refs/heads/users/me/a".into(),
                "push declined due to repository rule violations".into(),
            )],
            remote_messages: vec![
                "- Cannot create ref due to creations being restricted.".into(),
            ],
        };
        assert!(!rejected.is_non_fast_forward());
        assert_eq!(
            rejected.to_string(),
            "the remote refused to update\n  \
             users/me/a: push declined due to repository rule violations\n\
             The remote said:\n  \
             remote: - Cannot create ref due to creations being restricted."
        );
    }

    #[test]
    fn non_fast_forward_is_recognised() {
        let rejected = PushRejected {
            rejected: vec![
                ("refs/heads/a".into(), "non-fast-forward".into()),
                ("refs/heads/b".into(), "fetch first".into()),
            ],
            remote_messages: Vec::new(),
        };
        assert!(rejected.is_non_fast_forward());
    }

    #[test]
    fn non_fast_forward_push_to_a_real_remote_is_a_push_rejection() {
        let t = crate::testutil::TestRepo::new();
        let first = t.commit_file("first", "a.txt", "one", &[]);
        let unrelated = t.commit_file("unrelated", "b.txt", "two", &[]);
        let bare = tempfile::tempdir().unwrap();
        git2::Repository::init_bare(bare.path()).unwrap();
        let remote = GitRemote::new(
            Arc::new(t.open()),
            bare.path().to_str().unwrap().into(),
            String::new(),
        );

        remote
            .push(&[format!("{unrelated}:refs/heads/topic")])
            .unwrap();
        let err = remote
            .push(&[format!("{first}:refs/heads/topic")])
            .unwrap_err();
        let rejected = err
            .downcast_ref::<PushRejected>()
            .unwrap_or_else(|| panic!("expected PushRejected, got: {err:?}"));
        assert!(rejected.is_non_fast_forward(), "{rejected}");
        assert!(!err.to_string().contains("could not connect"), "{err:?}");
    }

    #[test]
    fn ref_update_limit_is_read_from_the_rule_violation_message() {
        let reason = "push declined due to repository rule violations";
        let mut rejected = PushRejected {
            rejected: vec![
                ("refs/heads/a".into(), reason.into()),
                ("refs/heads/b".into(), reason.into()),
            ],
            remote_messages: vec![
                "error: GH013: Repository rule violations found for refs/heads/a.".into(),
                "- Pushes can not update more than 5 branches or tags.".into(),
            ],
        };
        assert_eq!(rejected.max_ref_updates(), Some(5));

        rejected.rejected[1].1 = "non-fast-forward".into();
        assert_eq!(rejected.max_ref_updates(), None, "mixed reasons");

        rejected.rejected[1].1 = reason.into();
        rejected.remote_messages[1] =
            "- Cannot create ref due to creations being restricted.".into();
        assert_eq!(rejected.max_ref_updates(), None, "a different rule");
    }

    #[test]
    fn only_refused_refspecs_are_retried_in_their_original_order() {
        let refspecs: Vec<String> = vec![
            "aaaa:refs/heads/a".into(),
            "+bbbb:refs/heads/b".into(),
            "cccc:refs/heads/c".into(),
            ":refs/heads/old".into(),
        ];
        let rejected = PushRejected {
            rejected: vec![
                ("refs/heads/old".into(), "rule violations".into()),
                ("refs/heads/b".into(), "rule violations".into()),
            ],
            remote_messages: Vec::new(),
        };
        assert_eq!(
            refspecs_to_retry(&refspecs, &rejected),
            vec![
                "+bbbb:refs/heads/b".to_string(),
                ":refs/heads/old".to_string()
            ]
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
