//! Per-edge dependency outcome acceptance — spec
//! docs/superpowers/specs/2026-10-01-dependency-conditions-design.md.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Every dependency resolves, once terminal, to exactly one of these.
/// Spec §2.1. No `Pending` variant here — this is the schema-facing
/// vocabulary used inside `accept`, never a row's live state (see
/// `gate::DepOutcome` for that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// The exact YAML spelling (matches the `rename_all = "snake_case"`
    /// serde form) — for user-facing error text, never `{:?}`'s PascalCase.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Skipped => "skipped",
            Outcome::Omitted => "omitted",
        }
    }
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

/// All step names referenced anywhere in this tree, duplicates preserved —
/// callers (cycle detection, unknown-name checks) decide what to do with
/// repeats.
pub fn collect_names<'a>(entries: &'a [DependsOnEntry], out: &mut Vec<&'a str>) {
    for e in entries {
        if let Some(name) = e.leaf_name() {
            out.push(name);
        }
        if let Some(children) = e.children() {
            collect_names(children, out);
        }
    }
}

fn check_siblings(children: &[DependsOnEntry], errors: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    for c in children {
        if let Some(name) = c.leaf_name() {
            if !seen.insert(name) {
                errors.push(format!(
                    "duplicate dependency '{name}' in the same depends_on group"
                ));
            }
        }
    }
}

fn check_nested(entry: &DependsOnEntry, errors: &mut Vec<String>) {
    match entry {
        DependsOnEntry::Step(s) => {
            if let AcceptSet::Outcomes(v) = &s.accept {
                if v.is_empty() {
                    errors.push(format!("'{}' has an empty accept list", s.step));
                }
                let mut seen = std::collections::HashSet::new();
                for o in v {
                    if !seen.insert(*o) {
                        errors.push(format!(
                            "'{}' has a duplicate outcome '{}' in its accept list",
                            s.step,
                            o.as_str()
                        ));
                    }
                }
            }
        }
        DependsOnEntry::All(a) => {
            if a.all.is_empty() {
                errors.push("an 'all' group must not be empty".into());
            }
            check_siblings(&a.all, errors);
            for c in &a.all {
                check_nested(c, errors);
            }
        }
        DependsOnEntry::Any(a) => {
            if a.any.is_empty() {
                errors.push("an 'any' group must not be empty".into());
            }
            check_siblings(&a.any, errors);
            for c in &a.any {
                check_nested(c, errors);
            }
        }
        DependsOnEntry::Name(_) => {}
    }
}

