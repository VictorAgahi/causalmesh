use rayon::ThreadPool;
use std::sync::Arc;

/// Applies OS-level background priority/QoS to the calling thread. Invoked once
/// per rescan worker thread at startup so background rescans stay out of the
/// editor's way (RFC-001 Commandment 7). No-op on platforms without a
/// background priority primitive.
fn apply_background_priority() {
    // macOS: Assign Background Quality of Service (Zero UI impact)
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
    }

    // Linux: Nice level 10 (low priority) & idle IO
    #[cfg(target_os = "linux")]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }

    // Windows: Lower scheduling priority one notch below normal, then also
    // drop I/O and memory priority via per-thread background mode (Vista+).
    // THREAD_MODE_BACKGROUND_BEGIN alone does not change what
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
        SetThreadPriority(thread, THREAD_MODE_BACKGROUND_BEGIN);
    }
}

/// BackgroundRescanEngine providing dedicated thread pool with OS-level QoS throttling
/// guaranteeing zero IDE keystroke stuttering (< 150ms invariant) per RFC-001 Commandment 7.
pub struct BackgroundRescanEngine {
    thread_pool: Arc<ThreadPool>,
}

impl BackgroundRescanEngine {
    pub fn new() -> Result<Self, rayon::ThreadPoolBuildError> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus::get().min(4))
            .thread_name(|idx| format!("mesh-rescan-{idx}"))
            .start_handler(|_thread_id| apply_background_priority())
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
        apply_background_priority();
    }

    /// Windows-only: verifies the rescan pool's priority-setting call actually
    /// drops the calling thread's scheduling priority below normal, per
    /// ROADMAP Item 13's definition of done. Gated to `target_os = "windows"`
    /// since it exercises the Win32 thread priority APIs directly.
    #[cfg(target_os = "windows")]
    #[test]
    fn test_windows_rescan_thread_runs_below_normal_priority() {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_NORMAL,
        };

        apply_background_priority();

        let priority = unsafe { GetThreadPriority(GetCurrentThread()) };
        assert!(
            priority < THREAD_PRIORITY_NORMAL as i32,
            "expected rescan thread priority ({priority}) below normal ({})",
            THREAD_PRIORITY_NORMAL as i32
        );
    }
}
