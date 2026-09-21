//! SSH agent probing, status detection, and actionable error formatting.
//!
//! When `nspr` connects or pushes to a remote over SSH (directly or via git's
//! `pushInsteadOf` / `insteadOf` rewrites), libgit2 delegates authentication to
//! `auth-git2`, which in turn queries the SSH agent via libssh2.
//!
//! If the SSH agent is unresponsive or wedged, libssh2's socket operations on
//! the Unix domain socket block indefinitely, hanging the entire CLI without
//! diagnostic output. Furthermore, if the SSH agent connection fails, libgit2
//! surfaces a generic "all authentication attempts failed" error, leading
//! users to wonder why their GitHub token or setup didn't work.
//!
//! This module probes the agent with a bounded timeout before libgit2 is
//! offered the agent credential, preventing hangs and producing sensible,
//! actionable error messages that explain exactly what failed and how to fix it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Result of probing an SSH agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshAgentStatus {
    /// Agent is responsive and has `identities_count` keys loaded.
    Available { identities_count: usize },
    /// `$SSH_AUTH_SOCK` is unset or empty.
    NotConfigured,
    /// Connecting to the agent socket failed (e.g. socket does not exist or connection refused).
    ConnectionFailed { path: PathBuf, reason: String },
    /// Connecting to the agent socket or reading the response timed out.
    TimedOut { path: PathBuf, timeout: Duration },
}

impl SshAgentStatus {
    /// Whether the agent is responsive and can be offered to `auth-git2`.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// Default timeout for probing the SSH agent.
pub const DEFAULT_SSH_AGENT_TIMEOUT: Duration = Duration::from_millis(1500);

/// Probe an SSH agent socket at `sock_path` with a maximum timeout.
#[cfg(unix)]
pub fn probe_ssh_agent(sock_path: &Path, timeout: Duration) -> SshAgentStatus {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = match UnixStream::connect(sock_path) {
        Ok(s) => s,
        Err(e) => {
            return SshAgentStatus::ConnectionFailed {
                path: sock_path.to_path_buf(),
                reason: e.to_string(),
            };
        }
    };

    if let Err(e) = stream.set_read_timeout(Some(timeout)) {
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: format!("failed to set read timeout: {e}"),
        };
    }
    if let Err(e) = stream.set_write_timeout(Some(timeout)) {
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: format!("failed to set write timeout: {e}"),
        };
    }

    // SSH2_AGENTC_REQUEST_IDENTITIES (OpenSSH agent protocol):
    // 4-byte big-endian packet length (1) + 1-byte message type (11).
    let req = [0, 0, 0, 1, 11];
    if let Err(e) = stream.write_all(&req) {
        if e.kind() == std::io::ErrorKind::TimedOut
            || e.kind() == std::io::ErrorKind::WouldBlock
        {
            return SshAgentStatus::TimedOut {
                path: sock_path.to_path_buf(),
                timeout,
            };
        }
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: format!("failed to send request to agent: {e}"),
        };
    }

    // Read 4-byte response length
    let mut len_buf = [0u8; 4];
    if let Err(e) = stream.read_exact(&mut len_buf) {
        if e.kind() == std::io::ErrorKind::TimedOut
            || e.kind() == std::io::ErrorKind::WouldBlock
        {
            return SshAgentStatus::TimedOut {
                path: sock_path.to_path_buf(),
                timeout,
            };
        }
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: format!("failed to read response length from agent: {e}"),
        };
    }

    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len < 5 {
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: "invalid response length from agent".to_string(),
        };
    }

    // Read 1-byte response type + 4-byte count
    let mut header = [0u8; 5];
    if let Err(e) = stream.read_exact(&mut header) {
        if e.kind() == std::io::ErrorKind::TimedOut
            || e.kind() == std::io::ErrorKind::WouldBlock
        {
            return SshAgentStatus::TimedOut {
                path: sock_path.to_path_buf(),
                timeout,
            };
        }
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: format!("failed to read response header from agent: {e}"),
        };
    }

    const SSH2_AGENT_IDENTITIES_ANSWER: u8 = 12;
    if header[0] != SSH2_AGENT_IDENTITIES_ANSWER {
        return SshAgentStatus::ConnectionFailed {
            path: sock_path.to_path_buf(),
            reason: format!("unexpected agent response type: {}", header[0]),
        };
    }

    let count = u32::from_be_bytes([header[1], header[2], header[3], header[4]])
        as usize;
    SshAgentStatus::Available {
        identities_count: count,
    }
}

#[cfg(not(unix))]
pub fn probe_ssh_agent(
    _sock_path: &Path,
    _timeout: Duration,
) -> SshAgentStatus {
    SshAgentStatus::NotConfigured
}

