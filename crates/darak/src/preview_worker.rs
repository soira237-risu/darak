//! One active capture/build, one latest request, and one completed preview.
//!
//! Publication requires the active scan ID and epoch plus a version newer than
//! the last published packet; cancelled or superseded builds are discarded.
//! Capture copies live data under a non-blocking scanner lock, then builds the
//! tree outside that lock with cancellation checks. The active progress lease
//! is dropped before publication/idle notification; the completed packet owns
//! a tree rather than the scan progress. Replaced slots are dropped and wakers
//! are called outside the mailbox lock.
//!
//! `scan_cancel_releases_active_and_pending_progress` and
//! `waker_runs_after_publication_outside_mailbox_lock` pin these boundaries.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use darak_core::{ScanProgress, Tree};
use eframe::egui;

pub struct PreviewRequest {
    pub scan_id: u64,
    pub progress: Arc<ScanProgress>,
    pub after_version: u64,
}

pub struct PreviewPacket {
    pub scan_id: u64,
    pub version: u64,
    pub tree: Tree,
    #[cfg_attr(not(feature = "perf"), allow(dead_code))] // Optional accepted-packet telemetry.
    pub capture_time: Duration,
    #[cfg_attr(not(feature = "perf"), allow(dead_code))] // Optional accepted-packet telemetry.
    pub build_time: Duration,
}

pub struct PreviewController {
    shared: Arc<Shared>,
}

pub struct PreviewRuntime {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

struct Pending {
    request: PreviewRequest,
    epoch: u64,
}

#[derive(Default)]
struct State {
    scan_id: Option<u64>,
    // Pin identity without keeping a completed/cancelled scan alive.
    source: Option<Weak<ScanProgress>>,
    published_version: u64,
    pending: Option<Pending>,
    completion: Option<PreviewPacket>,
    active: bool,
    waker: Option<egui::Context>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    epoch: AtomicU64,
    stop: AtomicBool,
    #[cfg(test)]
    hook: Option<TestHook>,
}

#[cfg(test)]
type TestHook = Box<dyn Fn(Stage, u64, u64) + Send + Sync>;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Captured,
    Built,
}

impl PreviewRuntime {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (PreviewController, Self) {
        Self::start(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            #[cfg(test)]
            hook: None,
        })
    }

    fn start(shared: Shared) -> (PreviewController, Self) {
        let shared = Arc::new(shared);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("darak-preview".into())
            .spawn(move || {
                // Unwinding releases the active progress before clearing pending leases.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(&worker_shared);
                }));
                worker_shared.finish_worker();
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(_) => {
                shared.finish_worker();
                None
            }
        };
        (
            PreviewController {
                shared: Arc::clone(&shared),
            },
            Self { shared, worker },
        )
    }

    /// Main calls this after the event loop returns; the UI only signals shutdown.
    pub fn shutdown_and_join(mut self) {
        self.shared.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn with_hook(hook: TestHook) -> (PreviewController, Self) {
        Self::start(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            hook: Some(hook),
        })
    }
}

impl PreviewController {
    #[cfg(test)]
    pub(crate) fn wait_idle(&self) {
        let state = self.shared.lock();
        let (state, timed) = self.shared.changed.wait_timeout_while(
            state, Duration::from_secs(10), |state| state.active || state.pending.is_some(),
        ).unwrap();
        assert!(!timed.timed_out(), "preview worker did not become idle");
        assert!(!state.active);
    }

    pub fn set_waker(&mut self, ctx: egui::Context) {
        let old = self.shared.lock().waker.replace(ctx.clone());
        drop(old);
        ctx.request_repaint();
    }

    pub fn activate(&mut self, scan_id: u64) {
        let discarded = {
            let mut state = self.shared.lock();
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
            state.scan_id = Some(scan_id);
            state.source = None;
            state.published_version = 0;
            take_slots(&mut state)
        };
        drop(discarded);
        self.shared.changed.notify_all();
    }

    pub fn request_latest(&mut self, request: PreviewRequest) {
        let discarded = {
            let mut state = self.shared.lock();
            if self.shared.stop.load(Ordering::Acquire)
                || state.scan_id != Some(request.scan_id)
                || request.progress.is_cancelled()
            {
                drop(state);
                return;
            }
            let source = Arc::downgrade(&request.progress);
            if let Some(current) = &state.source {
                if !Weak::ptr_eq(current, &source) {
                    drop(state);
                    return;
                }
            } else {
                state.source = Some(source);
            }
            let epoch = self.shared.epoch.load(Ordering::Acquire);
            // Same-scan submissions replace pending only, preserving active work.
            state.pending.replace(Pending { request, epoch })
        };
        drop(discarded);
        self.shared.changed.notify_one();
    }

