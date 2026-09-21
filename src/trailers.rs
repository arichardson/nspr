//! Commit message parsing built on standard git trailers.
//!
//! `nspr` stores all of its per-commit state in the commit message, as git
//! trailers:
//!
//! ```text
//! Add the widget cache
//!
//! Some explanation of the change.
//!
//! Pull-Request: https://github.com/o/r/pull/103
//! Depends-On: #101
//! ```
//!
//! The subject becomes the PR title and the body becomes the PR description.
//! There is deliberately no other metadata store; see the design notes in the
//! implementation plan for why nothing needs to be persisted.

use std::fmt::Write as _;

/// Trailer key linking a commit to its pull request.
pub const PULL_REQUEST: &str = "Pull-Request";

/// Legacy trailer key written by `spr` (`spacedentist/spr` / `ejoffe/spr`).
pub const LEGACY_SPR_PULL_REQUEST: &str = "Pull Request";

/// Trailer key declaring which layer this commit stacks on.
pub const DEPENDS_ON: &str = "Depends-On";

fn matches_trailer_key(actual: &str, query: &str) -> bool {
    actual.eq_ignore_ascii_case(query)
        || (query.eq_ignore_ascii_case(PULL_REQUEST)
            && actual.eq_ignore_ascii_case(LEGACY_SPR_PULL_REQUEST))
}

/// A parsed commit message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitMessage {
    /// First line of the message. Becomes the pull request title.
    pub subject: String,
    /// Everything between the subject and the trailer block, trimmed. Becomes
    /// the pull request description.
    pub body: String,
    /// The trailer block, in the order the trailers appeared.
    pub trailers: Vec<(String, String)>,
}

