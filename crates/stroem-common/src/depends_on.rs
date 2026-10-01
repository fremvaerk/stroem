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

fn default_accept() -> AcceptSet {
    AcceptSet::default_completed_only()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEntry {
    pub step: String,
    #[serde(default = "default_accept")]
    pub accept: AcceptSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllEntry {
    pub all: Vec<DependsOnEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnyEntry {
    pub any: Vec<DependsOnEntry>,
}

/// One entry in a `depends_on` list. `deny_unknown_fields` lives on each
/// named struct above, not on this enum's variants directly — serde treats
/// that as a container attribute, and rejects it on a bare enum variant.
/// See spec §7 for why this is the standard pattern, not a workaround.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DependsOnEntry {
    Name(String),
    Step(StepEntry),
    All(AllEntry),
    Any(AnyEntry),
}

impl DependsOnEntry {
    /// For a leaf (`Name`/`Step`) entry, the step it names; `None` for a
    /// group (`All`/`Any`), which doesn't name exactly one step.
    pub fn leaf_name(&self) -> Option<&str> {
        match self {
            DependsOnEntry::Name(n) => Some(n.as_str()),
            DependsOnEntry::Step(s) => Some(s.step.as_str()),
            DependsOnEntry::All(_) | DependsOnEntry::Any(_) => None,
        }
    }

    /// This entry's direct children, if it's a group; `None` for a leaf.
    pub fn children(&self) -> Option<&[DependsOnEntry]> {
        match self {
            DependsOnEntry::All(a) => Some(&a.all),
            DependsOnEntry::Any(a) => Some(&a.any),
            DependsOnEntry::Name(_) | DependsOnEntry::Step(_) => None,
        }
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

    #[test]
    fn bare_name_parses_as_a_name_entry() {
        let e: DependsOnEntry = serde_json::from_str("\"build-sessions\"").unwrap();
        assert!(matches!(e, DependsOnEntry::Name(n) if n == "build-sessions"));
    }

    #[test]
    fn step_entry_parses_with_explicit_accept() {
        let e: DependsOnEntry =
            serde_json::from_str("{\"step\":\"a\",\"accept\":[\"completed\",\"failed\"]}").unwrap();
        match e {
            DependsOnEntry::Step(s) => {
                assert_eq!(s.step, "a");
                assert!(s.accept.contains(Outcome::Failed));
            }
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn step_entry_without_accept_defaults_to_completed_only() {
        let e: DependsOnEntry = serde_json::from_str("{\"step\":\"a\"}").unwrap();
        match e {
            DependsOnEntry::Step(s) => assert!(s.accept.contains(Outcome::Completed) && !s.accept.contains(Outcome::Failed)),
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn all_and_any_groups_parse_and_nest() {
        let e: DependsOnEntry =
            serde_json::from_str("{\"any\":[\"mirror-a\",{\"all\":[\"audit\",\"source\"]}]}").unwrap();
        match e {
            DependsOnEntry::Any(a) => assert_eq!(a.any.len(), 2),
            other => panic!("expected Any, got {other:?}"),
        }
    }

    #[test]
    fn typo_d_key_is_a_hard_error_not_silently_dropped() {
        let err = serde_json::from_str::<DependsOnEntry>("{\"step\":\"a\",\"accpet\":[\"completed\"]}");
        assert!(err.is_err(), "typo'd key must fail to parse, not silently default accept");
    }

    #[test]
    fn two_discriminating_keys_at_once_is_a_hard_error() {
        let err = serde_json::from_str::<DependsOnEntry>(
            "{\"step\":\"a\",\"any\":[\"b\"]}",
        );
        assert!(
            err.is_err(),
            "a mapping with both 'step' and 'any' must not silently resolve to whichever variant matches first"
        );
    }
}
