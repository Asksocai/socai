//! Bounded, content-free measurements scoped to one tool invocation.
//! No I/O or model calls on the measured path. Cloned contexts share the
//! collector, while each nested scope carries its own explicit parent.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct ToolTimings(Arc<TimingState>);
struct TimingState {
    phases: Mutex<Vec<ToolPhase>>,
    anchor_ns: u64,
    clock: Instant,
    started: AtomicU64,
    finished: AtomicU64,
    dropped: AtomicU64,
}
impl Default for ToolTimings {
    fn default() -> Self {
        Self(Arc::new(TimingState {
            anchor_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(u64::MAX as u128) as u64,
            clock: Instant::now(),
            phases: Mutex::new(Vec::new()),
            started: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }))
    }
}

#[derive(Clone)]
pub struct ToolPhase {
    pub id: String,
    pub parent: Option<String>,
    pub phase: &'static str,
    pub post_id: Option<String>,
    pub post_index: Option<u64>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub succeeded: bool,
}

pub struct ToolPhaseGuard {
    timings: ToolTimings,
    record: ToolPhase,
}
pub struct ToolPhaseBatch {
    pub phases: Vec<ToolPhase>,
    pub interval: Option<(u64, u64)>,
    pub dropped: u64,
}

impl ToolTimings {
    pub fn begin(
        &self,
        parent: Option<String>,
        phase: &'static str,
        post_id: Option<&str>,
        post_index: Option<u64>,
    ) -> ToolPhaseGuard {
        // IDs only, never URL/query/title/body. X uses numeric IDs, XHS uses
        // 24-char hex IDs. Invalid values are omitted rather than truncated.
        let post_id = post_id
            .filter(|s| {
                (5..=24).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
                    || s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(str::to_owned);
        ToolPhaseGuard {
            timings: self.clone(),
            record: ToolPhase {
                id: uuid::Uuid::new_v4().simple().to_string()[..16].to_string(),
                parent,
                phase,
                post_id,
                post_index,
                start_ns: self.now_ns(),
                end_ns: 0,
                succeeded: false,
            },
        }
    }

    fn now_ns(&self) -> u64 {
        self.0
            .anchor_ns
            .saturating_add(self.0.clock.elapsed().as_nanos().min(u64::MAX as u128) as u64)
    }

    pub fn start(&self) {
        self.0.started.store(self.now_ns(), Ordering::Relaxed)
    }
    pub fn finish(&self) {
        self.0.finished.store(self.now_ns(), Ordering::Relaxed)
    }
    pub fn drain(&self) -> ToolPhaseBatch {
        let start = self.0.started.load(Ordering::Relaxed);
        let end = self.0.finished.load(Ordering::Relaxed);
        ToolPhaseBatch {
            phases: self
                .0
                .phases
                .lock()
                .map(|mut phases| std::mem::take(&mut *phases))
                .unwrap_or_default(),
            interval: (start > 0 && end >= start).then_some((start, end)),
            dropped: self.0.dropped.swap(0, Ordering::Relaxed),
        }
    }
}

impl ToolPhaseGuard {
    pub fn id(&self) -> &str {
        &self.record.id
    }
    pub fn finish(mut self, succeeded: bool) {
        self.record.succeeded = succeeded;
    }
}

impl Drop for ToolPhaseGuard {
    fn drop(&mut self) {
        self.record.end_ns = self.timings.now_ns();
        if let Ok(mut phases) = self.timings.0.phases.lock() {
            if phases.len() < 300 {
                phases.push(self.record.clone());
            } else {
                self.timings.0.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
