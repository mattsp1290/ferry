//! Health endpoints.
//!
//! `GET /healthz` is liveness: 200 while the scheduler loop has ticked within
//! the last 60 s, 503 otherwise (including before the first tick). Only the
//! scheduler loop calls `HealthState::beat`, so a wedged scheduler fails
//! liveness even while other tasks keep running.
//!
//! `GET /readyz` is an operator signal: 200 once startup checks passed.
//!
//! There is no other route and no metrics endpoint.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// A heartbeat older than this fails liveness.
pub const LIVENESS_WINDOW: Duration = Duration::from_secs(60);

/// Shared, cheap-to-clone health state.
#[derive(Debug, Clone, Default)]
pub struct HealthState {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    last_beat: Mutex<Option<Instant>>,
    ready: AtomicBool,
}

impl HealthState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a scheduler tick. Only the scheduler loop calls this.
    pub fn beat(&self) {
        *self
            .inner
            .last_beat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }

    /// Time since the last `beat`, or `None` before the first one.
    pub fn heartbeat_age(&self) -> Option<Duration> {
        let last = *self
            .inner
            .last_beat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.map(|at| Instant::now().saturating_duration_since(at))
    }

    /// True when a beat is younger than `LIVENESS_WINDOW`. No beat yet: false.
    pub fn is_live(&self) -> bool {
        self.heartbeat_age()
            .is_some_and(|age| age < LIVENESS_WINDOW)
    }

    pub fn set_ready(&self, ready: bool) {
        self.inner.ready.store(ready, Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }
}

/// The health router: `GET /healthz` and `GET /readyz`, nothing else.
pub fn router(state: HealthState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

async fn healthz(State(state): State<HealthState>) -> (StatusCode, &'static str) {
    if state.is_live() {
        (StatusCode::OK, "ok\n")
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "scheduler heartbeat stale\n",
        )
    }
}

async fn readyz(State(state): State<HealthState>) -> (StatusCode, &'static str) {
    if state.is_ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}

/// Serves the health router on `listener` until `shutdown` is cancelled, then
/// drains in-flight requests.
pub async fn serve(
    listener: TcpListener,
    state: HealthState,
    shutdown: CancellationToken,
) -> io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
}
