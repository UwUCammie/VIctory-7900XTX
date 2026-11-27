//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost. Passes over the
//! screen (no vertex input, no depth) have workers and a longer budget of
//! their own ([`screen_budget`]): the title fills its lighting tables with
//! such draws once, at the world's first frame, and behind a world's worth of
//! pipelines they were dropped; that run had no lamps and no diffuse light to
//! its end (run textures-d2, the first after a new translation; Belobug
//! 18e57db8 found the same).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    /// Taken by a worker from [`Queue::slow`].
    was_slow: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`]).
    screen: bool,
    /// A compute pipeline whose dispatches wait for it ([`compute_waits`]),
    /// whether a dropped dispatch of it was logged, and whether a dispatch
    /// has had its one wait for it ([`COMPUTE_CHANCE`]).
    compute: bool,
    compute_dropped: AtomicBool,
    compute_waited: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            screen: graphics.as_ref().is_some_and(|g| g.screen_pass()),
            compute: graphics.is_none() && shaders.first().is_some_and(|s| s.stage == "compute") && compute_waits(),
            compute_dropped: AtomicBool::new(false),
            compute_waited: AtomicBool::new(false),
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            was_slow: AtomicBool::new(false),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let began = Instant::now();
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            super::library::note_time(&self.key, began.elapsed().as_millis() as u64);
            made
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// The hashes of the shaders `p` is made of.
pub(crate) fn hashes(p: &Pending) -> String {
    let hashes: Vec<&str> = p.shaders.iter().map(|s| s.hash.as_str()).collect();
    hashes.join("+")
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// As [`wait_budget`], for passes over the screen (`VICTORY_SCREEN_WAIT_MS`,
/// 10 s; 0: like the others). Below the title's 30 s watchdog.
fn screen_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_SCREEN_WAIT_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(10_000);
        Duration::from_millis(millis)
    })
}

/// Whether passes over the screen go before the others.
fn screens_first() -> bool {
    !screen_budget().is_zero()
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Under Wine, the first dispatch of a compute pipeline not made yet waits
/// for it outside the frame's budget, within its own ([`COMPUTE_BUDGET`];
/// `VICTORY_COMPUTE_WAIT`: unset, on under Wine only; `0`
/// never; `1` on any host). A dropped draw loses one frame's picture; a
/// dropped dispatch the title issues once loses its output for the
/// session. With a cold driver shader cache the head blend-shape map
/// (61e54e90adccaf6a) was made 1 to 21 s after its one dispatch, behind
/// hundreds of graphics pipelines, and every head of the session had no
/// skin (runs r2 and r6, vkd3d-proton 3.2.0 and 71cae09 alike; a warm
/// cache had skin). A timing race, not a Wine one (the fork's issue #75 saw
/// heads without skin on Windows), but on Windows it is untried, so it is
/// off there by default.
fn compute_waits() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = crate::wine::on_under_wine("VICTORY_COMPUTE_WAIT");
        if on {
            tracing::info!(
                ms = COMPUTE_BUDGET.as_millis() as u64,
                "compute dispatches wait for their pipelines"
            );
        }
        on
    })
}

/// What compute dispatches ([`compute_waits`]) may hold one frame, from
/// the frame's first compute wait, over all threads. It was 10 s: on a cold
/// start every frame that dispatched pipelines not made yet was held up to
/// that again, 8 gaps of 1 to 10 s between frames in the first 300 s here
/// (run b1-cur) and 30 in a tester's run on an RX 7900 XT (issue #386).
/// Waiting does not make compiles faster; what a one-off dispatch needs is
/// its own pipeline made next ([`Queue::hurry`]): the head map is made 3 ms
/// after a worker takes it, but on a cold start (translation too) its
/// dispatch waited 289 ms for a worker to come free (run n3-new, with 300 ms
/// here). A shared allowance refilled over time instead (1 s, a fifth of the
/// time) was spent by the bursts of new pipelines when the head map came,
/// which was made 14 s after its dispatch (run n2-new).
const COMPUTE_BUDGET: Duration = Duration::from_millis(500);

/// The end of the frame's compute waits ([`COMPUTE_BUDGET`]).
struct FrameWaits {
    frame: u64,
    until: Option<Instant>,
}

impl FrameWaits {
    const fn new() -> Self {
        FrameWaits { frame: u64::MAX, until: None }
    }

    /// A compute wait of frame `frame` begins at `now`: how long it may
    /// wait. Threads that wait together end together (a second thread that
    /// found the budget taken dropped its dispatch, 91b10a73e48b2192, 18 ms
    /// before its pipeline was made), and a frame is held at most the budget
    /// however many wait. (A thread that read the frame before a newer one
    /// began counts in the newer one: it must not give that frame a second
    /// budget.)
    fn begin(&mut self, now: Instant, frame: u64) -> Duration {
        if self.frame == u64::MAX || frame > self.frame {
            *self = FrameWaits { frame, until: Some(now + COMPUTE_BUDGET) };
        }
        self.until.map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
    }
}

/// A wait at least this long without its pipeline made is the pipeline's
/// one wait: one that is never made (six were, with vkd3d-proton 3.2.0; one
/// left unmade because the driver's compiler crashed on it; a worker stuck
/// in a compile) holds one frame once, not every frame that dispatches it
/// (as the fork's #73 asks of every wait). A shorter one (the frame's
/// budget nearly spent) leaves it another.
const COMPUTE_CHANCE: Duration = Duration::from_millis(100);

/// A recorder can take its frame number, then reach the budget lock out of order.
fn reserve_wait(
    spent: &mut (u64, Duration),
    frame: u64,
    budget: Duration,
    shared: bool,
) -> Duration {
    if frame < spent.0 {
        return Duration::ZERO;
    }
    if frame > spent.0 {
        *spent = (frame, Duration::ZERO);
    }
    let left = budget.saturating_sub(spent.1);
    if shared {
        spent.1 += left;
    }
    left
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Arc<Pending>, frame: u64) {
    let screen = p.screen && screens_first();
    // A compute dispatch ([`compute_waits`]) of a pipeline that has not had
    // its wait waits within the frame's compute budget; with that spent, or
    // after its wait, as any other. The pipeline it waits for is made next.
    let allowed = if p.compute && !p.compute_waited.load(Ordering::Relaxed) {
        static WAITS: Mutex<FrameWaits> = Mutex::new(FrameWaits::new());
        WAITS.lock().unwrap_or_else(|e| e.into_inner()).begin(Instant::now(), frame)
    } else {
        Duration::ZERO
    };
    let compute = !allowed.is_zero();
    if compute {
        QUEUE.lock().unwrap_or_else(|e| e.into_inner()).hurry(p);
    }
    let budget = if screen {
        screen_budget()
    } else if compute {
        allowed
    } else {
        wait_budget()
    };
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now); of the passes over the screen too.
    static SPENT: [Mutex<(u64, Duration)>; 2] = [const { Mutex::new((0, Duration::ZERO)) }; 2];
    let spent_of = &SPENT[usize::from(screen)];
    let shared = shared_budget();
    let left = if compute {
        allowed
    } else {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        reserve_wait(&mut spent, frame, budget, shared)
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    if screen && waited > Duration::from_millis(500) {
        tracing::info!(
            ms = waited.as_millis() as u64,
            made = p.made.get().is_some(),
            shaders = ?p.shaders,
            "a pass over the screen was waited for (VICTORY_SCREEN_WAIT_MS=0: dropped past half a second a frame)"
        );
    }
    if !compute {
        let mut spent = spent_of.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    if compute && p.made.get().is_none() && waited >= COMPUTE_CHANCE {
        p.compute_waited.store(true, Ordering::Relaxed);
    }
    // A dispatch still without its pipeline is dropped by the caller: said
    // once a pipeline.
    if p.compute && p.made.get().is_none() && !p.compute_dropped.swap(true, Ordering::Relaxed) {
        tracing::info!(
            shaders = %hashes(p),
            frame,
            waited_ms = waited.as_millis() as u64,
            allowed = compute,
            "a compute dispatch was dropped: its pipeline was not made within its wait"
        );
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// How many workers may make pipelines at once that are known to take long
/// and that no draw has asked for: a quarter of them. On a fresh cache (a
/// change of the translator or the driver) the early list begins with
/// programs that take tens of seconds each: all 16 workers were in them
/// through the whole of the title's loading, 97 pipelines made in 20 s, and
/// the first minute and a half of the world ran at 0.5 to 1.6 s a frame,
/// dropping up to 2,900 draws a frame (run shaders-1).
/// `VICTORY_PIPELINE_SLOW=0`: in the order the title creates them, as before.
fn slow_limit() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        if std::env::var_os("VICTORY_PIPELINE_SLOW").is_some_and(|v| v == "0") {
            usize::MAX
        } else {
            (worker_count(None, std::thread::available_parallelism().map_or(2, |n| n.get())) / 4).max(1)
        }
    })
}

struct Queue {
    /// Requested passes over the screen: before all others, and the only ones
    /// [`SCREEN_WORKERS`] take.
    screen: VecDeque<Arc<Pending>>,
    /// Compute pipelines a dispatch waits for now ([`Queue::hurry`]), and
    /// the other requested ones whose dispatches wait ([`compute_waits`]):
    /// next, before the requested graphics, the newest first.
    hurried: VecDeque<Arc<Pending>>,
    compute: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    /// Those of the early ones known to take long, and how many of them are
    /// being made ([`slow_limit`]).
    slow: VecDeque<Arc<Pending>>,
    slow_now: usize,
    requested: VecDeque<Arc<Pending>>,
    /// What the workers other than the screen passes' have taken and not
    /// made yet, and how many of those workers there are
    /// ([`Queue::reserved`]).
    taken: Vec<Arc<Pending>>,
    workers: Option<usize>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            screen: VecDeque::new(),
            hurried: VecDeque::new(),
            compute: VecDeque::new(),
            early: VecDeque::new(),
            slow: VecDeque::new(),
            slow_now: 0,
            requested: VecDeque::new(),
            taken: Vec::new(),
            workers: None,
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        self.push_as(p, priority, slow_limit() != usize::MAX && super::library::slow(&p.key))
    }

    fn push_as(&mut self, p: &Arc<Pending>, priority: u8, slow: bool) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED && p.screen && screens_first() {
            self.screen.push_back(p.clone());
        } else if priority == REQUESTED && p.compute {
            // A compute dispatch that waits ([`compute_waits`]) goes before
            // the requested graphics: behind them the head blend-shape map
            // was made 21 s after its dispatch (run r2), past any bound. Not
            // with the passes over the screen: a cold world frame asks for
            // hundreds of compute pipelines, and those passes wait too.
            self.compute.push_front(p.clone());
        } else if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else if slow {
            self.slow.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    /// `p`, requested and not taken by a worker yet, goes before every
    /// other compute pipeline: a dispatch waits for it now. Compute
    /// pipelines are made newest first anyway: first in, first out, a short
    /// wait would be spent on the hundreds dropped before it (a cold start's
    /// first world frame dispatches some 670), and a pipeline the title
    /// dispatches once would sit behind those it dispatches every frame.
    fn hurry(&mut self, p: &Arc<Pending>) {
        if p.queue_state.load(Ordering::Relaxed) == REQUESTED && !self.hurried.iter().any(|q| Arc::ptr_eq(q, p)) {
            self.hurried.push_front(p.clone());
            // (The worker kept for it may be asleep.)
            AVAILABLE.notify_all();
        }
    }

    /// Whether the last worker free is kept for pipelines a dispatch waits
    /// for now ([`Queue::hurry`]). The head map compiles in 1 to 3 ms, but on
    /// a cold start its dispatch waited 280, 289, 444 and 1,132 ms for a
    /// worker to come free (runs n5-new, n3-new, n4-new, m1-merged): the
    /// last was past [`COMPUTE_BUDGET`], and the heads had no skin. Only
    /// with [`compute_waits`] and four workers or more. A worker stuck in a
    /// compile that never returns counts as taken: the next one is kept.
    fn reserved(&mut self) -> bool {
        if !compute_waits() {
            return false;
        }
        let workers = *self.workers.get_or_insert_with(|| {
            worker_count(
                std::env::var("VICTORY_PIPELINE_WORKERS").ok().and_then(|v| v.trim().parse().ok()),
                std::thread::available_parallelism().map_or(2, |n| n.get()),
            )
        });
        self.taken.retain(|p| p.made.get().is_none());
        workers >= 4 && self.taken.len() + 1 >= workers
    }

    fn pop(&mut self, screens_only: bool, slow_limit: usize) -> Option<Arc<Pending>> {
        // An early entry can have been promoted and claimed already.
        let unclaimed = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        };
        // A compute pipeline can be in both of its lines.
        let waiting = |list: &mut VecDeque<Arc<Pending>>| {
            std::iter::from_fn(|| list.pop_front()).find(|p| p.queue_state.load(Ordering::Relaxed) == REQUESTED)
        };
        let mut p = self.screen.pop_front();
        if !screens_only {
            p = p.or_else(|| waiting(&mut self.hurried));
            if p.is_none() && !self.reserved() {
                p = waiting(&mut self.compute)
                    .or_else(|| self.requested.pop_front())
                    .or_else(|| unclaimed(&mut self.early));
                if p.is_none() && self.slow_now < slow_limit {
                    p = unclaimed(&mut self.slow);
                    if let Some(p) = &p {
                        p.was_slow.store(true, Ordering::Relaxed);
                        self.slow_now += 1;
                    }
                }
            }
            if let Some(p) = &p
                && compute_waits()
            {
                self.taken.push(p.clone());
            }
        }
        let p = p?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        // (One woken could be a worker that takes passes over the screen
        // alone: it would leave the pipeline to nobody.)
        AVAILABLE.notify_all();
    }
}

/// Workers that make passes over the screen alone, besides the others.
const SCREEN_WORKERS: usize = 2;

fn next(screens_only: bool) -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop(screens_only, slow_limit()) {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// How many workers make pipelines: `asked` (`VICTORY_PIPELINE_WORKERS`, 1 to
/// 64), else two thirds of the logical processors, 2 to 16. They run below the
/// title's own threads, and a cold cache queues hundreds at once.
fn worker_count(asked: Option<usize>, processors: usize) -> usize {
    match asked {
        Some(n) => n.clamp(1, 64),
        None => (processors * 2 / 3).clamp(2, 16),
    }
}

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count(
            std::env::var("VICTORY_PIPELINE_WORKERS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            std::thread::available_parallelism().map_or(2, |n| n.get()),
        );
        tracing::info!(workers, "pipeline workers");
        let screens = if screens_first() { SCREEN_WORKERS } else { 0 };
        for i in 0..workers + screens {
            let screens_only = i >= workers;
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording),
                    // and the streaming one, which is below normal itself
                    // (`VICTORY_PIPELINE_LOWEST=0`: level with it, as before).
                    let lowest = !screens_only && std::env::var_os("VICTORY_PIPELINE_LOWEST").is_none_or(|v| v != "0");
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let priority = if lowest { THREAD_PRIORITY_LOWEST } else { THREAD_PRIORITY_BELOW_NORMAL };
                        let _ = SetThreadPriority(GetCurrentThread(), priority);
                    }
                    loop {
                        let p = next(screens_only);
                        p.make();
                        if p.was_slow.load(Ordering::Relaxed) {
                            let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                            queue.slow_now -= 1;
                            AVAILABLE.notify_all();
                        }
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_pipeline_wait_keeps_newer_frame_budget() {
        use super::reserve_wait;
        let budget = Duration::from_millis(500);
        for shared in [false, true] {
            let mut spent = (0, Duration::ZERO);
            assert_eq!(reserve_wait(&mut spent, 0, budget, shared), budget);
            assert_eq!(reserve_wait(&mut spent, 8, budget, shared), budget);
            // In non-shared mode a completed wait, rather than its
            // reservation, charges the budget.
            if !shared {
                spent.1 = Duration::from_millis(250);
            }
            let before = spent;
            assert_eq!(reserve_wait(&mut spent, 7, budget, shared), Duration::ZERO);
            assert_eq!(spent, before);
            assert_eq!(reserve_wait(&mut spent, 8, budget, shared), budget - before.1);
            assert_eq!(reserve_wait(&mut spent, 9, budget, shared), budget);
        }
    }

    #[test]
    fn worker_count_follows_the_processors_and_the_switch() {
        assert_eq!(worker_count(None, 1), 2);
        assert_eq!(worker_count(None, 24), 16);
        assert_eq!(worker_count(None, 64), 16);
        assert_eq!(worker_count(Some(0), 24), 1);
        assert_eq!(worker_count(Some(20), 24), 20);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &earlier));
        assert!(queue.pop(false, 1).is_none());
    }

    #[test]
    fn pipelines_known_to_take_long_leave_workers_for_the_others() {
        let new = || Arc::new(Pending::new(Vec::new(), None));
        let (long, longer, asked, quick) = (new(), new(), new(), new());
        let mut queue = Queue::new();
        for p in [&long, &longer, &asked] {
            assert_eq!(queue.push_as(p, EARLY, true), Some(true));
        }
        assert_eq!(queue.push_as(&quick, EARLY, false), Some(true));
        // The quick one first, then one long one at a time; one a draw asks
        // for waits for nobody.
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &quick));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &long));
        assert!(queue.pop(false, 1).is_none());
        assert_eq!(queue.push_as(&asked, REQUESTED, true), Some(false));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &asked));
        assert!(!asked.was_slow.load(Ordering::Relaxed));
        assert!(queue.pop(false, 1).is_none());
        queue.slow_now -= 1;
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &longer));
        assert!(queue.pop(false, 1).is_none());
    }

    /// Passes over the screen go before all others, and workers of their own
    /// take nothing else.
    #[test]
    fn passes_over_the_screen_go_first() {
        let of = |screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.screen = screen;
            Arc::new(p)
        };
        let (world, screen) = (of(false), of(true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(Arc::ptr_eq(&queue.pop(true, 1).unwrap(), &screen));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }

    /// A compute dispatch that waits goes before requested graphics, after
    /// passes over the screen, and is not taken by their own workers.
    #[test]
    fn waiting_compute_dispatches_go_first() {
        let of = |compute, screen| {
            let mut p = Pending::new(Vec::new(), None);
            p.compute = compute;
            p.screen = screen;
            Arc::new(p)
        };
        let (world, compute, screen) = (of(false, false), of(true, false), of(false, true));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&world, REQUESTED), Some(true));
        assert_eq!(queue.push(&compute, REQUESTED), Some(true));
        assert_eq!(queue.push(&screen, REQUESTED), Some(true));
        assert!(queue.pop(true, 1).is_some_and(|p| Arc::ptr_eq(&p, &screen)));
        assert!(queue.pop(true, 1).is_none());
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &compute));
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &world));
    }

    /// Compute pipelines are made newest first, before requested graphics;
    /// one a dispatch waits for now goes first, once, and not to the screen
    /// passes' workers.
    #[test]
    fn a_pipeline_a_dispatch_waits_for_is_made_next() {
        let of = || {
            let mut p = Pending::new(Vec::new(), None);
            p.compute = true;
            Arc::new(p)
        };
        let (first, second, waited, later) = (of(), of(), of(), of());
        let world = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();
        queue.workers = Some(64);
        for p in [&world, &first, &waited, &second, &later] {
            assert_eq!(queue.push(p, REQUESTED), Some(true));
        }
        // A later dispatch of one already queued does not move it.
        assert_eq!(queue.push(&first, REQUESTED), None);
        queue.hurry(&waited);
        queue.hurry(&waited);
        assert!(queue.pop(true, 1).is_none());
        for p in [&waited, &later, &second, &first, &world] {
            assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), p));
        }
        queue.hurry(&first);
        assert!(queue.pop(false, 1).is_none());
    }

    /// With all workers but one busy, the last takes only a pipeline a
    /// dispatch waits for now (or a pass over the screen); a worker that
    /// finishes gives its place back.
    #[test]
    fn the_last_worker_is_kept_for_a_waited_for_pipeline() {
        if !compute_waits() {
            return;
        }
        let of = |compute| {
            let mut p = Pending::new(Vec::new(), None);
            p.compute = compute;
            Arc::new(p)
        };
        let mut queue = Queue::new();
        queue.workers = Some(4);
        let (a, b, c, d, waited) = (of(true), of(true), of(true), of(false), of(true));
        for p in [&a, &b, &c, &d] {
            queue.push(p, REQUESTED);
        }
        for p in [&c, &b, &a] {
            assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), p));
        }
        assert!(queue.pop(false, 1).is_none());
        queue.push(&waited, REQUESTED);
        assert!(queue.pop(false, 1).is_none());
        queue.hurry(&waited);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &waited));
        // One of the first three is made: its worker takes ordinary work again.
        let _ = c.made.set(None);
        let _ = waited.made.set(None);
        assert!(Arc::ptr_eq(&queue.pop(false, 1).unwrap(), &d));
    }

    /// A frame's compute waits end together, the budget after its first
    /// began; a newer frame has its own; a late thread of an older frame
    /// does not open a new one.
    #[test]
    fn compute_waits_end_with_the_frames_budget() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut w = FrameWaits::new();
        assert_eq!(w.begin(t0, 7), COMPUTE_BUDGET);
        assert_eq!(w.begin(t0 + ms(100), 7), COMPUTE_BUDGET - ms(100));
        assert_eq!(w.begin(t0 + COMPUTE_BUDGET, 7), Duration::ZERO);
        assert_eq!(w.begin(t0 + COMPUTE_BUDGET, 8), COMPUTE_BUDGET);
        assert_eq!(w.begin(t0 + COMPUTE_BUDGET + ms(50), 7), COMPUTE_BUDGET - ms(50));
    }

    /// A compute pipeline that is never made (a worker stuck in its compile)
    /// is waited for once, within the frame's compute budget; later
    /// dispatches wait no longer than for the others.
    #[test]
    fn a_compute_pipeline_not_made_has_one_wait() {
        let mut p = Pending::new(Vec::new(), None);
        p.compute = true;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);
        let started = Instant::now();
        assert!(ready(&p).is_none());
        let first = started.elapsed();
        assert!(first >= COMPUTE_CHANCE && first < COMPUTE_BUDGET + Duration::from_millis(500), "{first:?}");
        assert!(p.compute_waited.load(Ordering::Relaxed));
        let started = Instant::now();
        assert!(ready(&p).is_none());
        assert!(started.elapsed() <= wait_budget() + Duration::from_millis(500));
    }
}
