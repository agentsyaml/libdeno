use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use deno_core::v8;
use deno_core::V8CrossThreadTaskSpawner;
use deno_runtime::web_worker::WebWorkerInternalHandle;
use tokio::sync::watch;

static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
static REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn active_worker_count() -> usize {
    ACTIVE_WORKERS.load(Ordering::SeqCst)
}

#[derive(Clone, Default)]
pub(crate) struct WorkerLifecycleRegistry(Arc<Inner>);

#[derive(Default)]
struct State {
    closing: bool,
    workers: HashMap<u32, Record>,
}

#[derive(Clone)]
struct Control {
    isolate: v8::IsolateHandle,
    tasks: V8CrossThreadTaskSpawner,
}

struct Record {
    registration_id: u64,
    parent_id: u32,
    control: Option<Control>,
}

struct Inner {
    state: Mutex<State>,
    count: watch::Sender<usize>,
    closing: watch::Sender<bool>,
}

impl Default for Inner {
    fn default() -> Self {
        let (count, _receiver) = watch::channel(0);
        let (closing, _receiver) = watch::channel(false);
        Self {
            state: Mutex::new(State::default()),
            count,
            closing,
        }
    }
}

impl WorkerLifecycleRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn closing_signal(&self) -> watch::Receiver<bool> {
        self.0.closing.subscribe()
    }

    pub(crate) fn reserve(&self, worker_id: u32, parent_id: u32) -> Option<WorkerRegistration> {
        let registration_id = REGISTRATION_ID.fetch_add(1, Ordering::Relaxed);
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.closing {
            return None;
        }
        let previous = state.workers.insert(
            worker_id,
            Record {
                registration_id,
                parent_id,
                control: None,
            },
        );
        if previous.is_none() {
            ACTIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
        }
        self.0.count.send_replace(state.workers.len());
        Some(WorkerRegistration {
            registry: self.clone(),
            worker_id,
            registration_id,
        })
    }

    pub(crate) fn attach(
        &self,
        worker_id: u32,
        registration_id: u64,
        isolate: v8::IsolateHandle,
        tasks: V8CrossThreadTaskSpawner,
    ) {
        let control = Control { isolate, tasks };
        let stop = {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let closing = state.closing;
            if let Some(record) = state
                .workers
                .get_mut(&worker_id)
                .filter(|record| record.registration_id == registration_id)
            {
                record.control = Some(control.clone());
            } else {
                return;
            }
            closing
        };
        if stop {
            request_stop(control);
        }
    }

    #[cfg(test)]
    fn parent_id(&self, worker_id: u32) -> Option<u32> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .workers
            .get(&worker_id)
            .map(|record| record.parent_id)
    }

    pub(crate) fn close(&self) {
        let mut controls = {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.closing = true;
            self.0.closing.send_replace(true);
            state
                .workers
                .values()
                .filter_map(|record| {
                    record
                        .control
                        .clone()
                        .map(|control| (record.parent_id, control))
                })
                .collect::<Vec<_>>()
        };
        controls.sort_by_key(|(parent_id, _)| *parent_id);
        for (_, control) in controls {
            request_stop(control);
        }
    }

    pub(crate) async fn close_and_wait(&self) -> bool {
        self.close();
        let mut count = self.0.count.subscribe();
        loop {
            if *count.borrow() == 0 {
                return true;
            }
            if count.changed().await.is_err() {
                return false;
            }
        }
    }

    fn unregister(&self, worker_id: u32, registration_id: u64) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state
            .workers
            .get(&worker_id)
            .is_some_and(|record| record.registration_id == registration_id)
        {
            state.workers.remove(&worker_id);
            ACTIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
            self.0.count.send_replace(state.workers.len());
        }
    }
}

pub(crate) struct WorkerRegistration {
    registry: WorkerLifecycleRegistry,
    worker_id: u32,
    registration_id: u64,
}

impl WorkerRegistration {
    pub(crate) fn registration_id(&self) -> u64 {
        self.registration_id
    }
}

