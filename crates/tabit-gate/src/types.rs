//! Core types for the gate — ported from pi-sanity's `types.ts`, with
//! `action-utils.ts` folded in here (the porting map's one fold:
//! stricter-action aggregation is the same concern as the result type
//! it produces).

use std::collections::BTreeSet;

/// The three possible outcomes of any check.
///
/// Ported from `types.ts` (`Action = "allow" | "ask" | "deny"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// Run without interrupting the user.
    #[default]
    Allow,
    /// Ask the user for confirmation first.
    Ask,
    /// Refuse.
    Deny,
}

impl Action {
    /// The action's config spelling (`"allow"`, `"ask"`, `"deny"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Ask => "ask",
            Action::Deny => "deny",
        }
    }

    /// Parse a config action string; `None` for anything else.
    ///
    /// TS carries invalid strings through untyped; this typed boundary
    /// is where invalid values surface (callers warn and fall back).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Action::Allow),
            "ask" => Some(Action::Ask),
            "deny" => Some(Action::Deny),
            _ => None,
        }
    }

    /// Comparison priority (higher = stricter) — `action-utils.ts`'s
    /// `ACTION_PRIORITY`.
    pub fn priority(self) -> u8 {
        match self {
            Action::Allow => 0,
            Action::Ask => 1,
            Action::Deny => 2,
        }
    }

    /// The stricter of two actions (`action-utils.ts`'s
    /// `stricterAction`; ties keep the first argument, as in TS).
    pub fn stricter(a: Self, b: Self) -> Self {
        if a.priority() >= b.priority() { a } else { b }
    }
}

/// Result from any checker operation (`types.ts`'s `CheckResult`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckResult {
    pub action: Action,
    pub reason: Option<String>,
}

impl CheckResult {
    /// A plain allow with no reason (TS `{ action: "allow" }`).
    pub fn allow() -> Self {
        CheckResult {
            action: Action::Allow,
            reason: None,
        }
    }

    /// An action with an optional reason.
    pub fn new(action: Action, reason: Option<String>) -> Self {
        CheckResult { action, reason }
    }
}

/// Aggregate multiple check results into the strictest action.
/// Reasons are deduplicated (first-appearance order) and joined with
/// "; " — a faithful port of `action-utils.ts`'s `aggregateResults`.
pub fn aggregate_results(results: Vec<CheckResult>) -> CheckResult {
    if results.is_empty() {
        return CheckResult::allow();
    }

    let mut strictest = results[0].action;
    let mut seen = BTreeSet::new();
    let mut reasons: Vec<String> = vec![];

    for result in &results {
        strictest = Action::stricter(strictest, result.action);
        if let Some(reason) = &result.reason
            && seen.insert(reason.clone())
        {
            reasons.push(reason.clone());
        }
    }

    CheckResult {
        action: strictest,
        reason: if reasons.is_empty() {
            None
        } else {
            Some(reasons.join("; "))
        },
    }
}
