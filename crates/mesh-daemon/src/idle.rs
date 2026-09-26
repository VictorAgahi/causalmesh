//! Idle watchdog for meshd: triggers graceful shutdown when no clients
//! have been connected for longer than the configured idle timeout.
//!
//! This is the key resource-conservation mechanism: a daemon spawned
//! ad-hoc by `mesh-mcp run` (no launchd/systemd) self-terminates once
//! all IDE sessions are closed, freeing RAM and file descriptors.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Shared active-client counter. Incremented on accept, decremented on disconnect.
#[derive(Clone)]
pub struct ClientCounter(Arc<AtomicUsize>);

impl Default for ClientCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientCounter {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// Spawns the idle watchdog Tokio task.
/// Polls every `poll_interval` and cancels `token` if no clients have
/// been connected for `idle_timeout` duration.
///
/// `startup_grace` is a *second*, independent deadline (P2 step 3.3): if no client has
/// connected at all within `startup_grace` of this call, the daemon shuts down too — this is
/// the orphaned-daemon guard, distinct from `idle_timeout`'s "had clients, now idle" case.
/// `mesh-mcp run`'s `ensure_daemon_running` auto-spawns `meshd` ad-hoc (no launchd/systemd
/// supervising it); if that spawn ever races or fails after the process itself started (the
/// parent CLI exits, a config error prevents any tool ever calling in, the workspace path was
/// wrong so no client ever finds this daemon's socket), the pre-3.3 watchdog's "only activates
/// after the first client" rule meant such a daemon lived forever, unkillable except by PID —
/// exactly the kind of zombie this mechanism exists to prevent for the *other* case.
pub fn spawn_idle_watchdog(
    counter: ClientCounter,
    token: CancellationToken,
    idle_timeout: Duration,
    poll_interval: Duration,
    startup_grace: Duration,
) {
    tokio::spawn(async move {
        let started_at = std::time::Instant::now();
        let mut ever_had_client = false;
        let mut idle_since: Option<std::time::Instant> = None;

        loop {
            tokio::time::sleep(poll_interval).await;

            if token.is_cancelled() {
                break;
            }

            let active = counter.count();

            if active > 0 {
                ever_had_client = true;
                idle_since = None;
            } else if ever_had_client {
                let since = idle_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= idle_timeout {
                    tracing::info!(
                        target: "meshd::idle",
                        "No clients for {:?}. Initiating graceful idle shutdown.",
                        idle_timeout
                    );
                    token.cancel();
                    break;
                }
            } else if started_at.elapsed() >= startup_grace {
                tracing::warn!(
                    target: "meshd::idle",
                    "No client connected within {:?} of startup. Initiating graceful shutdown \
                     (orphaned-daemon guard).",
                    startup_grace
                );
                token.cancel();
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_counter_increment_decrement() {
        let c = ClientCounter::new();
        assert_eq!(c.count(), 0);
        c.increment();
        c.increment();
        assert_eq!(c.count(), 2);
        c.decrement();
        assert_eq!(c.count(), 1);
    }

    #[tokio::test]
    async fn test_idle_watchdog_fires_after_timeout() {
        let counter = ClientCounter::new();
        let token = CancellationToken::new();

        // Simulate: client connected
        counter.increment();

        spawn_idle_watchdog(
            counter.clone(),
            token.clone(),
            Duration::from_millis(80),  // idle timeout
            Duration::from_millis(20),  // poll interval
            Duration::from_secs(3_600), // startup grace: irrelevant here, kept well out of the way
        );

        // Give watchdog at least one poll cycle to observe the active client
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Client disconnects
        counter.decrement();

        // Wait for idle timeout + several poll cycles
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            token.is_cancelled(),
            "Watchdog should have cancelled the token"
        );
    }

    #[tokio::test]
    async fn test_idle_watchdog_does_not_fire_before_startup_grace_elapses() {
        let counter = ClientCounter::new();
        let token = CancellationToken::new();

        // Never had a client, but well within the (generous) startup grace → must NOT fire yet.
        spawn_idle_watchdog(
            counter,
            token.clone(),
            Duration::from_millis(30),
            Duration::from_millis(10),
            Duration::from_secs(3_600),
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !token.is_cancelled(),
            "Watchdog must not fire before startup_grace elapses, even with no client ever"
        );
    }

    /// P2 step 3.3's actual new behavior: a daemon that never gets a client at all (auto-spawn
    /// raced or failed after the process started, wrong workspace path, ...) must eventually
    /// shut itself down rather than living forever as an unreachable zombie — the gap the
    /// pre-3.3 "only activates after the first client" rule left open.
    #[tokio::test]
    async fn test_idle_watchdog_fires_after_startup_grace_with_no_client_ever() {
        let counter = ClientCounter::new();
        let token = CancellationToken::new();

        spawn_idle_watchdog(
            counter,
            token.clone(),
            Duration::from_secs(3_600), // idle timeout: irrelevant, never had a client
            Duration::from_millis(10),
            Duration::from_millis(50), // startup grace
        );

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            token.is_cancelled(),
            "Watchdog must cancel the token once startup_grace elapses with no client ever \
             connected"
        );
    }

    /// A client connecting *after* startup but before `startup_grace` elapses must disarm the
    /// startup-grace deadline entirely — it's the "orphaned, nobody will ever connect" case
    /// this guards against, not a general "shut down anything idle at startup" policy.
    #[tokio::test]
    async fn test_startup_grace_does_not_fire_once_a_client_has_connected() {
        let counter = ClientCounter::new();
        let token = CancellationToken::new();

        spawn_idle_watchdog(
            counter.clone(),
            token.clone(),
            Duration::from_secs(3_600), // idle timeout: never goes idle again in this test
            Duration::from_millis(10),
            Duration::from_millis(50), // startup grace
        );

        // Client connects before the startup grace would otherwise fire.
        tokio::time::sleep(Duration::from_millis(20)).await;
        counter.increment();

        // Wait well past when the startup grace alone would have fired.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !token.is_cancelled(),
            "A client connecting within the startup grace must disarm it, not just delay it"
        );
    }
}
