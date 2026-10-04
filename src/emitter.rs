//! Periodic gauge emitter. It reads scheduler state and never writes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::health::HealthState;
use crate::scheduler::SharedStatus;
use crate::telemetry::Metrics;

/// Settings of the periodic metrics emitter.
#[derive(Debug, Clone)]
pub struct EmitterConfig {
    pub interval: Duration,
    /// Directory measured for `ferry.cache.bytes`. `None` disables the gauge.
    pub cache_dir: Option<PathBuf>,
    /// The cache is measured at most this often: walking it is not free.
    pub cache_scan_interval: Duration,
}

impl EmitterConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval: Duration::from_secs(30),
            cache_dir: Some(config.sync.cache_dir.clone()),
            cache_scan_interval: Duration::from_secs(15 * 60),
        }
    }
}

/// Emits the gauges every `interval` until `shutdown` is cancelled.
///
/// This task only reads scheduler state. It never writes the scheduler
/// heartbeat, and it emits `ferry.heartbeat` only while that heartbeat is
/// fresh, so a wedged scheduler shows up as missing data in Datadog.
pub async fn run_emitter(
    metrics: Arc<dyn Metrics>,
    health: HealthState,
    shared: SharedStatus,
    config: EmitterConfig,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_cache_scan: Option<Instant> = None;
    let mut cache_scan: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }

        let now = Instant::now();
        metrics.repos_configured(shared.entries().len());
        for (entry, status) in shared.entries().iter().zip(shared.snapshot()) {
            metrics.repo_state(
                entry,
                shared.last_success_age(&status, now),
                status.consecutive_failures,
            );
        }
        if health.is_live() {
            metrics.heartbeat();
        }

        let scan_due = last_cache_scan
            .is_none_or(|last| now.saturating_duration_since(last) >= config.cache_scan_interval);
        let scan_running = cache_scan.as_ref().is_some_and(|scan| !scan.is_finished());
        if let Some(cache_dir) = config
            .cache_dir
            .as_ref()
            .filter(|_| scan_due && !scan_running)
        {
            last_cache_scan = Some(now);
            // Walking a large LFS cache can take longer than the emit
            // interval. It runs on its own task so that it never delays the
            // heartbeat, which a monitor alerts on.
            cache_scan = Some(tokio::spawn(scan_cache(
                Arc::clone(&metrics),
                cache_dir.clone(),
            )));
        }
    }

    if let Some(scan) = cache_scan {
        scan.abort();
    }
}

async fn scan_cache(metrics: Arc<dyn Metrics>, cache_dir: PathBuf) {
    match tokio::task::spawn_blocking(move || directory_bytes(&cache_dir)).await {
        Ok(bytes) => metrics.cache_bytes(bytes),
        Err(error) => tracing::warn!(error = %error, "cache size scan failed"),
    }
}

/// Sum of the file sizes under `path`. Unreadable entries count as zero:
/// git rewrites the cache while this walks it.
fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_bytes_sums_nested_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("top"), [0u8; 10]).expect("write");
        std::fs::write(dir.path().join("a/b/nested"), [0u8; 32]).expect("write");
        assert_eq!(directory_bytes(dir.path()), 42);
        assert_eq!(directory_bytes(&dir.path().join("absent")), 0);
    }
}