/// Structural validation of a `depends_on` tree: duplicate siblings, empty
/// groups, empty accept lists. Does NOT check that referenced step names
/// exist in the flow — that needs the whole flow map and lives in
/// `validation.rs` (Task 4). An empty root (`entries.is_empty()`) is valid:
/// a step with no dependencies at all, unchanged from today.
pub fn validate_tree(entries: &[DependsOnEntry]) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check_siblings(entries, &mut errors);
    for e in entries {
        check_nested(e, &mut errors);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
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
        assert!(
            matches!(list, AcceptSet::Outcomes(v) if v == vec![Outcome::Completed, Outcome::Failed])
        );
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
            DependsOnEntry::Step(s) => assert!(
                s.accept.contains(Outcome::Completed) && !s.accept.contains(Outcome::Failed)
            ),
            other => panic!("expected Step, got {other:?}"),
        }
    }

    #[test]
    fn all_and_any_groups_parse_and_nest() {
        let e: DependsOnEntry =
            serde_json::from_str("{\"any\":[\"mirror-a\",{\"all\":[\"audit\",\"source\"]}]}")
                .unwrap();
        match e {
            DependsOnEntry::Any(a) => assert_eq!(a.any.len(), 2),
            other => panic!("expected Any, got {other:?}"),
        }
    }

    #[test]
    fn typo_d_key_is_a_hard_error_not_silently_dropped() {
        let err =
            serde_json::from_str::<DependsOnEntry>("{\"step\":\"a\",\"accpet\":[\"completed\"]}");
        assert!(
            err.is_err(),
            "typo'd key must fail to parse, not silently default accept"
        );
    }

    #[test]
    fn two_discriminating_keys_at_once_is_a_hard_error() {
        let err = serde_json::from_str::<DependsOnEntry>("{\"step\":\"a\",\"any\":[\"b\"]}");
        assert!(
            err.is_err(),
            "a mapping with both 'step' and 'any' must not silently resolve to whichever variant matches first"
        );
    }

    fn step(name: &str) -> DependsOnEntry {
        DependsOnEntry::Name(name.to_string())
    }

    #[test]
    fn collect_names_walks_the_whole_tree_with_duplicates_preserved() {
        let tree = vec![
            step("a"),
            DependsOnEntry::Any(AnyEntry {
                any: vec![
                    DependsOnEntry::All(AllEntry {
                        all: vec![step("a"), step("b")],
                    }),
                    DependsOnEntry::All(AllEntry {
                        all: vec![step("a"), step("c")],
                    }),
                ],
            }),
        ];
        let mut names = Vec::new();
        collect_names(&tree, &mut names);
        names.sort();
        assert_eq!(names, vec!["a", "a", "a", "b", "c"]);
    }

    #[test]
    fn validate_tree_rejects_duplicate_siblings_in_the_same_group() {
        let tree = vec![step("a"), step("a")];
        let err = validate_tree(&tree).unwrap_err();
        assert!(err
            .iter()
            .any(|e| e.contains("duplicate") && e.contains('a')));
    }

    #[test]
    fn validate_tree_accepts_the_same_name_in_different_any_branches() {
        // (A completed AND B completed) OR (A failed AND C completed) —
        // `A` legitimately appears twice, once per branch, with different
        // implied roles. This must NOT be rejected (spec §8).
        let tree = vec![DependsOnEntry::Any(AnyEntry {
            any: vec![
                DependsOnEntry::All(AllEntry {
                    all: vec![
                        DependsOnEntry::Step(StepEntry {
                            step: "a".into(),
                            accept: AcceptSet::default_completed_only(),
                        }),
                        step("b"),
                    ],
                }),
                DependsOnEntry::All(AllEntry {
                    all: vec![
                        DependsOnEntry::Step(StepEntry {
                            step: "a".into(),
                            accept: AcceptSet::Outcomes(vec![Outcome::Failed]),
                        }),
                        step("c"),
                    ],
                }),
            ],
        })];
        assert!(validate_tree(&tree).is_ok());
    }

    #[test]
    fn validate_tree_rejects_nested_empty_groups_but_allows_empty_root() {
        assert!(
            validate_tree(&[]).is_ok(),
            "an empty root depends_on is unchanged from today"
        );
        let nested_empty = vec![DependsOnEntry::All(AllEntry { all: vec![] })];
        let err = validate_tree(&nested_empty).unwrap_err();
        assert!(err.iter().any(|e| e.contains("empty")));
    }

    #[test]
    fn validate_tree_rejects_an_empty_accept_outcomes_list() {
        let tree = vec![DependsOnEntry::Step(StepEntry {
            step: "a".into(),
            accept: AcceptSet::Outcomes(vec![]),
        })];
        let err = validate_tree(&tree).unwrap_err();
        assert!(err.iter().any(|e| e.contains("empty") && e.contains('a')));
    }

    #[test]
    fn validate_tree_runs_against_the_authored_shape_not_a_flattened_one() {
        // Implementation caution from spec §8: check siblings before any
        // normalization. A nested `all` inside an `any` is NOT a sibling of
        // the `any`'s other children even though it shares a name with one.
        let tree = vec![DependsOnEntry::Any(AnyEntry {
            any: vec![
                step("a"),
                DependsOnEntry::All(AllEntry {
                    all: vec![step("a")],
                }),
            ],
        })];
        // "a" appears once as a direct child of the `any`, and once nested
        // one level deeper inside an `all` that is itself a child of the
        // `any` — these are different groups, so this must be accepted.
        assert!(validate_tree(&tree).is_ok());
    }
}