    pub fn try_take_latest(&mut self) -> Option<PreviewPacket> {
        self.shared.lock().completion.take()
    }

    pub fn invalidate(&mut self, scan_id: u64) {
        let discarded = {
            let mut state = self.shared.lock();
            if state.scan_id != Some(scan_id) {
                return;
            }
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
            state.scan_id = None;
            state.source = None;
            take_slots(&mut state)
        };
        drop(discarded);
        self.shared.changed.notify_all();
    }

    pub fn shutdown(&mut self) {
        self.shared.shutdown();
    }
}

impl Drop for PreviewController {
    fn drop(&mut self) {
        self.shared.shutdown();
    }
}

impl Drop for PreviewRuntime {
    fn drop(&mut self) {
        // Fallback drop never joins a worker on the UI thread.
        self.shared.shutdown();
    }
}

type Discarded = (Option<Pending>, Option<PreviewPacket>);

fn take_slots(state: &mut State) -> Discarded {
    (state.pending.take(), state.completion.take())
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn wake(&self, waker: Option<egui::Context>) {
        self.changed.notify_all();
        if let Some(waker) = waker {
            waker.request_repaint();
        }
    }

    fn shutdown(&self) {
        let (discarded, waker) = {
            let mut state = self.lock();
            self.stop.store(true, Ordering::Release);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            state.scan_id = None;
            state.source = None;
            (take_slots(&mut state), state.waker.clone())
        };
        drop(discarded);
        self.wake(waker);
    }

    fn finish_worker(&self) {
        self.shutdown();
        let waker = {
            let mut state = self.lock();
            state.active = false;
            state.waker.clone()
        };
        self.wake(waker);
    }
}

