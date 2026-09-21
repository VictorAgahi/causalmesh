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
/// The watchdog only activates after the first client has connected
/// (to avoid immediate shutdown during startup latency).
pub fn spawn_idle_watchdog(
    counter: ClientCounter,
    token: CancellationToken,
    idle_timeout: Duration,
    poll_interval: Duration,
) {
    tokio::spawn(async move {
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
            Duration::from_millis(80), // idle timeout
            Duration::from_millis(20), // poll interval
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
    async fn test_idle_watchdog_does_not_fire_if_never_had_client() {
        let counter = ClientCounter::new();
        let token = CancellationToken::new();

        // Never had a client → watchdog must NOT fire
        spawn_idle_watchdog(
            counter,
            token.clone(),
            Duration::from_millis(30),
            Duration::from_millis(10),
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !token.is_cancelled(),
            "Watchdog must not fire without ever having a client"
        );
    }
}
