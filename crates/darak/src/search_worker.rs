//! One cancellable search, one latest request, and one completed result.
//!
//! Requests are bounded to one active lease and one replaceable pending lease;
//! the completion slot carries IDs/highlights, never an `Arc<Tree>`. Advancing
//! the epoch cancels obsolete work before a newer request can publish.
//! Displaced pending leases are dropped outside the mailbox lock; `retiring`
//! counts those drops until they finish. A `Released` fence is published only
//! when active/pending leases are gone and `retiring == 0`, after dropping the
//! worker-owned tree references. External owners may still make mutation Busy.
//! Publication precedes the waker call, which always runs outside the lock.
//!
//! These boundaries are pinned by `release_waits_for_active_and_pending_leases`,
//! `failure_and_fence_wait_for_a_displaced_lease_drop`, and
//! `worker_failure_releases_all_tree_leases` below.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use darak_core::search::{Query, SearchCancelled, SearchResult, search_with_cancel};
use darak_core::{NodeId, SizeMode, Tree};
use eframe::egui;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchKey {
    pub doc_id: u64,
    pub tree_version: u64,
    pub query: String,
    pub scope: NodeId,
    pub mode: SizeMode,
}

pub struct SearchRequest {
    pub generation: u64,
    pub key: SearchKey,
    pub tree: Arc<Tree>,
    pub query: Query,
    pub edited_at: Instant,
}

pub struct SearchCompletion {
    pub generation: u64,
    pub key: SearchKey,
    pub result: SearchResult,
    pub bits: Vec<u8>,
    pub timings: SearchTimings,
    pub edited_at: Instant,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SearchTimings {
    #[cfg_attr(not(feature = "perf"), allow(dead_code))]
    pub queued: Duration,
    pub search: Duration,
    #[cfg_attr(not(feature = "perf"), allow(dead_code))]
    pub highlight: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseFence {
    pub token: u64,
    pub doc_id: u64,
    pub tree_version: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SearchControlEvent {
    Released(ReleaseFence),
    WorkerFailed(String),
}

pub struct SearchController {
    shared: Arc<Shared>,
}

pub struct SearchRuntime {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// Optional boundary-only instrumentation; no events enter the worker mailbox.
#[cfg(feature = "perf")]
#[derive(Clone, Copy, Debug)]
pub struct CancellationRelease {
    pub doc_id: u64,
    pub generation: u64,
    pub cancelled_at: Instant,
    pub released_at: Instant,
}

#[cfg(feature = "perf")]
pub type ReleaseObserver = Arc<dyn Fn(CancellationRelease) + Send + Sync>;

#[cfg(feature = "perf")]
#[derive(Default)]
struct LeaseStamp {
    state: Mutex<(Option<Instant>, bool)>,
}

#[cfg(feature = "perf")]
impl LeaseStamp {
    fn cancel(&self, at: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.1 {
            state.0.get_or_insert(at);
        }
    }
}

#[cfg(feature = "perf")]
struct LeaseTrace {
    stamp: Arc<LeaseStamp>,
    observer: ReleaseObserver,
    doc_id: u64,
    generation: u64,
}

#[cfg(feature = "perf")]
impl Drop for LeaseTrace {
    fn drop(&mut self) {
        // Pending drops request first; the active tree binding also drops first,
        // including during unwind. State retains only the stamp, never this guard.
        let released_at = Instant::now();
        let cancelled_at = {
            let mut state = self
                .stamp
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.1 = true;
            state.0
        };
        if let Some(cancelled_at) = cancelled_at.filter(|&at| at <= released_at) {
            (self.observer)(CancellationRelease {
                doc_id: self.doc_id,
                generation: self.generation,
                cancelled_at,
                released_at,
            });
        }
    }
}

struct Pending {
    request: SearchRequest,
    epoch: u64,
    queued_at: Instant,
    // Keep after request: retirement must drop its tree before the trace guard.
    #[cfg(feature = "perf")]
    trace: Option<LeaseTrace>,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    completion: Option<SearchCompletion>,
    active: bool,
    #[cfg(feature = "perf")]
    active_stamp: Option<Arc<LeaseStamp>>,
    #[cfg(feature = "perf")]
    release_observer: Option<ReleaseObserver>,
    // Requests taken by a controller are dropped outside the mailbox lock.
    // Their leases still count until the controller reports those drops done.
    retiring: usize,
    blocked: Option<ReleaseFence>,
    fences: VecDeque<ReleaseFence>,
    controls: VecDeque<SearchControlEvent>,
    failed: bool,
    failure: Option<String>,
    waker: Option<egui::Context>,
}

#[cfg(all(test, feature = "perf"))]
#[derive(Clone, Copy)]
enum TraceBoundary {
    Published,
    Released,
}
#[cfg(all(test, feature = "perf"))]
type TraceBoundaryHook = Arc<dyn Fn(TraceBoundary) + Send + Sync>;

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    epoch: AtomicU64,
    stop: AtomicBool,
    #[cfg(test)]
    hook: Option<Box<dyn Fn(u64) + Send + Sync>>,
    #[cfg(all(test, feature = "perf"))]
    trace_hook: Mutex<Option<TraceBoundaryHook>>,
}

impl SearchRuntime {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (SearchController, Self) {
        Self::start(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            #[cfg(test)]
            hook: None,
            #[cfg(all(test, feature = "perf"))]
            trace_hook: Mutex::new(None),
        })
    }

    fn start(shared: Shared) -> (SearchController, Self) {
        let shared = Arc::new(shared);
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("darak-search".into())
            .spawn(move || {
                // Unwinding drops the active request before the exit cleanup can
                // acknowledge a release fence or report worker failure.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(&worker_shared);
                }));
                let failure = outcome.err().map(|payload| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "Search worker panicked".into())
                });
                worker_shared.finish_worker(failure);
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                shared.finish_worker(Some(format!("Could not start search worker: {error}")));
                None
            }
        };
        (
            SearchController {
                shared: Arc::clone(&shared),
            },
            Self { shared, worker },
        )
    }

    /// Called by main after the event loop has returned, never by the UI.
    pub fn shutdown_and_join(mut self) {
        self.shared.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn with_hook(hook: Box<dyn Fn(u64) + Send + Sync>) -> (SearchController, Self) {
        Self::start(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            hook: Some(hook),
            #[cfg(all(test, feature = "perf"))]
            trace_hook: Mutex::new(None),
        })
    }
}