impl Drop for WorkerRegistration {
    fn drop(&mut self) {
        self.registry
            .unregister(self.worker_id, self.registration_id);
    }
}

pub(crate) struct WorkerLifecycleOwner(WorkerLifecycleRegistry);

impl WorkerLifecycleOwner {
    pub(crate) fn new(registry: WorkerLifecycleRegistry) -> Self {
        Self(registry)
    }
}

impl Drop for WorkerLifecycleOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn request_stop(control: Control) {
    control.isolate.terminate_execution();
    control.tasks.spawn(move |scope| {
        let op_state = deno_core::JsRuntime::op_state_from(scope);
        let mut handle = {
            op_state
                .borrow()
                .borrow::<WebWorkerInternalHandle>()
                .clone()
        };
        handle.terminate();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_isolated(name: &str, test: impl FnOnce()) {
        const CHILD: &str = "LIBDENO_WORKER_LIFECYCLE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            test();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("could not launch isolated worker lifecycle test");
        assert!(
            output.status.success(),
            "isolated test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "isolated test did not run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn per_run_registration_tracks_parent_and_releases_worker_count() {
        run_isolated(
            "worker_lifecycle::tests::per_run_registration_tracks_parent_and_releases_worker_count",
            || {
                let before = active_worker_count();
                let registry = WorkerLifecycleRegistry::new();
                let registration = registry.reserve(u32::MAX, 7).unwrap();
                assert_eq!(registry.parent_id(u32::MAX), Some(7));
                assert_eq!(active_worker_count(), before + 1);
                drop(registration);
                assert_eq!(active_worker_count(), before);
            },
        );
    }

    #[test]
    fn close_waits_for_bootstrapping_registration_to_drop() {
        run_isolated(
            "worker_lifecycle::tests::close_waits_for_bootstrapping_registration_to_drop",
            || {
                let registry = WorkerLifecycleRegistry::new();
                let registration = registry.reserve(u32::MAX, 0).unwrap();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let registry_for_close = registry.clone();
                let waiting =
                    runtime.spawn(async move { registry_for_close.close_and_wait().await });
                drop(registration);
                assert!(runtime.block_on(waiting).unwrap());
            },
        );
    }

    #[test]
    fn registration_after_close_is_rejected() {
        run_isolated(
            "worker_lifecycle::tests::registration_after_close_is_rejected",
            || {
                let before = active_worker_count();
                let registry = WorkerLifecycleRegistry::new();
                registry.close();
                assert!(registry.reserve(u32::MAX, 7).is_none());
                assert_eq!(active_worker_count(), before);
            },
        );
    }

    #[test]
    fn shutdown_signal_is_visible_to_late_subscribers() {
        let registry = WorkerLifecycleRegistry::new();
        registry.close();
        let mut closing = registry.closing_signal();
        assert!(*closing.borrow_and_update());
    }

    #[test]
    fn close_and_bootstrap_reservation_race_is_accounted() {
        run_isolated(
            "worker_lifecycle::tests::close_and_bootstrap_reservation_race_is_accounted",
            || {
                let before = active_worker_count();
                let registry = WorkerLifecycleRegistry::new();
                let barrier = Arc::new(std::sync::Barrier::new(3));
                let close_registry = registry.clone();
                let close_barrier = barrier.clone();
                let closer = std::thread::spawn(move || {
                    close_barrier.wait();
                    close_registry.close();
                });
                let reserve_registry = registry.clone();
                let reserve_barrier = barrier.clone();
                let reserver = std::thread::spawn(move || {
                    reserve_barrier.wait();
                    reserve_registry.reserve(u32::MAX - 1, 7)
                });
                barrier.wait();
                closer.join().unwrap();
                if let Some(registration) = reserver.join().unwrap() {
                    drop(registration);
                }
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                assert!(runtime.block_on(registry.close_and_wait()));
                assert_eq!(active_worker_count(), before);
            },
        );
    }
}
