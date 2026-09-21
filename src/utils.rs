//! Small shared helpers.

use unicode_normalization::UnicodeNormalization;

/// Convert a commit subject into a branch-name-safe slug.
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;

    for ch in text.nfkd().filter(|c| !is_combining_mark(*c)) {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_dash = true;
        }
    }

    // Keep branch names reasonable, without cutting mid-word.
    const MAX: usize = 60;
    if out.len() > MAX {
        let cut = out[..MAX].rfind('-').unwrap_or(MAX);
        out.truncate(cut);
    }
    out
}

fn is_combining_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Add the widget cache"), "add-the-widget-cache");
        assert_eq!(slugify("Fix   multiple   spaces"), "fix-multiple-spaces");
        assert_eq!(slugify("Punctuation!?"), "punctuation");
        assert_eq!(slugify("  leading and trailing  "), "leading-and-trailing");
    }

    #[test]
    fn slugify_strips_accents() {
        assert_eq!(slugify("Café update"), "cafe-update");
    }

    #[test]
    fn slugify_truncates_on_a_word_boundary() {
        let s = slugify(
            "This is an extremely long commit subject line that goes on and on \
             well past any reasonable branch name length",
        );
        assert!(s.len() <= 60);
        assert!(!s.ends_with('-'));
    }

    #[test]
    fn slugify_handles_empty() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("!!!"), "");
    }
}