impl SearchController {
    #[cfg(feature = "perf")]
    pub fn set_release_observer(&mut self, observer: ReleaseObserver) {
        let old = self.shared.lock().release_observer.replace(observer);
        drop(old);
    }

    pub fn set_waker(&mut self, ctx: egui::Context) {
        // Even Context callbacks must not run while the mailbox is locked.
        let old = self.shared.lock().waker.replace(ctx.clone());
        drop(old);
        ctx.request_repaint();
    }

    pub fn submit_latest(&mut self, request: SearchRequest) {
        let discarded = {
            let mut state = self.shared.lock();
            if state.failed || state.blocked.is_some() || self.shared.stop.load(Ordering::Acquire) {
                drop(state);
                return;
            }
            // Stamp cancellation before the worker can observe its atomic signal.
            let discarded = take_slots(&mut state);
            let epoch = self
                .shared
                .epoch
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1);
            #[cfg(all(test, feature = "perf"))]
            self.shared.trace_boundary(TraceBoundary::Published);
            #[cfg(feature = "perf")]
            let trace = state.release_observer.as_ref().map(|observer| LeaseTrace {
                stamp: Arc::default(),
                observer: Arc::clone(observer),
                doc_id: request.key.doc_id,
                generation: request.generation,
            });
            state.pending = Some(Pending {
                request,
                epoch,
                queued_at: Instant::now(),
                #[cfg(feature = "perf")]
                trace,
            });
            discarded
        };
        self.shared.retire(discarded);
    }

    pub fn invalidate(&mut self) {
        let discarded = {
            let mut state = self.shared.lock();
            let discarded = take_slots(&mut state);
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
            #[cfg(all(test, feature = "perf"))]
            self.shared.trace_boundary(TraceBoundary::Published);
            discarded
        };
        self.shared.retire(discarded);
    }

    pub fn release_for_mutation(&mut self, fence: ReleaseFence) {
        let discarded = {
            let mut state = self.shared.lock();
            let discarded = take_slots(&mut state);
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
            #[cfg(all(test, feature = "perf"))]
            self.shared.trace_boundary(TraceBoundary::Published);
            state.blocked = Some(fence);
            state.fences.push_back(fence);
            discarded
        };
        self.shared.retire(discarded);
    }

    pub fn resume_after_mutation(&mut self, fence: ReleaseFence) {
        let mut state = self.shared.lock();
        if state.blocked == Some(fence) && !state.fences.contains(&fence) {
            state.blocked = None;
        }
    }

    pub fn try_take_completion(&mut self) -> Option<SearchCompletion> {
        self.shared.lock().completion.take()
    }

    pub fn try_take_control(&mut self) -> Option<SearchControlEvent> {
        self.shared.lock().controls.pop_front()
    }

    pub fn shutdown(&mut self) {
        self.shared.shutdown();
    }
}