/// Check the status of the SSH agent pointed to by `$SSH_AUTH_SOCK`.
pub fn check_ssh_agent(timeout: Duration) -> SshAgentStatus {
    match std::env::var("SSH_AUTH_SOCK") {
        Ok(path) if !path.is_empty() => {
            probe_ssh_agent(Path::new(&path), timeout)
        }
        _ => SshAgentStatus::NotConfigured,
    }
}

/// Check whether a URL uses SSH transport.
pub fn is_ssh_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("git@")
        || url.starts_with("ssh://")
        || (url.contains(':')
            && !url.starts_with("http://")
            && !url.starts_with("https://")
            && !url.starts_with("git://")
            && !url.starts_with("file://")
            && !url.starts_with('/'))
}

/// Determine the effective URL git will dial, applying `insteadOf` and `pushInsteadOf`
/// from git config if present.
pub fn resolve_effective_url(
    config: &git2::Config,
    url: &str,
    is_push: bool,
) -> String {
    let mut best_prefix = String::new();
    let mut best_replacement = String::new();

    // Search for url.<base>.pushInsteadOf (if push) and url.<base>.insteadOf
    if let Ok(mut entries) =
        config.entries(Some("url\\..*\\.(push)?[iI]nstead[oO]f"))
    {
        while let Some(Ok(entry)) = entries.next() {
            let Ok(name) = entry.name() else { continue };
            let Ok(value) = entry.value() else { continue };

            let Some(tail) = name.strip_prefix("url.") else {
                continue;
            };
            let (base, kind) = match tail.rsplit_once('.') {
                Some((b, k)) => (b, k.to_lowercase()),
                None => continue,
            };

            let matches_kind = if is_push {
                kind == "pushinsteadof" || kind == "insteadof"
            } else {
                kind == "insteadof"
            };

            if matches_kind && url.starts_with(value) {
                // Prefer longer prefix match; for equal length, prefer pushinsteadof
                if value.len() > best_prefix.len()
                    || (value.len() == best_prefix.len()
                        && kind == "pushinsteadof")
                {
                    best_prefix = value.to_string();
                    best_replacement = base.to_string();
                }
            }
        }
    }

    if !best_prefix.is_empty() {
        format!("{}{}", best_replacement, &url[best_prefix.len()..])
    } else {
        url.to_string()
    }
}

