//! The stats ledger: attribution, grouping, totals.

use super::*;

fn usage(tokens: u64) -> Usage {
    let mut usage = Usage::new();
    usage.total_tokens = tokens;
    usage
}

#[test]
fn same_model_records_accumulate_into_one_entry() {
    let mut ledger = UsageLedger::new();
    ledger.add("p", "m", Some("high"), usage(10));
    ledger.add("p", "m", None, usage(5));
    assert_eq!(ledger.per_model().len(), 1);
    assert_eq!(ledger.per_model()[0].usage.total_tokens, 15);
    assert_eq!(
        ledger.per_model()[0].thinking_level.as_deref(),
        Some("high"),
        "the first-seen level stays on display"
    );
    assert_eq!(ledger.total_usage().total_tokens, 15);
}

#[test]
fn different_models_split_into_entries() {
    let mut ledger = UsageLedger::new();
    ledger.add("p", "m1", None, usage(10));
    ledger.add("q", "m2", None, usage(7));
    ledger.add("p", "m1", None, usage(3));
    assert_eq!(ledger.per_model().len(), 2);
    assert_eq!(ledger.per_model()[0].usage.total_tokens, 13);
    assert_eq!(ledger.per_model()[1].usage.total_tokens, 7);
    assert_eq!(ledger.total_usage().total_tokens, 20);
}

#[test]
fn an_empty_ledger_reports_nothing() {
    let ledger = UsageLedger::new();
    assert!(ledger.per_model().is_empty());
    assert_eq!(ledger.total_usage().total_tokens, 0);
}
