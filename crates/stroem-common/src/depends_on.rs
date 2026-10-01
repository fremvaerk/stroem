//! Per-edge dependency outcome acceptance — spec
//! docs/superpowers/specs/2026-10-01-dependency-conditions-design.md.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Every dependency resolves, once terminal, to exactly one of these.
/// Spec §2.1. No `Pending` variant here — this is the schema-facing
/// vocabulary used inside `accept`, never a row's live state (see
/// `gate::DepOutcome` for that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    Failed,
    Cancelled,
    Skipped,
    Omitted,
}

impl Outcome {
    pub const ALL: [Outcome; 5] = [
        Outcome::Completed,
        Outcome::Failed,
        Outcome::Cancelled,
        Outcome::Skipped,
        Outcome::Omitted,
    ];
}

/// Deserializes only from the exact string "terminal"; any other value is a
/// parse error. Serializes back to that same literal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalKeyword;

impl Serialize for TerminalKeyword {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("terminal")
    }
}

impl<'de> Deserialize<'de> for TerminalKeyword {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TerminalVisitor;
        impl Visitor<'_> for TerminalVisitor {
            type Value = TerminalKeyword;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "the literal string \"terminal\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<TerminalKeyword, E> {
                if v == "terminal" {
                    Ok(TerminalKeyword)
                } else {
                    Err(E::custom(format!(
                        "expected the literal string \"terminal\", got \"{v}\""
                    )))
                }
            }
        }
        deserializer.deserialize_str(TerminalVisitor)
    }
}

/// A non-empty set of outcomes that satisfies one dependency edge, or the
/// literal `terminal` sugar for "all five, I don't care which." Spec §2.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcceptSet {
    Terminal(TerminalKeyword),
    Outcomes(Vec<Outcome>),
}

impl AcceptSet {
    pub fn contains(&self, outcome: Outcome) -> bool {
        match self {
            AcceptSet::Terminal(_) => true,
            AcceptSet::Outcomes(v) => v.contains(&outcome),
        }
    }

    /// The default for a bare-string `depends_on` entry and for a `{step:
    /// ...}` entry with no explicit `accept` — matches today's "must
    /// complete" meaning.
    pub fn default_completed_only() -> Self {
        AcceptSet::Outcomes(vec![Outcome::Completed])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_round_trips_snake_case() {
        let json = serde_json::to_string(&Outcome::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Outcome::Completed);
        assert_eq!(
            serde_json::to_string(&Outcome::Omitted).unwrap(),
            "\"omitted\""
        );
    }

    #[test]
    fn terminal_keyword_accepts_only_the_exact_string() {
        let ok: TerminalKeyword = serde_json::from_str("\"terminal\"").unwrap();
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"terminal\"");
        let err = serde_json::from_str::<TerminalKeyword>("\"Terminal\"");
        assert!(err.is_err(), "must reject case variants, not fuzzy-match");
        let err2 = serde_json::from_str::<TerminalKeyword>("\"all\"");
        assert!(err2.is_err());
    }

    #[test]
    fn accept_set_outcomes_contains_matches_the_list() {
        let a = AcceptSet::Outcomes(vec![Outcome::Completed, Outcome::Failed]);
        assert!(a.contains(Outcome::Completed));
        assert!(a.contains(Outcome::Failed));
        assert!(!a.contains(Outcome::Skipped));
    }

    #[test]
    fn accept_set_terminal_contains_everything() {
        let a = AcceptSet::Terminal(TerminalKeyword);
        for o in Outcome::ALL {
            assert!(a.contains(o), "{o:?} should be accepted by terminal");
        }
    }

    #[test]
    fn accept_set_deserializes_both_shapes() {
        let list: AcceptSet = serde_json::from_str("[\"completed\",\"failed\"]").unwrap();
        assert!(matches!(list, AcceptSet::Outcomes(v) if v == vec![Outcome::Completed, Outcome::Failed]));
        let term: AcceptSet = serde_json::from_str("\"terminal\"").unwrap();
        assert!(matches!(term, AcceptSet::Terminal(_)));
    }
}