/// Produce a user-facing, actionable error message diagnosing an SSH authentication failure.
pub fn format_ssh_auth_error(
    effective_url: &str,
    original_url: &str,
    ssh_status: &SshAgentStatus,
) -> String {
    let rewrite_note = if effective_url != original_url {
        format!(" (rewritten from '{original_url}' via git config)")
    } else {
        String::new()
    };

    match ssh_status {
        SshAgentStatus::TimedOut { path, timeout } => {
            format!(
                "SSH authentication failed connecting to {effective_url}{rewrite_note}: \
                 the SSH agent at '{}' timed out after {:.1}s. \
                 Make sure your SSH agent is responsive, or restart it with `eval $(ssh-agent)`.",
                path.display(),
                timeout.as_secs_f64()
            )
        }
        SshAgentStatus::ConnectionFailed { path, reason } => {
            format!(
                "SSH authentication failed connecting to {effective_url}{rewrite_note}: \
                 could not connect to SSH agent at '{}' ({reason}). \
                 Ensure your SSH agent is running (e.g. `eval $(ssh-agent)` and `ssh-add`), \
                 or test your key with `ssh -T git@github.com`.",
                path.display()
            )
        }
        SshAgentStatus::NotConfigured => {
            format!(
                "SSH authentication failed connecting to {effective_url}{rewrite_note}: \
                 $SSH_AUTH_SOCK is not set and no usable SSH keys were found in ~/.ssh. \
                 Start an SSH agent with `eval $(ssh-agent) && ssh-add`, \
                 or test with `ssh -T git@github.com`."
            )
        }
        SshAgentStatus::Available {
            identities_count: 0,
        } => {
            format!(
                "SSH authentication failed connecting to {effective_url}{rewrite_note}: \
                 the SSH agent is running but has no keys loaded. \
                 Add a key using `ssh-add <path-to-key>` (check with `ssh-add -l`), \
                 or test with `ssh -T git@github.com`."
            )
        }
        SshAgentStatus::Available { identities_count } => {
            format!(
                "SSH authentication failed connecting to {effective_url}{rewrite_note}: \
                 the SSH agent has {identities_count} key(s) loaded, but GitHub rejected authentication. \
                 Verify your SSH key is added to your GitHub account (test with `ssh -T git@github.com`)."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    #[test]
    fn probe_detects_missing_socket() {
        let missing = Path::new("/tmp/nspr-test-nonexistent-socket-path.sock");
        let status = probe_ssh_agent(missing, Duration::from_millis(100));
        assert!(
            matches!(status, SshAgentStatus::ConnectionFailed { .. }),
            "expected ConnectionFailed, got {status:?}"
        );

        let err = format_ssh_auth_error(
            "git@github.com:o/r.git",
            "git@github.com:o/r.git",
            &status,
        );
        assert!(err.contains("could not connect to SSH agent"));
        assert!(err.contains("ssh-add"));
        assert!(err.contains("ssh -T git@github.com"));
    }

    #[test]
    fn probe_detects_connection_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("refused.sock");

        // Bind and immediately drop listener so the socket file exists but connection is refused
        {
            let _listener = UnixListener::bind(&sock_path).unwrap();
        }

        let status = probe_ssh_agent(&sock_path, Duration::from_millis(100));
        assert!(
            matches!(status, SshAgentStatus::ConnectionFailed { .. }),
            "expected ConnectionFailed, got {status:?}"
        );
        let err = format_ssh_auth_error(
            "git@github.com:o/r.git",
            "https://github.com/o/r.git",
            &status,
        );
        assert!(err.contains("Connection refused") || err.contains("connect"));
        assert!(err.contains("rewritten from 'https://github.com/o/r.git'"));
    }

    #[test]
    fn probe_detects_timeout_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("hanging.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        // Spawn mock agent that accepts connection and sleeps without replying
        let handle = thread::spawn(move || {
            if let Ok((_stream, _)) = listener.accept() {
                thread::sleep(Duration::from_millis(500));
            }
        });

        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(80);
        let status = probe_ssh_agent(&sock_path, timeout);
        let elapsed = start.elapsed();

        assert!(
            matches!(status, SshAgentStatus::TimedOut { .. }),
            "expected TimedOut, got {status:?}"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "probe took too long: {elapsed:?}"
        );

        let err = format_ssh_auth_error(
            "git@github.com:o/r.git",
            "git@github.com:o/r.git",
            &status,
        );
        assert!(err.contains("timed out after 0.1s"));
        assert!(err.contains("Make sure your SSH agent is responsive"));

        let _ = handle.join();
    }

    #[test]
    fn probe_detects_responsive_agent_with_keys() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("valid.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0u8; 5];
            stream.read_exact(&mut req).unwrap();
            assert_eq!(req, [0, 0, 0, 1, 11]);

            // Response: len=5, msg_type=12 (SSH2_AGENT_IDENTITIES_ANSWER), count=3
            let resp = [0, 0, 0, 5, 12, 0, 0, 0, 3];
            stream.write_all(&resp).unwrap();
        });

        let status = probe_ssh_agent(&sock_path, Duration::from_secs(1));
        assert_eq!(
            status,
            SshAgentStatus::Available {
                identities_count: 3
            }
        );
        assert!(status.is_available());

        let _ = handle.join();
    }

    #[test]
    fn probe_detects_agent_with_zero_keys() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("empty.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0u8; 5];
            stream.read_exact(&mut req).unwrap();
            // Response: len=5, msg_type=12, count=0
            let resp = [0, 0, 0, 5, 12, 0, 0, 0, 0];
            stream.write_all(&resp).unwrap();
        });

        let status = probe_ssh_agent(&sock_path, Duration::from_secs(1));
        assert_eq!(
            status,
            SshAgentStatus::Available {
                identities_count: 0
            }
        );
        let err = format_ssh_auth_error(
            "git@github.com:o/r.git",
            "git@github.com:o/r.git",
            &status,
        );
        assert!(err.contains("has no keys loaded"));
        assert!(err.contains("ssh-add <path-to-key>"));

        let _ = handle.join();
    }

    #[test]
    fn url_rewrites_and_detection() {
        let t = crate::testutil::TestRepo::new();
        let repo = t.open();
        let mut config = repo.config().unwrap();

        config
            .set_str("url.git@github.com:.pushInsteadOf", "https://github.com/")
            .unwrap();
        config
            .set_str("url.https://github.com/.insteadOf", "git://github.com/")
            .unwrap();

        assert_eq!(
            resolve_effective_url(&config, "https://github.com/o/r.git", true),
            "git@github.com:o/r.git"
        );
        assert_eq!(
            resolve_effective_url(&config, "https://github.com/o/r.git", false),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            resolve_effective_url(&config, "git://github.com/o/r.git", false),
            "https://github.com/o/r.git"
        );

        assert!(is_ssh_url("git@github.com:o/r.git"));
        assert!(is_ssh_url("ssh://git@github.com/o/r.git"));
        assert!(!is_ssh_url("https://github.com/o/r.git"));
    }
}
