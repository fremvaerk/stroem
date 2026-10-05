//! `ref:` values on action, task and trigger references
//! (spec docs/superpowers/specs/2026-10-02-git-refs-design.md § 4.2).
//! Pure syntax only: whether a branch, tag or commit exists is decided by the
//! server when it resolves the reference. Refname rules follow
//! `git check-ref-format`; they are reimplemented here so this crate, which the
//! CLI and the worker depend on, does not link libgit2.

use std::fmt;

const HEADS: &str = "refs/heads/";
const TAGS: &str = "refs/tags/";

/// A syntactically valid `ref:` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRefSpec {
    /// A full 40-hex commit SHA, lowercased.
    Commit(String),
    /// `refs/heads/<name>` — the payload is `<name>`.
    Branch(String),
    /// `refs/tags/<name>` — the payload is `<name>`.
    Tag(String),
    /// A bare name: resolved as a branch first, then as a tag.
    Name(String),
}

/// Why a `ref:` value is not usable. `Display` is a user-facing sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRefError {
    Empty,
    Templated(String),
    InvalidName(String),
}

impl fmt::Display for GitRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitRefError::Empty => f.write_str("`ref` must not be empty"),
            GitRefError::Templated(r) => {
                write!(f, "`ref` '{r}' is a template; refs are static strings")
            }
            GitRefError::InvalidName(r) => {
                write!(f, "'{r}' is not a valid git branch or tag name")
            }
        }
    }
}

impl std::error::Error for GitRefError {}

/// Parse a `ref:` value (spec § 4.2). Never touches git.
pub fn parse_git_ref(s: &str) -> Result<GitRefSpec, GitRefError> {
    if s.is_empty() {
        return Err(GitRefError::Empty);
    }
    if s.contains("{{") || s.contains("{%") {
        return Err(GitRefError::Templated(s.to_string()));
    }
    if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(GitRefSpec::Commit(s.to_ascii_lowercase()));
    }
    if let Some(name) = s.strip_prefix(HEADS) {
        return checked(name, s).map(GitRefSpec::Branch);
    }
    if let Some(name) = s.strip_prefix(TAGS) {
        return checked(name, s).map(GitRefSpec::Tag);
    }
    if s.starts_with("refs/") {
        // Only branches and tags are supported (refs/pull/…, refs/notes/… are not).
        return Err(GitRefError::InvalidName(s.to_string()));
    }
    checked(s, s).map(GitRefSpec::Name)
}

/// The hint for a bare name that resolved to nothing and looks like an
/// abbreviated commit SHA (4–39 hex chars).
pub fn short_sha_hint(s: &str) -> Option<&'static str> {
    ((4..=39).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some("short commit SHAs are not supported; use the full 40-character SHA")
}

fn checked(name: &str, written: &str) -> Result<String, GitRefError> {
    if is_valid_refname(name) {
        Ok(name.to_string())
    } else {
        Err(GitRefError::InvalidName(written.to_string()))
    }
}

/// `git check-ref-format` rules for the part after `refs/heads/` / `refs/tags/`.
fn is_valid_refname(name: &str) -> bool {
    if name.is_empty() || name == "@" {
        return false;
    }
    if name.starts_with('/') || name.ends_with('/') || name.ends_with('.') {
        return false;
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") {
        return false;
    }
    let forbidden = |b: u8| {
        b < 0x20 || b == 0x7f || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    };
    if name.bytes().any(forbidden) {
        return false;
    }
    name.split('/')
        .all(|c| !c.starts_with('.') && !c.ends_with(".lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901";

    #[test]
    fn full_sha_is_a_commit_and_is_lowercased() {
        assert_eq!(parse_git_ref(SHA), Ok(GitRefSpec::Commit(SHA.to_string())));
        assert_eq!(
            parse_git_ref(&SHA.to_ascii_uppercase()),
            Ok(GitRefSpec::Commit(SHA.to_string()))
        );
    }

    #[test]
    fn qualified_refs_pick_their_namespace() {
        assert_eq!(
            parse_git_ref("refs/heads/release/2.3"),
            Ok(GitRefSpec::Branch("release/2.3".into()))
        );
        assert_eq!(
            parse_git_ref("refs/tags/v4.1.0"),
            Ok(GitRefSpec::Tag("v4.1.0".into()))
        );
    }

    #[test]
    fn bare_names_are_names() {
        assert_eq!(
            parse_git_ref("release/2.3"),
            Ok(GitRefSpec::Name("release/2.3".into()))
        );
        assert_eq!(
            parse_git_ref("v4.1.0"),
            Ok(GitRefSpec::Name("v4.1.0".into()))
        );
        // A short hex string is a legal branch name; the hint is applied later,
        // only when no such branch or tag exists.
        assert_eq!(
            parse_git_ref("3f2a9c0"),
            Ok(GitRefSpec::Name("3f2a9c0".into()))
        );
    }

    #[test]
    fn empty_and_templated_refs_are_rejected() {
        assert_eq!(parse_git_ref(""), Err(GitRefError::Empty));
        assert_eq!(
            parse_git_ref("{{ input.release }}"),
            Err(GitRefError::Templated("{{ input.release }}".into()))
        );
        assert_eq!(
            parse_git_ref("{% if a %}x{% endif %}"),
            Err(GitRefError::Templated("{% if a %}x{% endif %}".into()))
        );
    }

    #[test]
    fn invalid_refnames_are_rejected() {
        for bad in [
            "a..b",
            "a b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[0]",
            "a\\b",
            "/a",
            "a/",
            "a//b",
            "a.",
            ".a",
            "a/.b",
            "a.lock",
            "a/b.lock/c",
            "@",
            "a@{1}",
            "a\tb",
            "refs/pull/1/head",
            "refs/heads/",
            "refs/tags/",
            // Review Focus 5: surrounding whitespace is rejected, never trimmed.
            " v1",
            "v1 ",
            "v1\n",
            " release/2.3",
        ] {
            assert!(
                matches!(parse_git_ref(bad), Err(GitRefError::InvalidName(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn errors_render_as_user_facing_sentences() {
        assert_eq!(GitRefError::Empty.to_string(), "`ref` must not be empty");
        assert_eq!(
            GitRefError::Templated("{{ x }}".into()).to_string(),
            "`ref` '{{ x }}' is a template; refs are static strings"
        );
        assert_eq!(
            GitRefError::InvalidName("a..b".into()).to_string(),
            "'a..b' is not a valid git branch or tag name"
        );
    }

    #[test]
    fn short_sha_hint_only_for_4_to_39_hex_chars() {
        assert!(short_sha_hint("3f2a").is_some());
        assert!(short_sha_hint("3f2a9c0").is_some());
        assert!(short_sha_hint(&SHA[..39]).is_some());
        assert!(short_sha_hint("3f2").is_none());
        assert!(short_sha_hint(SHA).is_none());
        assert!(short_sha_hint("release").is_none());
    }
}
