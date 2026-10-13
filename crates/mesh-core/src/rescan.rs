use rayon::ThreadPool;
use std::sync::Arc;

/// How far below normal a rescan pool's threads run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanPriority {
    /// Watcher-triggered rescans, which nobody waits for: `QOS_CLASS_BACKGROUND`
    /// (macOS), `nice(10)` (Linux), below-normal plus background mode (Windows).
    Background,
    /// Config-driven reloads, which a request is waiting for: `QOS_CLASS_UTILITY`
    /// (macOS), `nice(5)` (Linux), below-normal (Windows). Still behind the editor,
    /// but not starved the way the background class is on a busy machine.
    Utility,
}

/// Applies OS-level priority/QoS to the calling thread. Invoked once per rescan
/// worker thread at startup so rescans stay out of the editor's way (CLAUDE.md
/// Commandment 7). No-op on platforms without a priority primitive.
fn apply_priority(priority: RescanPriority) {
    // macOS: Quality of Service class of the calling thread.
    #[cfg(target_os = "macos")]
    unsafe {
        let class = match priority {
            RescanPriority::Background => libc::qos_class_t::QOS_CLASS_BACKGROUND,
            RescanPriority::Utility => libc::qos_class_t::QOS_CLASS_UTILITY,
        };
        libc::pthread_set_qos_class_self_np(class, 0);
    }

    // Linux: nice level of the calling thread (10, or 5 for utility).
    #[cfg(target_os = "linux")]
    unsafe {
        let nice = match priority {
            RescanPriority::Background => 10,
            RescanPriority::Utility => 5,
        };
        libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }

    // Windows: Lower scheduling priority one notch below normal, then (background
    // only) also drop I/O and memory priority via per-thread background mode
    // (Vista+). THREAD_MODE_BACKGROUND_BEGIN alone does not change what
    // GetThreadPriority() reports, so THREAD_PRIORITY_BELOW_NORMAL is set
    // first to guarantee the thread runs below normal CPU priority too.
    #[cfg(target_os = "windows")]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
            THREAD_PRIORITY_BELOW_NORMAL,
        };
        let thread = GetCurrentThread();
        SetThreadPriority(thread, THREAD_PRIORITY_BELOW_NORMAL);
        if priority == RescanPriority::Background {
            SetThreadPriority(thread, THREAD_MODE_BACKGROUND_BEGIN);
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = priority;
}

/// BackgroundRescanEngine providing dedicated thread pool with OS-level QoS throttling
/// guaranteeing zero IDE keystroke stuttering (< 150ms invariant) per CLAUDE.md Commandment 7.
pub struct BackgroundRescanEngine {
    thread_pool: Arc<ThreadPool>,
}

impl BackgroundRescanEngine {
    /// The watcher-triggered pool, at [`RescanPriority::Background`].
    pub fn new() -> Result<Self, rayon::ThreadPoolBuildError> {
        Self::with_priority(RescanPriority::Background)
    }

    /// A pool whose threads run at `priority`.
    pub fn with_priority(priority: RescanPriority) -> Result<Self, rayon::ThreadPoolBuildError> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus::get().min(4))
            .thread_name(move |idx| match priority {
                RescanPriority::Background => format!("mesh-rescan-{idx}"),
                RescanPriority::Utility => format!("mesh-reload-{idx}"),
            })
            .start_handler(move |_thread_id| apply_priority(priority))
            .build()?;

        Ok(Self {
            thread_pool: Arc::new(pool),
        })
    }

    /// Spawns a CPU-intensive indexing or parsing job onto the isolated QoS-throttled thread pool.
    pub fn spawn<F, R>(&self, task: F) -> tokio::sync::oneshot::Receiver<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.thread_pool.spawn(move || {
            let res = task();
            let _ = tx.send(res);
        });
        rx
    }

    /// Runs `op` inside the QoS-throttled pool so any `par_iter` it spawns executes on
    /// background-priority threads instead of Rayon's global (normal-priority) pool.
    #[inline]
    pub fn install<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R {
        self.thread_pool.install(op)
    }

    #[inline]
    pub fn thread_count(&self) -> usize {
        self.thread_pool.current_num_threads()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_background_rescan_engine_exec() {
        let engine = BackgroundRescanEngine::new().expect("build background rescan engine");
        let rx = engine.spawn(|| {
            let mut sum = 0u64;
            for i in 0..10_000 {
                sum += i;
            }
            sum
        });

        let result = rx.await.expect("receive result");
        assert_eq!(result, 49995000);
    }

    /// Cross-platform smoke test: the priority-setting call must never panic,
    /// on any target (including platforms with no background priority
    /// primitive, where it is a no-op).
    #[test]
    fn test_apply_background_priority_does_not_panic() {
        apply_priority(RescanPriority::Background);
    }

    /// The OS politeness invariant (CLAUDE.md commandment 7), for both pools: the
    /// watcher pool's threads really run in the background class and the
    /// config-reload pool's in the intermediate (utility) one, which is strictly
    /// above it. Checked from inside a worker thread of each pool.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn both_pools_run_at_their_own_priority_below_normal() {
        fn effective(engine: &BackgroundRescanEngine) -> i64 {
            engine.install(|| {
                #[cfg(target_os = "macos")]
                unsafe {
                    let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
                    let mut relative = 0;
                    libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative);
                    class as i64
                }
                #[cfg(target_os = "linux")]
                unsafe {
                    i64::from(libc::getpriority(libc::PRIO_PROCESS, 0))
                }
            })
        }
        let background = BackgroundRescanEngine::new().expect("background pool");
        let utility =
            BackgroundRescanEngine::with_priority(RescanPriority::Utility).expect("utility pool");
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                effective(&background),
                libc::qos_class_t::QOS_CLASS_BACKGROUND as i64
            );
            assert_eq!(
                effective(&utility),
                libc::qos_class_t::QOS_CLASS_UTILITY as i64
            );
            // Utility is above background, and both are below the default class.
            assert!(
                (libc::qos_class_t::QOS_CLASS_UTILITY as i64)
                    > (libc::qos_class_t::QOS_CLASS_BACKGROUND as i64)
            );
            assert!(
                (libc::qos_class_t::QOS_CLASS_UTILITY as i64)
                    < (libc::qos_class_t::QOS_CLASS_DEFAULT as i64)
            );
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(effective(&background), 10);
            assert_eq!(effective(&utility), 5);
        }
    }

    /// Windows-only: verifies the rescan pool's priority-setting call actually
    /// drops the calling thread's scheduling priority below normal.
    /// Gated to `target_os = "windows"` since it exercises Win32 thread APIs directly.
    #[cfg(target_os = "windows")]
    #[test]
    fn test_windows_rescan_thread_runs_below_normal_priority() {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_NORMAL,
        };

        apply_priority(RescanPriority::Background);

        let priority = unsafe { GetThreadPriority(GetCurrentThread()) };
        assert!(
            priority < THREAD_PRIORITY_NORMAL as i32,
            "expected rescan thread priority ({priority}) below normal ({})",
            THREAD_PRIORITY_NORMAL as i32
        );
    }
}