fn worker_loop(shared: &Shared) {
    loop {
        let (pending, published_version) = {
            let state = shared.lock();
            let mut state = shared
                .changed
                .wait_while(state, |state| {
                    state.pending.is_none() && !shared.stop.load(Ordering::Acquire)
                })
                .unwrap_or_else(|error| error.into_inner());
            if shared.stop.load(Ordering::Acquire) {
                return;
            }
            state.active = true;
            (
                state.pending.take().expect("pending request after wake"),
                state.published_version,
            )
        };
        let Pending { request, epoch } = pending;
        let cancelled = || {
            shared.stop.load(Ordering::Acquire)
                || shared.epoch.load(Ordering::Acquire) != epoch
                || request.progress.is_cancelled()
        };
        let packet = (|| {
            if cancelled() {
                return None;
            }
            let started = Instant::now();
            let captured = {
                // Do not wait for the scanner. A later submission retries contention.
                let live = request.progress.live.try_lock().ok()?;
                if live.version <= request.after_version.max(published_version)
                    || live.dirs.is_empty()
                {
                    return None;
                }
                live.capture()
            };
            let capture_time = started.elapsed();
            let version = captured.version();
            #[cfg(test)]
            if let Some(hook) = &shared.hook {
                hook(Stage::Captured, request.scan_id, version);
            }
            let started = Instant::now();
            let tree = captured.build_with_cancel(&cancelled).ok()??;
            let build_time = started.elapsed();
            #[cfg(test)]
            if let Some(hook) = &shared.hook {
                hook(Stage::Built, request.scan_id, version);
            }
            Some(PreviewPacket {
                scan_id: request.scan_id,
                version,
                tree,
                capture_time,
                build_time,
            })
        })();
        let scan_cancelled = request.progress.is_cancelled();
        let scan_id = request.scan_id;
        // Release the active lease before idle notification or publication.
        drop(request);
        let (discarded, waker) = {
            let mut state = shared.lock();
            state.active = false;
            let publish = !scan_cancelled
                && !shared.stop.load(Ordering::Acquire)
                && shared.epoch.load(Ordering::Acquire) == epoch
                && state.scan_id == Some(scan_id)
                && packet
                    .as_ref()
                    .is_some_and(|packet| packet.version > state.published_version);
            let discarded = if publish {
                let packet = packet.expect("checked publication");
                state.published_version = packet.version;
                state.completion.replace(packet)
            } else {
                packet
            };
            (discarded, state.waker.clone())
        };
        drop(discarded);
        shared.wake(waker);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darak_core::NO_NODE;
    use darak_core::scan::LiveDir;
    use darak_core::{ScanProgress, Tree};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn update(progress: &ScanProgress, version: u64, name: &str) {
        let mut live = progress.live.lock().unwrap();
        live.version = version;
        live.dirs = vec![LiveDir {
            parent: NO_NODE,
            depth: 0,
            name: name.into(),
            alloc: version * 10,
            logical: version * 9,
            files: 1,
            direct_alloc: version * 10,
            direct_logical: version * 9,
            direct_files: 1,
        }];
    }

    fn progress(version: u64, name: &str) -> Arc<ScanProgress> {
        let progress = Arc::new(ScanProgress::default());
        update(&progress, version, name);
        progress
    }

    fn request(progress: &Arc<ScanProgress>, scan_id: u64, after_version: u64) -> PreviewRequest {
        PreviewRequest {
            scan_id,
            progress: Arc::clone(progress),
            after_version,
        }
    }

    fn gated(
        stage: Stage,
    ) -> (
        PreviewController,
        PreviewRuntime,
        Receiver<(u64, u64)>,
        Sender<bool>,
    ) {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<bool>();
        let release_rx = Mutex::new(release_rx);
        let (controller, runtime) =
            PreviewRuntime::with_hook(Box::new(move |at, scan_id, version| {
                if at == stage {
                    started_tx.send((scan_id, version)).unwrap();
                    let panic = release_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                    assert!(!panic, "injected preview worker failure");
                }
            }));
        (controller, runtime, started_rx, release_tx)
    }

    fn idle(shared: &Shared) {
        let state = shared.lock();
        let (state, timed) = shared
            .changed
            .wait_timeout_while(state, TIMEOUT, |state| {
                state.active || state.pending.is_some()
            })
            .unwrap();
        assert!(!timed.timed_out(), "worker did not become idle");
        assert!(!state.active);
    }

    #[test]
    fn preview_slots_remain_bounded_under_request_flood() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let progress = progress(1, "initial");
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), (1, 1));
        for version in 2..=1_000 {
            update(&progress, version, "latest");
            controller.request_latest(request(&progress, 1, 0));
            let state = controller.shared.lock();
            assert!(state.active);
            assert!(state.pending.is_some());
            assert!(state.completion.is_none());
            assert_eq!(
                Arc::strong_count(&progress),
                3,
                "only active + pending own progress"
            );
        }
        release.send(false).unwrap();
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), (1, 1_000));
        let first = controller
            .try_take_latest()
            .expect("same-scan flood must not cancel active build");
        assert_eq!(first.version, 1);
        assert_eq!(first.tree.name(0), "initial");
        release.send(false).unwrap();
        idle(&controller.shared);
        let latest = controller.try_take_latest().unwrap();
        assert_eq!(latest.version, 1_000);
        assert_eq!(latest.tree.version, latest.version);
        assert_eq!(latest.tree.name(0), "latest");
        assert_eq!(Arc::strong_count(&progress), 1);
        runtime.shutdown_and_join();
    }

    #[test]
    fn late_preview_cannot_replace_final_or_new_scan() {
        let (mut controller, runtime, started, release) = gated(Stage::Built);
        let old = progress(1, "old");
        controller.activate(1);
        controller.request_latest(request(&old, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        let (final_tx, final_rx) = mpsc::channel();
        final_tx.send(Tree::default()).unwrap();
        controller.invalidate(1);
        let final_tree = final_rx
            .try_recv()
            .expect("final result is independent of paused preview");
        assert!(final_tree.is_empty());
        assert!(controller.try_take_latest().is_none());
        let new = progress(7, "new");
        controller.activate(2);
        controller.request_latest(request(&new, 2, 0));
        controller.invalidate(1); // A late old-scan invalidation cannot invalidate scan 2.
        release.send(false).unwrap();
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), (2, 7));
        assert!(controller.try_take_latest().is_none());
        assert_eq!(Arc::strong_count(&old), 1);
        release.send(false).unwrap();
        idle(&controller.shared);
        let packet = controller.try_take_latest().unwrap();
        assert_eq!(packet.scan_id, 2);
        assert_eq!(packet.tree.name(0), "new");
        runtime.shutdown_and_join();
    }

    #[test]
    fn unpublished_capture_does_not_advance_seen_version() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let progress = progress(5, "captured");
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(controller.shared.lock().published_version, 0);
        controller.invalidate(1);
        release.send(false).unwrap();
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        controller.activate(2);
        controller.request_latest(request(&progress, 2, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        release.send(false).unwrap();
        idle(&controller.shared);
        assert_eq!(controller.try_take_latest().unwrap().version, 5);
        runtime.shutdown_and_join();
    }

    #[test]
    fn mft_empty_live_can_transition_to_walk_preview() {
        let (mut controller, runtime) = PreviewRuntime::new();
        let progress = Arc::new(ScanProgress::default());
        progress.live.lock().unwrap().version = 3;
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        assert_eq!(controller.shared.lock().published_version, 0);
        update(&progress, 4, "Walk fallback");
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        let packet = controller.try_take_latest().unwrap();
        assert_eq!(packet.version, 4);
        assert_eq!(packet.tree.name(0), "Walk fallback");
        runtime.shutdown_and_join();
    }

    #[test]
    fn contended_live_does_not_advance_seen_version() {
        let (mut controller, runtime) = PreviewRuntime::new();
        let progress = progress(8, "locked");
        controller.activate(1);
        let live = progress.live.lock().unwrap();
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        assert_eq!(controller.shared.lock().published_version, 0);
        drop(live);
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        assert_eq!(controller.try_take_latest().unwrap().version, 8);
        runtime.shutdown_and_join();
    }

    #[test]
    fn published_packets_are_newer_and_completion_is_replaced() {
        let (mut controller, runtime) = PreviewRuntime::new();
        let progress = progress(3, "three");
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 3));
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        assert_eq!(
            controller
                .shared
                .lock()
                .completion
                .as_ref()
                .unwrap()
                .version,
            3
        );
        update(&progress, 4, "four");
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        assert_eq!(controller.try_take_latest().unwrap().version, 4);
        controller.request_latest(request(&progress, 1, 0));
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        runtime.shutdown_and_join();
    }

    #[test]
    fn capture_and_version_belong_to_requested_source() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let source = progress(2, "captured source");
        let unrelated = progress(90, "unrelated");
        controller.activate(1);
        controller.request_latest(request(&source, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        // The live lock is released before build; later mutations cannot label the capture.
        update(&source, 3, "later live");
        controller.request_latest(request(&unrelated, 1, 0));
        controller.request_latest(request(&unrelated, 999, 0));
        assert!(controller.shared.lock().pending.is_none());
        assert_eq!(Arc::strong_count(&unrelated), 1);
        release.send(false).unwrap();
        idle(&controller.shared);
        let packet = controller.try_take_latest().unwrap();
        assert_eq!(packet.version, 2);
        assert_eq!(packet.tree.version, 2);
        assert_eq!(packet.tree.name(0), "captured source");
        assert_eq!(packet.tree.node(0).alloc, 20);
        runtime.shutdown_and_join();
    }

    #[test]
    fn scan_cancel_releases_active_and_pending_progress() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let progress = progress(1, "cancelled");
        let lease = Arc::downgrade(&progress);
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.request_latest(request(&progress, 1, 0));
        progress.cancel();
        drop(progress);
        release.send(false).unwrap();
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        assert!(lease.upgrade().is_none());
        runtime.shutdown_and_join();
    }

    #[test]
    fn preview_shutdown_releases_progress_and_wakes_idle_worker() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let progress = progress(1, "shutdown");
        let lease = Arc::downgrade(&progress);
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.request_latest(request(&progress, 1, 0));
        drop(progress);
        controller.shutdown();
        assert!(controller.shared.lock().pending.is_none());
        release.send(false).unwrap();
        runtime.shutdown_and_join();
        assert!(lease.upgrade().is_none());
        assert!(controller.try_take_latest().is_none());
        let (controller, runtime) = PreviewRuntime::new();
        drop(controller); // Controller drop signals only; runtime owns the join.
        let (done_tx, done_rx) = mpsc::channel();
        let joiner = thread::spawn(move || {
            runtime.shutdown_and_join();
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(TIMEOUT)
            .expect("idle shutdown must wake worker");
        joiner.join().unwrap();
    }

    #[test]
    fn failed_worker_releases_leases_and_rejects_requests() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let progress = progress(1, "panic");
        let lease = Arc::downgrade(&progress);
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.request_latest(request(&progress, 1, 0));
        drop(progress);
        release.send(true).unwrap();
        idle(&controller.shared);
        assert!(lease.upgrade().is_none());
        assert!(controller.try_take_latest().is_none());
        let next = self::progress(2, "after failure");
        controller.activate(2);
        controller.request_latest(request(&next, 2, 0));
        assert_eq!(Arc::strong_count(&next), 1);
        runtime.shutdown_and_join();
    }

    #[test]
    fn controller_drop_only_signals_paused_worker() {
        let (mut controller, runtime, started, release) = gated(Stage::Captured);
        let progress = progress(1, "drop");
        let lease = Arc::downgrade(&progress);
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.request_latest(request(&progress, 1, 0));
        drop(progress);
        drop(controller);
        // Reaching this send while the worker is gated proves Drop did not join.
        release.send(false).unwrap();
        runtime.shutdown_and_join();
        assert!(lease.upgrade().is_none());
    }

    #[test]
    fn initialization_failure_releases_pending_progress() {
        // Exercise the cleanup used when thread::Builder::spawn returns Err.
        // No test-specific product spawn function is needed.
        let progress = progress(1, "init failure");
        let lease = Arc::downgrade(&progress);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                scan_id: Some(1),
                source: Some(Arc::downgrade(&progress)),
                pending: Some(Pending {
                    request: request(&progress, 1, 0),
                    epoch: 0,
                }),
                ..State::default()
            }),
            changed: Condvar::new(),
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            hook: None,
        });
        drop(progress);
        shared.finish_worker();
        assert!(lease.upgrade().is_none());
        assert!(shared.stop.load(Ordering::Acquire));
        let mut controller = PreviewController { shared };
        let next = self::progress(2, "rejected");
        controller.activate(2);
        controller.request_latest(request(&next, 2, 0));
        assert_eq!(Arc::strong_count(&next), 1);
    }

    #[test]
    fn waker_runs_after_publication_outside_mailbox_lock() {
        let (mut controller, runtime, started, release) = gated(Stage::Built);
        let progress = progress(1, "repaint");
        controller.activate(1);
        controller.request_latest(request(&progress, 1, 0));
        started.recv_timeout(TIMEOUT).unwrap();
        let ctx = egui::Context::default();
        let weak = Arc::downgrade(&controller.shared);
        let (repaint_tx, repaint_rx) = mpsc::channel();
        ctx.set_request_repaint_callback(move |_| {
            if let Some(shared) = weak.upgrade() {
                let state = shared
                    .state
                    .try_lock()
                    .expect("repaint callback cannot run under mailbox lock");
                let _ = repaint_tx.send(state.completion.is_some());
            }
        });
        controller.set_waker(ctx.clone());
        // Settle egui's coalesced repaint requests before releasing publication.
        for _ in 0..3 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
            output.textures_delta.clear();
        }
        while repaint_rx.try_recv().is_ok() {}
        release.send(false).unwrap();
        assert!(
            repaint_rx.recv_timeout(TIMEOUT).unwrap(),
            "worker must publish before waking UI"
        );
        idle(&controller.shared);
        let packet = controller.try_take_latest().unwrap();
        assert_eq!(packet.version, 1);
        assert!(packet.capture_time < TIMEOUT);
        assert!(packet.build_time < TIMEOUT);
        runtime.shutdown_and_join();
    }

    #[test]
    fn poisoned_capture_releases_progress_without_stopping_worker() {
        let (mut controller, runtime) = PreviewRuntime::new();
        let source = progress(1, "poisoned");
        let poison = Arc::clone(&source);
        assert!(
            thread::spawn(move || {
                let _guard = poison.live.lock().unwrap();
                panic!("injected live mutex poison");
            })
            .join()
            .is_err()
        );
        controller.activate(1);
        controller.request_latest(request(&source, 1, 0));
        idle(&controller.shared);
        assert!(controller.try_take_latest().is_none());
        assert_eq!(controller.shared.lock().published_version, 0);
        assert_eq!(Arc::strong_count(&source), 1);
        let next = progress(2, "healthy");
        controller.activate(2);
        controller.request_latest(request(&next, 2, 0));
        idle(&controller.shared);
        assert_eq!(
            controller.try_take_latest().unwrap().tree.name(0),
            "healthy"
        );
        runtime.shutdown_and_join();
    }
}
