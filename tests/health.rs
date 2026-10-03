//! The `ferry::health` router and server: liveness window under a paused
//! clock, readiness, routing, and serving over TCP.
//!
//! The tests live in `mod health` so their names stay `health::...`.

mod health {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use ferry::health::{HealthState, LIVENESS_WINDOW, router};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    async fn status(state: &HealthState, method: &str, path: &str) -> StatusCode {
        router(state.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test(start_paused = true)]
    async fn healthz_follows_the_heartbeat_under_a_paused_clock() {
        let state = HealthState::new();
        assert_eq!(state.heartbeat_age(), None);
        assert!(!state.is_live());
        assert_eq!(
            status(&state, "GET", "/healthz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );

        state.beat();
        assert_eq!(status(&state, "GET", "/healthz").await, StatusCode::OK);

        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(status(&state, "GET", "/healthz").await, StatusCode::OK);
        assert_eq!(state.heartbeat_age(), Some(Duration::from_secs(59)));

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(state.heartbeat_age().unwrap() > LIVENESS_WINDOW);
        assert_eq!(
            status(&state, "GET", "/healthz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );

        state.beat();
        assert_eq!(status(&state, "GET", "/healthz").await, StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn readyz_is_503_until_ready_and_clones_share_state() {
        let state = HealthState::new();
        assert_eq!(
            status(&state, "GET", "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        state.clone().set_ready(true);
        assert!(state.is_ready());
        assert_eq!(status(&state, "GET", "/readyz").await, StatusCode::OK);
        state.set_ready(false);
        assert_eq!(
            status(&state, "GET", "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_two_routes_exist() {
        let state = HealthState::new();
        state.beat();
        state.set_ready(true);
        for path in ["/", "/metrics", "/healthz/", "/health", "/readyz/x"] {
            assert_eq!(
                status(&state, "GET", path).await,
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
        assert_eq!(
            status(&state, "POST", "/healthz").await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn serve_answers_over_tcp_and_stops_on_cancel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = HealthState::new();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(ferry::health::serve(
            listener,
            state.clone(),
            shutdown.clone(),
        ));

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/healthz");
        assert_eq!(client.get(&url).send().await.unwrap().status(), 503);
        state.beat();
        assert_eq!(client.get(&url).send().await.unwrap().status(), 200);
        assert_eq!(
            client
                .get(format!("http://{addr}/nope"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("server stops after cancellation")
            .unwrap()
            .unwrap();
    }
}
