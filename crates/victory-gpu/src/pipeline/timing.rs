//! Optional wall-clock phases after a pipeline's shader translations finish.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use super::Shader;

struct Active {
    began: Instant,
    phases: Vec<(&'static str, u64)>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

pub(super) struct Timing<'a> {
    shaders: &'a [Arc<Shader>],
    previous: Option<Active>,
    enabled: bool,
}

impl<'a> Timing<'a> {
    pub(super) fn start(shaders: &'a [Arc<Shader>]) -> Self {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| {
            std::env::var_os("VICTORY_PIPELINE_STAGE_TIMING").is_some_and(|v| v == "1")
        });
        let previous = if enabled {
            ACTIVE.replace(Some(Active {
                began: Instant::now(),
                phases: vec![("bindings", 0)],
            }))
        } else {
            None
        };
        Self { shaders, previous, enabled }
    }
}

impl Drop for Timing<'_> {
    fn drop(&mut self) {
        if self.enabled && let Some(active) = ACTIVE.replace(self.previous.take()) {
            let us = active.began.elapsed().as_micros() as u64;
            tracing::info!(us, shaders = ?self.shaders, phases = ?active.phases,
                "host pipeline timing");
        }
    }
}

/// Starts the next phase of this thread's pipeline, if it has a trace.
pub(super) fn phase(name: &'static str) {
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active {
            active.phases.push((name, active.began.elapsed().as_micros() as u64));
        }
    });
}
