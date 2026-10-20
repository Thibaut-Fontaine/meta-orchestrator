//! Small property-style tests for the invariants that matter most.

use chrono::{Duration, Utc};
use meta_orchestrator::checks::{import_markdown, parse_due, CheckStore};
use meta_orchestrator::scheduler::Scheduler;
use meta_orchestrator::util::slugify;

#[test]
fn slugify_is_idempotent_and_safe() {
    for sample in [
        "Hello, World!",
        "  spaces  and -- dashes ",
        "Ünïcode ☃ project",
        "a/b\\c:d*e?f",
        &"x".repeat(500),
    ] {
        let once = slugify(sample);
        assert_eq!(once, slugify(&once));
        assert!(!once.starts_with('-'));
        assert!(!once.ends_with('-'));
        assert!(!once.is_empty());
        assert!(once
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }
}

#[test]
fn parsed_due_dates_are_in_the_future() {
    let now = Utc::now();
    for input in ["+30m", "2h", "3d", "1w", "10m"] {
        let due = parse_due(input, now).unwrap().unwrap();
        assert!(due > now, "{input} -> {due}");
        assert!(due <= now + Duration::days(8));
    }
}

#[test]
fn check_import_is_idempotent_over_many_lines() {
    let mut store = CheckStore::default();
    let content: String = (0..25)
        .map(|i| format!("- [ ] Check {i} | due: +{i}h | kind: vps\n"))
        .collect();
    let first = import_markdown(&mut store, "demo", &content, Utc::now());
    let second = import_markdown(&mut store, "demo", &content, Utc::now());
    assert_eq!(first.created, 25);
    assert_eq!(second.created, 0);
    assert_eq!(second.updated, 25);
    assert_eq!(store.open().len(), 25);
}

#[test]
fn scheduler_slots_are_disjoint_and_cover_usable_cores() {
    for total in 1..=16 {
        for reserve in 0..total {
            for parallel in 1..=6 {
                let scheduler = Scheduler::new(total, reserve, parallel);
                let slots = scheduler.slots();
                assert_eq!(slots.len(), scheduler.max_parallel());
                let mut all: Vec<usize> = slots.iter().flatten().copied().collect();
                all.sort_unstable();
                let mut unique = all.clone();
                unique.dedup();
                assert_eq!(
                    all, unique,
                    "overlapping slots for {total}/{reserve}/{parallel}"
                );
                assert_eq!(all.len(), scheduler.usable_cores());
                assert!(all.iter().all(|c| *c < total));
                assert!(all.iter().all(|c| *c >= reserve));
            }
        }
    }
}
