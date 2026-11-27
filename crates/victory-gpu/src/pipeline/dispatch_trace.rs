//! Recording outcomes for a private selection of compute shaders.

use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use super::pipeline;
use crate::object::This;

const RECORDED: &str = "recorded";

struct Watch {
    hash: String,
    attempts: AtomicU64,
    saw_recorded: AtomicBool,
}

impl Watch {
    fn new(hash: String) -> Self {
        Self {
            hash,
            attempts: AtomicU64::new(0),
            saw_recorded: AtomicBool::new(false),
        }
    }

    /// A late successful recording still matters when the first attempts all failed.
    fn report(&self, attempt: u64, recorded: bool) -> Option<bool> {
        let first_recorded = recorded && !self.saw_recorded.swap(true, Ordering::Relaxed);
        (attempt <= 8 || first_recorded).then_some(first_recorded)
    }
}

fn watches() -> &'static [Watch] {
    static WATCHES: OnceLock<Vec<Watch>> = OnceLock::new();
    WATCHES.get_or_init(|| {
        let value = std::env::var("VICTORY_TRACE_COMPUTE_DISPATCHES").unwrap_or_default();
        let mut hashes = BTreeSet::new();
        for hash in value.split(',').map(str::trim).filter(|hash| !hash.is_empty()) {
            if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                hashes.insert(hash.to_ascii_lowercase());
            } else {
                tracing::warn!("VICTORY_TRACE_COMPUTE_DISPATCHES expects 16-digit shader hashes");
            }
        }
        hashes.into_iter().map(Watch::new).collect()
    })
}

struct Attempt {
    watch: &'static Watch,
    number: u64,
    route: &'static str,
    frame: u64,
    began: Instant,
    outcome: &'static str,
}

/// Trace the first attempts and the first successful recording for a selected
/// shader. A recording still needs submission, execution and successful writes.
#[derive(Default)]
pub(crate) struct DispatchTrace(Option<Attempt>);

impl DispatchTrace {
    pub(crate) fn start(this: This, route: &'static str) -> Self {
        let watches = watches();
        if watches.is_empty() || this.is_null() {
            return Self::default();
        }
        let [shader] = pipeline(this).shaders.as_slice() else {
            return Self::default();
        };
        if shader.stage != "compute" {
            return Self::default();
        }
        let Some(watch) = watches.iter().find(|w| w.hash == shader.hash) else {
            return Self::default();
        };
        let number = watch.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        let frame = crate::frame::current();
        let began = Instant::now();
        if number == 1 {
            tracing::info!(shader = %watch.hash, attempt = number, route, frame,
                "compute dispatch trace began");
        }
        Self(Some(Attempt {
            watch, number, route, frame, began, outcome: "skipped",
        }))
    }

    pub(crate) fn outcome(&mut self, outcome: &'static str) {
        if let Some(attempt) = &mut self.0 {
            attempt.outcome = outcome;
        }
    }

    pub(crate) fn recorded(mut self) {
        self.outcome(RECORDED);
    }
}

impl Drop for DispatchTrace {
    fn drop(&mut self) {
        let Some(a) = &self.0 else { return };
        let Some(first_recorded) = a.watch.report(a.number, a.outcome == RECORDED) else {
            return;
        };
        tracing::info!(shader = %a.watch.hash, attempt = a.number, route = a.route, frame = a.frame,
            outcome = a.outcome, elapsed_us = a.began.elapsed().as_micros() as u64, first_recorded,
            "compute dispatch trace finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_a_late_success_after_the_first_eight_attempts() {
        let watch = Watch::new("0000000000000000".into());
        for attempt in 1..=8 {
            assert_eq!(watch.report(attempt, false), Some(false));
        }
        assert_eq!(watch.report(9, false), None);
        assert_eq!(watch.report(10, true), Some(true));
        assert_eq!(watch.report(11, true), None);
    }
}