/// True if `line` looks like `Token: value`.
///
/// Git allows separators other than `:` via configuration, but `nspr` only
/// ever writes `:` and being strict here keeps the round-trip predictable.
fn is_trailer_line(line: &str) -> bool {
    let Some((token, _)) = line.split_once(':') else {
        return false;
    };
    if token.eq_ignore_ascii_case(LEGACY_SPR_PULL_REQUEST) {
        return true;
    }
    !token.is_empty()
        && token.starts_with(|c: char| c.is_ascii_alphabetic())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// True if `line` continues the previous trailer (git allows indented
/// continuations for multi-line trailer values).
fn is_continuation_line(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

impl CommitMessage {
    /// Parse a raw commit message.
    ///
    /// The trailer block is the final paragraph, and only if *every* line in it
    /// is a trailer or a continuation. A message that is only a subject line
    /// never yields trailers, so a subject like `Fix: the thing` is preserved
    /// as a subject rather than being eaten as a trailer.
    pub fn parse(raw: &str) -> Self {
        let normalized = raw.replace("\r\n", "\n");
        let text = normalized.trim_matches('\n');

        let mut lines = text.split('\n');
        let subject = lines.next().unwrap_or("").trim_end().to_string();
        let rest: Vec<&str> = lines.collect();

        // Drop the blank line(s) immediately after the subject.
        let mut start = 0;
        while start < rest.len() && rest[start].trim().is_empty() {
            start += 1;
        }
        let rest = &rest[start..];

        if rest.is_empty() {
            return Self {
                subject,
                body: String::new(),
                trailers: Vec::new(),
            };
        }

        // Find the start of the final paragraph.
        let mut para_start = rest.len();
        while para_start > 0 && !rest[para_start - 1].trim().is_empty() {
            para_start -= 1;
        }
        let last_para = &rest[para_start..];

        let is_trailer_block = !last_para.is_empty()
            && last_para.iter().any(|l| is_trailer_line(l))
            && last_para
                .iter()
                .all(|l| is_trailer_line(l) || is_continuation_line(l));

        let (body_lines, trailer_lines) = if is_trailer_block {
            (&rest[..para_start], last_para)
        } else {
            (rest, &[] as &[&str])
        };

        let mut trailers = Vec::new();
        for line in trailer_lines {
            if is_continuation_line(line) {
                // Append to the previous trailer's value.
                if let Some((_, value)) = trailers.last_mut() {
                    let value: &mut String = value;
                    value.push('\n');
                    value.push_str(line);
                }
                continue;
            }
            if let Some((token, value)) = line.split_once(':') {
                trailers.push((token.to_string(), value.trim().to_string()));
            }
        }

        Self {
            subject,
            body: body_lines.join("\n").trim().to_string(),
            trailers,
        }
    }

    /// Render back to a commit message, in `git interpret-trailers` layout.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(self.subject.trim_end());
        out.push('\n');

        if !self.body.trim().is_empty() {
            out.push('\n');
            out.push_str(self.body.trim());
            out.push('\n');
        }

        if !self.trailers.is_empty() {
            out.push('\n');
            for (token, value) in &self.trailers {
                // Multi-line values were stored with their indentation intact.
                let _ = writeln!(out, "{token}: {value}");
            }
        }

        out
    }

    /// Look up a trailer value, case-insensitively on the key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.trailers
            .iter()
            .find(|(k, _)| matches_trailer_key(k, key))
            .map(|(_, v)| v.as_str())
    }

    /// True if this commit message contains the legacy `Pull Request:` (with a
    /// space) trailer written by `spr`.
    pub fn has_legacy_spr_trailer(&self) -> bool {
        self.trailers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(LEGACY_SPR_PULL_REQUEST))
    }

    /// Insert or replace a trailer, preserving its position if already present.
    pub fn set(&mut self, key: &str, value: &str) {
        if let Some(entry) = self
            .trailers
            .iter_mut()
            .find(|(k, _)| matches_trailer_key(k, key))
        {
            entry.0 = key.to_string();
            entry.1 = value.to_string();
        } else {
            self.trailers.push((key.to_string(), value.to_string()));
        }
    }

    /// Remove a trailer, returning whether anything was removed.
    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.trailers.len();
        self.trailers.retain(|(k, _)| !matches_trailer_key(k, key));
        self.trailers.len() != before
    }

    /// Render the commit message for a pull request branch commit, stripping
    /// review-time trailers (`Depends-On`, `Pull-Request`) while preserving
    /// subject, body, and user trailers (`Signed-off-by`, etc.).
    pub fn clean_for_branch(&self) -> String {
        let mut copy = self.clone();
        copy.remove(DEPENDS_ON);
        copy.remove(PULL_REQUEST);
        copy.render()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_only() {
        let m = CommitMessage::parse("Add the widget cache");
        assert_eq!(m.subject, "Add the widget cache");
        assert_eq!(m.body, "");
        assert!(m.trailers.is_empty());
    }

    #[test]
    fn subject_that_looks_like_a_trailer_is_not_one() {
        // The subject line must never be consumed as a trailer block.
        let m = CommitMessage::parse("Fix: the thing");
        assert_eq!(m.subject, "Fix: the thing");
        assert!(m.trailers.is_empty());
    }

    #[test]
    fn subject_and_body() {
        let m = CommitMessage::parse("Subject\n\nSome body text.\nMore text.");
        assert_eq!(m.subject, "Subject");
        assert_eq!(m.body, "Some body text.\nMore text.");
        assert!(m.trailers.is_empty());
    }

    #[test]
    fn subject_body_and_trailers() {
        let m = CommitMessage::parse(
            "Subject\n\nBody text.\n\nPull-Request: https://x/1\nDepends-On: #101\n",
        );
        assert_eq!(m.subject, "Subject");
        assert_eq!(m.body, "Body text.");
        assert_eq!(m.get(PULL_REQUEST), Some("https://x/1"));
        assert_eq!(m.get(DEPENDS_ON), Some("#101"));
    }

    #[test]
    fn subject_and_trailers_without_body() {
        let m = CommitMessage::parse("Subject\n\nPull-Request: https://x/1\n");
        assert_eq!(m.subject, "Subject");
        assert_eq!(m.body, "");
        assert_eq!(m.get(PULL_REQUEST), Some("https://x/1"));
    }

    #[test]
    fn body_containing_a_colon_is_not_a_trailer_block() {
        // The last paragraph has a colon line but also a non-trailer line,
        // so the whole paragraph must stay in the body.
        let m = CommitMessage::parse(
            "Subject\n\nNote: this is prose.\nAnd this continues it without indent.",
        );
        assert!(m.trailers.is_empty());
        assert_eq!(
            m.body,
            "Note: this is prose.\nAnd this continues it without indent."
        );
    }

    #[test]
    fn signed_off_by_only_block() {
        let m =
            CommitMessage::parse("Subject\n\nBody.\n\nSigned-off-by: A <a@b>");
        assert_eq!(m.body, "Body.");
        assert_eq!(m.get("Signed-off-by"), Some("A <a@b>"));
    }

    #[test]
    fn preserves_foreign_trailers_alongside_ours() {
        let raw = "Subject\n\nBody.\n\nCo-authored-by: B <b@c>\nPull-Request: https://x/1\nSigned-off-by: A <a@b>\n";
        let mut m = CommitMessage::parse(raw);
        assert_eq!(m.trailers.len(), 3);
        m.set(DEPENDS_ON, "#101");
        let rendered = m.render();
        assert!(rendered.contains("Co-authored-by: B <b@c>"));
        assert!(rendered.contains("Signed-off-by: A <a@b>"));
        assert!(rendered.contains("Depends-On: #101"));
        // Round-trips.
        assert_eq!(CommitMessage::parse(&rendered), m);
    }

    #[test]
    fn set_replaces_in_place() {
        let mut m = CommitMessage::parse(
            "S\n\nB\n\nPull-Request: old\nSigned-off-by: A <a@b>\n",
        );
        m.set(PULL_REQUEST, "new");
        assert_eq!(m.get(PULL_REQUEST), Some("new"));
        // Position preserved: Pull-Request is still first.
        assert_eq!(m.trailers[0].0, PULL_REQUEST);
        assert_eq!(m.trailers.len(), 2);
    }

    #[test]
    fn get_is_case_insensitive() {
        let m = CommitMessage::parse("S\n\npull-request: https://x/1\n");
        assert_eq!(m.get(PULL_REQUEST), Some("https://x/1"));
    }

    #[test]
    fn remove_trailer() {
        let mut m =
            CommitMessage::parse("S\n\nB\n\nPull-Request: x\nDepends-On: #1\n");
        assert!(m.remove(DEPENDS_ON));
        assert_eq!(m.get(DEPENDS_ON), None);
        assert!(!m.remove(DEPENDS_ON));
    }

    #[test]
    fn render_round_trips() {
        for raw in [
            "Subject",
            "Subject\n\nBody.",
            "Subject\n\nPull-Request: https://x/1",
            "Subject\n\nBody.\n\nPull-Request: https://x/1\nDepends-On: main",
        ] {
            let m = CommitMessage::parse(raw);
            assert_eq!(
                CommitMessage::parse(&m.render()),
                m,
                "round-trip failed for {raw:?}"
            );
        }
    }

    #[test]
    fn crlf_is_normalized() {
        let m = CommitMessage::parse("Subject\r\n\r\nBody.\r\n");
        assert_eq!(m.subject, "Subject");
        assert_eq!(m.body, "Body.");
    }

    #[test]
    fn continuation_lines_attach_to_previous_trailer() {
        let m =
            CommitMessage::parse("S\n\nB\n\nPull-Request: x\n  continued\n");
        assert_eq!(m.trailers.len(), 1);
        assert_eq!(m.get(PULL_REQUEST), Some("x\n  continued"));
    }
}