impl Drop for SearchController {
    fn drop(&mut self) {
        self.shared.shutdown();
    }
}

impl Drop for SearchRuntime {
    fn drop(&mut self) {
        // A fallback drop signals only. Explicit joining belongs after run_native.
        self.shared.shutdown();
    }
}

type Discarded = (Option<Pending>, Option<SearchCompletion>);

fn take_slots(state: &mut State) -> Discarded {
    #[cfg(feature = "perf")]
    {
        let at = Instant::now();
        if let Some(stamp) = &state.active_stamp {
            stamp.cancel(at);
        }
        if let Some(trace) = state
            .pending
            .as_ref()
            .and_then(|pending| pending.trace.as_ref())
        {
            trace.stamp.cancel(at);
        }
    }
    let pending = state.pending.take();
    state.retiring += usize::from(pending.is_some());
    (pending, state.completion.take())
}

impl Shared {
    #[cfg(all(test, feature = "perf"))]
    fn trace_boundary(&self, boundary: TraceBoundary) {
        let hook = self.trace_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(boundary);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn retire(&self, discarded: Discarded) {
        let retired = usize::from(discarded.0.is_some());
        drop(discarded);
        let waker = {
            let mut state = self.lock();
            state.retiring -= retired;
            flush_controls(&mut state);
            state.waker.clone()
        };
        self.wake(waker);
    }

    fn wake(&self, waker: Option<egui::Context>) {
        self.changed.notify_all();
        if let Some(waker) = waker {
            waker.request_repaint();
        }
    }

    fn shutdown(&self) {
        let discarded = {
            let mut state = self.lock();
            let discarded = take_slots(&mut state);
            self.stop.store(true, Ordering::Release);
            #[cfg(all(test, feature = "perf"))]
            self.trace_boundary(TraceBoundary::Published);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            discarded
        };
        self.retire(discarded);
    }

    fn finish_worker(&self, failure: Option<String>) {
        let discarded = {
            let mut state = self.lock();
            let discarded = take_slots(&mut state);
            self.stop.store(true, Ordering::Release);
            state.active = false;
            #[cfg(feature = "perf")]
            {
                state.active_stamp = None;
            }
            state.failed |= failure.is_some();
            state.failure = failure;
            discarded
        };
        self.retire(discarded);
    }
}

fn flush_controls(state: &mut State) {
    if !state.active && state.pending.is_none() && state.retiring == 0 {
        state
            .controls
            .extend(state.fences.drain(..).map(SearchControlEvent::Released));
        if let Some(failure) = state.failure.take() {
            state
                .controls
                .push_back(SearchControlEvent::WorkerFailed(failure));
        }
    }
}

fn worker_loop(shared: &Shared) {
    loop {
        let pending = {
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
            let pending = state.pending.take().expect("pending request after wake");
            #[cfg(feature = "perf")]
            {
                state.active_stamp = pending.trace.as_ref().map(|trace| Arc::clone(&trace.stamp));
            }
            pending
        };
        let Pending {
            request,
            epoch,
            queued_at,
            #[cfg(feature = "perf")]
            trace,
        } = pending;
        // `tree` is bound after `trace`, so unwind drops the Arc before the guard.
        let SearchRequest {
            generation,
            key,
            tree,
            query,
            edited_at,
        } = request;
        let queued = queued_at.elapsed();
        #[cfg(test)]
        if let Some(hook) = &shared.hook {
            hook(generation);
        }
        let cancelled =
            || shared.stop.load(Ordering::Acquire) || shared.epoch.load(Ordering::Acquire) != epoch;
        let completion = (|| {
            let started = Instant::now();
            let result =
                search_with_cancel(&tree, &query, key.mode, key.scope, usize::MAX, &cancelled)?;
            let search = started.elapsed();
            let started = Instant::now();
            let bits = build_search_bits_with_cancel(&tree, &result.hits, &cancelled)?;
            Ok::<_, SearchCancelled>(SearchCompletion {
                generation,
                key,
                result,
                bits,
                timings: SearchTimings {
                    queued,
                    search,
                    highlight: started.elapsed(),
                },
                edited_at,
            })
        })()
        .ok();
        // Neither a completion nor a release acknowledgement retains a tree.
        drop(tree);
        #[cfg(feature = "perf")]
        drop(trace);
        #[cfg(all(test, feature = "perf"))]
        shared.trace_boundary(TraceBoundary::Released);
        let (discarded, waker) = {
            let mut state = shared.lock();
            state.active = false;
            #[cfg(feature = "perf")]
            {
                state.active_stamp = None;
            }
            let discarded = if !cancelled() && state.blocked.is_none() && !state.failed {
                std::mem::replace(&mut state.completion, completion)
            } else {
                completion
            };
            flush_controls(&mut state);
            (discarded, state.waker.clone())
        };
        drop(discarded);
        shared.wake(waker);
    }
}

fn build_search_bits_with_cancel(
    tree: &Tree,
    hits: &[NodeId],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<u8>, SearchCancelled> {
    let check = || {
        if cancelled() {
            Err(SearchCancelled)
        } else {
            Ok(())
        }
    };
    check()?;
    let mut bits = vec![0; tree.len()];
    let mut steps = 0usize;
    let mut tick = || {
        steps += 1;
        if steps.is_multiple_of(256) {
            check()
        } else {
            Ok(())
        }
    };
    for &hit in hits {
        tick()?;
        bits[hit as usize] |= 1;
        let mut parent = tree.node(hit).parent;
        while parent != darak_core::NO_NODE && bits[parent as usize] & 2 == 0 {
            tick()?;
            bits[parent as usize] |= 2;
            parent = tree.node(parent).parent;
        }
    }
    // Nodes are BFS ordered: the parent's descendant bit is already complete.
    for id in 1..tree.len() {
        tick()?;
        let parent = tree.nodes[id].parent as usize;
        if bits[parent] & 5 != 0 {
            bits[id] |= 4;
        }
    }
    check()?;
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use darak_core::{Kind, NO_NODE, Node};
    use std::sync::mpsc::{self, Receiver, Sender};

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn tree() -> Arc<Tree> {
        let mut tree = Tree::default();
        for (name, kind, parent, size, first, count) in [
            ("C:\\", Kind::Dir, NO_NODE, 100, 1, 2),
            ("match", Kind::Dir, 0, 60, 3, 2),
            ("sibling", Kind::File, 0, 40, 0, 0),
            ("match-child", Kind::File, 1, 30, 0, 0),
            ("other-child", Kind::File, 1, 30, 0, 0),
        ] {
            tree.nodes.push(Node {
                alloc: size,
                logical: size,
                name_off: tree.names.len() as u32,
                parent,
                first_child: first,
                child_count: count,
                files: count,
                mtime: 0,
                name_len: name.len() as u16,
                kind,
                flags: 0,
            });
            tree.names.push_str(name);
        }
        Arc::new(tree)
    }

    fn request(tree: &Arc<Tree>, generation: u64) -> SearchRequest {
        SearchRequest {
            generation,
            key: SearchKey {
                doc_id: 1,
                tree_version: tree.version,
                query: "match".into(),
                scope: Tree::ROOT,
                mode: SizeMode::Allocated,
            },
            tree: Arc::clone(tree),
            query: Query::parse("match", 0).unwrap(),
            edited_at: Instant::now(),
        }
    }

    fn fence(token: u64) -> ReleaseFence {
        ReleaseFence {
            token,
            doc_id: 1,
            tree_version: 0,
        }
    }

    // Every active search pauses before borrowing its tree. Tests release it
    // explicitly; timeouts detect deadlocks without timing-based assertions.
    fn gated() -> (SearchController, SearchRuntime, Receiver<u64>, Sender<bool>) {
        let (started_tx, started_rx) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel::<bool>();
        let proceed_rx = Mutex::new(proceed_rx);
        let (controller, runtime) = SearchRuntime::with_hook(Box::new(move |generation| {
            started_tx.send(generation).unwrap();
            let panic = proceed_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
            assert!(!panic, "injected search worker failure");
        }));
        (controller, runtime, started_rx, proceed_tx)
    }

    fn wait_state(controller: &SearchController, ready: impl Fn(&State) -> bool) {
        let guard = controller.shared.state.lock().unwrap();
        let (guard, timed) = controller
            .shared
            .changed
            .wait_timeout_while(guard, TIMEOUT, |state| !ready(state))
            .unwrap();
        assert!(ready(&guard), "worker state timed out: {:?}", timed);
    }

    fn join(runtime: SearchRuntime) {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            runtime.shutdown_and_join();
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(TIMEOUT)
            .expect("worker shutdown timed out");
    }

    #[test]
    fn latest_request_replaces_pending_and_completion() {
        let tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        controller.submit_latest(request(&tree, 1));
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 1);
        controller.submit_latest(request(&tree, 2));
        controller.submit_latest(request(&tree, 3));
        {
            let state = controller.shared.state.lock().unwrap();
            assert!(state.active);
            assert_eq!(state.pending.as_ref().unwrap().request.generation, 3);
            assert!(state.completion.is_none());
            assert_eq!(Arc::strong_count(&tree), 3);
        }
        proceed.send(false).unwrap();
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 3);
        assert!(controller.try_take_completion().is_none());
        proceed.send(false).unwrap();
        wait_state(&controller, |state| state.completion.is_some());
        controller.submit_latest(request(&tree, 4));
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 4);
        assert!(controller.try_take_completion().is_none());
        proceed.send(false).unwrap();
        wait_state(&controller, |state| state.completion.is_some());
        assert_eq!(controller.try_take_completion().unwrap().generation, 4);
        join(runtime);
        assert_eq!(Arc::strong_count(&tree), 1);
    }

    #[test]
    fn completion_contains_complete_highlights() {
        let tree = tree();
        let (mut controller, runtime) = SearchRuntime::new();
        controller.set_waker(egui::Context::default());
        let request = request(&tree, 1);
        let edited_at = request.edited_at;
        let key = request.key.clone();
        controller.submit_latest(request);
        wait_state(&controller, |state| state.completion.is_some());
        let completion = controller.try_take_completion().unwrap();
        assert_eq!(completion.result.hits, vec![1, 3]);
        assert_eq!(completion.result.total, 2);
        assert_eq!(completion.result.total_size, 60);
        assert_eq!(completion.bits, vec![2, 3, 0, 5, 4]);
        assert_eq!(completion.edited_at, edited_at);
        assert_eq!(completion.key, key);
        assert!(
            completion.timings.queued + completion.timings.search + completion.timings.highlight
                <= edited_at.elapsed()
        );
        assert_eq!(Arc::strong_count(&tree), 1);
        join(runtime);
    }

    #[test]
    fn release_waits_for_active_and_pending_leases() {
        let tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        controller.submit_latest(request(&tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.submit_latest(request(&tree, 2));
        controller.release_for_mutation(fence(1));
        assert!(controller.try_take_control().is_none());
        assert_eq!(Arc::strong_count(&tree), 2);
        controller.resume_after_mutation(fence(1));
        controller.submit_latest(request(&tree, 3));
        assert_eq!(Arc::strong_count(&tree), 2);
        proceed.send(false).unwrap();
        wait_state(&controller, |state| !state.controls.is_empty());
        assert_eq!(
            controller.try_take_control(),
            Some(SearchControlEvent::Released(fence(1)))
        );
        assert_eq!(Arc::strong_count(&tree), 1);
        controller.submit_latest(request(&tree, 4));
        assert_eq!(Arc::strong_count(&tree), 1);
        controller.resume_after_mutation(fence(2));
        controller.submit_latest(request(&tree, 5));
        assert_eq!(Arc::strong_count(&tree), 1);
        controller.resume_after_mutation(fence(1));
        controller.submit_latest(request(&tree, 6));
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 6);
        proceed.send(false).unwrap();
        join(runtime);
        assert_eq!(Arc::strong_count(&tree), 1);
    }

    #[test]
    fn shutdown_wakes_idle_and_cancels_active_worker() {
        let (mut controller, runtime) = SearchRuntime::new();
        controller.shutdown();
        join(runtime);
        let tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        controller.submit_latest(request(&tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.submit_latest(request(&tree, 2));
        controller.shutdown();
        proceed.send(false).unwrap();
        join(runtime);
        assert!(controller.try_take_completion().is_none());
        assert_eq!(Arc::strong_count(&tree), 1);
    }

    #[test]
    fn worker_failure_releases_all_tree_leases() {
        for request_fence_before_failure in [true, false] {
            let tree = tree();
            let (mut controller, runtime, started, proceed) = gated();
            controller.submit_latest(request(&tree, 1));
            started.recv_timeout(TIMEOUT).unwrap();
            controller.submit_latest(request(&tree, 2));
            if request_fence_before_failure {
                controller.release_for_mutation(fence(1));
            }
            proceed.send(true).unwrap();
            wait_state(&controller, |state| {
                state
                    .controls
                    .iter()
                    .any(|event| matches!(event, SearchControlEvent::WorkerFailed(_)))
            });
            assert_eq!(Arc::strong_count(&tree), 1);
            if request_fence_before_failure {
                assert_eq!(
                    controller.try_take_control(),
                    Some(SearchControlEvent::Released(fence(1)))
                );
            }
            assert!(matches!(
                controller.try_take_control(),
                Some(SearchControlEvent::WorkerFailed(_))
            ));
            if !request_fence_before_failure {
                controller.release_for_mutation(fence(1));
                assert_eq!(
                    controller.try_take_control(),
                    Some(SearchControlEvent::Released(fence(1)))
                );
            }
            controller.resume_after_mutation(fence(1));
            controller.submit_latest(request(&tree, 3));
            assert_eq!(Arc::strong_count(&tree), 1);
            assert!(controller.try_take_completion().is_none());
            join(runtime);
        }
    }

    #[test]
    fn controller_drop_only_signals_and_releases_pending() {
        let tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        controller.submit_latest(request(&tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.submit_latest(request(&tree, 2));
        drop(controller);
        assert_eq!(Arc::strong_count(&tree), 2);
        proceed.send(false).unwrap();
        join(runtime);
        assert_eq!(Arc::strong_count(&tree), 1);
    }

    #[test]
    fn failure_and_fence_wait_for_a_displaced_lease_drop() {
        let tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        controller.submit_latest(request(&tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.submit_latest(request(&tree, 2));
        // Pause a controller between taking a displaced pending request and
        // dropping it outside the lock, then let worker failure race that drop.
        let displaced = take_slots(&mut controller.shared.lock());
        controller.release_for_mutation(fence(1));
        proceed.send(true).unwrap();
        wait_state(&controller, |state| state.failed);
        assert_eq!(Arc::strong_count(&tree), 2);
        assert!(controller.try_take_control().is_none());
        controller.shared.retire(displaced);
        assert_eq!(Arc::strong_count(&tree), 1);
        assert_eq!(
            controller.try_take_control(),
            Some(SearchControlEvent::Released(fence(1)))
        );
        assert!(matches!(
            controller.try_take_control(),
            Some(SearchControlEvent::WorkerFailed(_))
        ));
        join(runtime);
    }

    #[test]
    fn invalidate_discards_active_pending_and_completed_results() {
        let tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        controller.submit_latest(request(&tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.submit_latest(request(&tree, 2));
        controller.invalidate();
        proceed.send(false).unwrap();
        wait_state(&controller, |state| !state.active);
        assert_eq!(Arc::strong_count(&tree), 1);
        assert!(controller.try_take_completion().is_none());
        controller.submit_latest(request(&tree, 3));
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 3);
        proceed.send(false).unwrap();
        wait_state(&controller, |state| state.completion.is_some());
        controller.invalidate();
        assert!(controller.try_take_completion().is_none());
        join(runtime);
    }

    #[test]
    fn highlights_cancel_during_ancestor_and_descendant_passes() {
        use std::sync::atomic::AtomicUsize;
        let mut tree = Tree::default();
        for id in 0..1024 {
            tree.nodes.push(Node {
                parent: if id == 0 { NO_NODE } else { id - 1 },
                alloc: 1,
                logical: 1,
                name_off: 0,
                first_child: id + 1,
                child_count: u32::from(id < 1023),
                files: 0,
                mtime: 0,
                name_len: 0,
                kind: Kind::Dir,
                flags: 0,
            });
        }
        for hits in [&[1023][..], &[0][..]] {
            let calls = AtomicUsize::new(0);
            assert!(matches!(
                build_search_bits_with_cancel(&tree, hits, &|| calls
                    .fetch_add(1, Ordering::Relaxed)
                    >= 2),
                Err(SearchCancelled)
            ));
        }
    }
    #[cfg(feature = "perf")]
    #[test]
    fn cancellation_trace_returns_actual_active_and_displaced_pending_leases() {
        let active_tree = tree();
        let displaced_tree = tree();
        let pending_tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        let weak = [
            Arc::downgrade(&active_tree),
            Arc::downgrade(&displaced_tree),
            Arc::downgrade(&pending_tree),
        ];
        let shared = Arc::downgrade(&controller.shared);
        let (tx, released) = mpsc::channel();
        controller.set_release_observer(Arc::new(move |release| {
            assert!(
                shared.upgrade().unwrap().state.try_lock().is_ok(),
                "observer ran under mailbox lock"
            );
            let leases = weak[(release.generation - 1) as usize].strong_count();
            tx.send((release, leases)).unwrap();
        }));
        controller.submit_latest(request(&active_tree, 1));
        assert_eq!(started.recv_timeout(TIMEOUT).unwrap(), 1);
        controller.submit_latest(request(&displaced_tree, 2));
        let first_cancel = controller
            .shared
            .lock()
            .active_stamp
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .0
            .unwrap();
        controller.submit_latest(request(&pending_tree, 3));
        let (displaced, leases) = released.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(displaced.generation, 2);
        assert_eq!(leases, 1, "displaced request still owns a tree lease");
        controller.invalidate();
        let (pending, leases) = released.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(pending.generation, 3);
        assert_eq!(leases, 1);
        controller.invalidate();
        assert!(
            released.try_recv().is_err(),
            "active release emitted before barrier"
        );
        proceed.send(false).unwrap();
        let (active, leases) = released.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(active.generation, 1);
        assert_eq!(
            active.cancelled_at, first_cancel,
            "repeated invalidation reset cancellation time"
        );
        assert_eq!(leases, 1, "active request still owns a tree lease");
        for release in [active, displaced, pending] {
            assert!(release.cancelled_at <= release.released_at);
            assert!(release.released_at <= Instant::now());
            assert_eq!(release.doc_id, 1);
        }
        join(runtime);
        assert!(released.try_recv().is_err());
    }

    #[cfg(feature = "perf")]
    #[test]
    fn cancellation_trace_returns_leases_during_worker_unwind() {
        let active_tree = tree();
        let pending_tree = tree();
        let (mut controller, runtime, started, proceed) = gated();
        let weak = [Arc::downgrade(&active_tree), Arc::downgrade(&pending_tree)];
        let (tx, released) = mpsc::channel();
        controller.set_release_observer(Arc::new(move |release| {
            tx.send((
                release,
                weak[(release.generation - 1) as usize].strong_count(),
            ))
            .unwrap();
        }));
        controller.submit_latest(request(&active_tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        controller.submit_latest(request(&pending_tree, 2));
        proceed.send(true).unwrap();
        for generation in [1, 2] {
            let (release, leases) = released.recv_timeout(TIMEOUT).unwrap();
            assert_eq!(release.generation, generation);
            assert_eq!(leases, 1, "unwind/failure metric preceded tree release");
            assert!(release.cancelled_at <= release.released_at);
        }
        wait_state(&controller, |state| {
            state.failed && !state.active && state.retiring == 0
        });
        assert!(matches!(
            controller.try_take_control(),
            Some(SearchControlEvent::WorkerFailed(_))
        ));
        join(runtime);
        assert!(released.try_recv().is_err());
    }
    #[cfg(feature = "perf")]
    fn release_at_publication(operation: &str) {
        let replacement_tree = tree();
        let tree = tree();
        let (started_tx, started) = mpsc::channel();
        let (proceed, proceed_rx) = mpsc::channel();
        let proceed_rx = Mutex::new(proceed_rx);
        let (mut controller, runtime) = SearchRuntime::with_hook(Box::new(move |generation| {
            if generation == 1 {
                started_tx.send(()).unwrap();
                proceed_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
            }
        }));
        let (metric_tx, metrics) = mpsc::channel();
        let weak = Arc::downgrade(&tree);
        controller.set_release_observer(Arc::new(move |release| {
            metric_tx.send((release, weak.strong_count())).unwrap();
        }));
        controller.submit_latest(request(&tree, 1));
        started.recv_timeout(TIMEOUT).unwrap();
        let (dropped_tx, dropped) = mpsc::channel();
        let dropped = Mutex::new(dropped);
        let first = AtomicBool::new(true);
        *controller.shared.trace_hook.lock().unwrap() =
            Some(Arc::new(move |boundary| match boundary {
                TraceBoundary::Published if first.swap(false, Ordering::SeqCst) => {
                    // Controller still owns the mailbox mutex. The active request
                    // observes cancellation and drops its lease before it can continue.
                    proceed.send(false).unwrap();
                    dropped.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                }
                TraceBoundary::Released => {
                    dropped_tx.send(()).unwrap();
                }
                _ => {}
            }));
        match operation {
            "invalidate" => controller.invalidate(),
            "submit" => controller.submit_latest(request(&replacement_tree, 2)),
            "mutation" => controller.release_for_mutation(fence(1)),
            "shutdown" => controller.shutdown(),
            _ => unreachable!(),
        }
        // The released-boundary acknowledgement guarantees the guard has already
        // completed; no scheduling delay can explain a missing metric here.
        let metric = metrics.try_recv();
        // Disable the boundary hook before assertion/teardown.
        *controller.shared.trace_hook.lock().unwrap() = None;
        controller.shutdown();
        join(runtime);
        let (release, leases) =
            metric.expect("cancellation published before its timestamp: release metric lost");
        assert_eq!(release.generation, 1);
        assert_eq!(leases, 1, "metric preceded the active Tree Arc drop");
        assert!(release.cancelled_at <= release.released_at);
    }

    #[cfg(feature = "perf")]
    #[test]
    fn invalidation_stamps_before_cancellation_publication() {
        release_at_publication("invalidate");
    }
    #[cfg(feature = "perf")]
    #[test]
    fn replacement_stamps_before_cancellation_publication() {
        release_at_publication("submit");
    }
    #[cfg(feature = "perf")]
    #[test]
    fn mutation_stamps_before_cancellation_publication() {
        release_at_publication("mutation");
    }
    #[cfg(feature = "perf")]
    #[test]
    fn shutdown_stamps_before_cancellation_publication() {
        release_at_publication("shutdown");
    }
}
