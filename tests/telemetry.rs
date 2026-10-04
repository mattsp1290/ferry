//! Metric, log, trace, and health tests for `ferry::telemetry` and `ferry::health`.
//!
//! No test here mutates the process environment. Only
//! `init_installs_global_subscriber_once` installs a global subscriber; every
//! other test uses a scoped `Dispatch`.

mod support;

use std::net::UdpSocket;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use opentelemetry::Value;
use opentelemetry::trace::Status;
use tracing::dispatcher::with_default;
use tracing::field::Empty;
use tracing::{Dispatch, info, info_span, warn};
use tracing_subscriber::layer::SubscriberExt;

use ferry::config::RepoEntry;
use ferry::sync::outcome::{ErrorKind, SyncOutcome, SyncStatus};
use ferry::telemetry::dogstatsd::UDP_ROTATION_INTERVAL;
use ferry::telemetry::logging::dd_ids;
use ferry::telemetry::tracing::{mark_error, otel_layer, parse_agent_url};
use ferry::telemetry::{self, DogstatsdMetrics, DogstatsdTarget, LogFormat, Metrics, Settings};
use support::capture::{
    Capture, attr, attr_str, json_dispatch, json_dispatch_at, memory_provider, tags,
};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn entry() -> RepoEntry {
    RepoEntry {
        github: "Acme/Widget".to_owned(),
        forgejo: "mirror/widget".to_owned(),
        lfs: true,
        actions: false,
        adopt: false,
    }
}

const CONST_TAGS: &str = "service:ferry,env:test,version:1.2.3";

/// Receives `count` datagrams through `recv`, which returns the byte count.
fn recv_datagrams(
    count: usize,
    mut recv: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = [0_u8; 2048];
    for _ in 0..count {
        let n = recv(&mut buf).expect("datagram arrives");
        out.push(String::from_utf8(buf[..n].to_vec()).unwrap());
    }
    out
}

fn recv_datagrams_udp(socket: &UdpSocket, count: usize) -> Vec<String> {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    recv_datagrams(count, |buf| socket.recv_from(buf).map(|(n, _)| n))
}

/// A local UDP socket standing in for the Agent, and its `udp://` URL.
fn udp_server() -> (UdpSocket, String) {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let url = format!("udp://127.0.0.1:{}", server.local_addr().unwrap().port());
    (server, url)
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

#[test]
fn settings_defaults() {
    let settings = Settings::from_lookup(|_| None);
    assert_eq!(settings.service, "ferry");
    assert_eq!(settings.env, None);
    assert_eq!(settings.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(settings.dogstatsd_url, None);
    assert_eq!(settings.trace_agent_url, None);
    assert_eq!(settings.log_format, LogFormat::Json);
    assert_eq!(settings.log_level, "info");
    assert!(settings.warnings.is_empty());
    assert_eq!(settings, Settings::default());
}

#[test]
fn settings_read_every_variable() {
    let settings = Settings::from_lookup(|name| {
        let value = match name {
            "DD_SERVICE" => "svc",
            "DD_ENV" => "prod",
            "DD_VERSION" => "abc123",
            "DD_DOGSTATSD_URL" => "udp://dsd:8125",
            "DD_TRACE_AGENT_URL" => "http://apm:8126",
            "FERRY_LOG_FORMAT" => "TEXT",
            "FERRY_LOG_LEVEL" => "ferry=debug",
            _ => return None,
        };
        Some(value.to_owned())
    });
    assert_eq!(settings.service, "svc");
    assert_eq!(settings.env.as_deref(), Some("prod"));
    assert_eq!(settings.version, "abc123");
    assert_eq!(settings.dogstatsd_url.as_deref(), Some("udp://dsd:8125"));
    assert_eq!(settings.trace_agent_url.as_deref(), Some("http://apm:8126"));
    assert_eq!(settings.log_format, LogFormat::Text);
    assert_eq!(settings.log_level, "ferry=debug");
}

#[test]
fn settings_treat_empty_as_unset_and_bad_format_as_warning() {
    let settings = Settings::from_lookup(|name| match name {
        "DD_ENV" | "DD_DOGSTATSD_URL" => Some("  ".to_owned()),
        "FERRY_LOG_FORMAT" => Some("yaml".to_owned()),
        _ => None,
    });
    assert_eq!(settings.env, None);
    assert_eq!(settings.dogstatsd_url, None);
    assert_eq!(settings.log_format, LogFormat::Json);
    assert_eq!(settings.warnings.len(), 1);
}

// ---------------------------------------------------------------------------
// DogStatsD (plan test 1)
// ---------------------------------------------------------------------------

fn udp_metrics(rotation: Duration) -> (DogstatsdMetrics, UdpSocket) {
    let (server, url) = udp_server();
    let target = DogstatsdTarget::parse(&url).unwrap();
    let metrics = DogstatsdMetrics::connect(&target, &tags(), rotation)
        .unwrap()
        .0;
    (metrics, server)
}

fn expected_datagrams() -> Vec<String> {
    vec![
        format!(
            "ferry.sync.runs:1|c|#{CONST_TAGS},repo:acme/widget,result:error,error_kind:network"
        ),
        format!("ferry.sync.duration:1.5|d|#{CONST_TAGS},repo:acme/widget,result:error"),
        format!("ferry.sync.refs_changed:0|c|#{CONST_TAGS},repo:acme/widget"),
        format!("ferry.sync.refs_pruned:0|c|#{CONST_TAGS},repo:acme/widget"),
    ]
}

fn error_outcome() -> SyncOutcome {
    SyncOutcome {
        status: SyncStatus::Failed {
            kind: ErrorKind::Network,
            retry_after: None,
        },
        duration: Duration::from_millis(1500),
    }
}

fn exercise_all_methods(metrics: &dyn Metrics) {
    metrics.sync_finished(&entry(), &error_outcome());
    metrics.sync_finished(
        &entry(),
        &SyncOutcome {
            status: SyncStatus::Noop,
            duration: Duration::from_secs(2),
        },
    );
    metrics.repo_state(&entry(), Duration::from_millis(90_500), 4);
    metrics.repos_configured(7);
    metrics.cache_bytes(123_456);
    metrics.heartbeat();
}

fn expected_all_methods() -> Vec<String> {
    let mut expected = expected_datagrams();
    expected.extend([
        format!("ferry.sync.runs:1|c|#{CONST_TAGS},repo:acme/widget,result:noop,error_kind:none"),
        format!("ferry.sync.duration:2|d|#{CONST_TAGS},repo:acme/widget,result:noop"),
        format!("ferry.sync.refs_changed:0|c|#{CONST_TAGS},repo:acme/widget"),
        format!("ferry.sync.refs_pruned:0|c|#{CONST_TAGS},repo:acme/widget"),
        format!("ferry.repo.last_success_age_seconds:90.5|g|#{CONST_TAGS},repo:acme/widget"),
        format!("ferry.repo.consecutive_failures:4|g|#{CONST_TAGS},repo:acme/widget"),
        format!("ferry.repos.configured:7|g|#{CONST_TAGS}"),
        format!("ferry.cache.bytes:123456|g|#{CONST_TAGS}"),
        format!("ferry.heartbeat:1|g|#{CONST_TAGS}"),
    ]);
    expected
}

#[test]
fn dogstatsd_udp_emits_exact_datagrams_for_sync_finished() {
    let (metrics, server) = udp_metrics(Duration::from_secs(60));
    metrics.sync_finished(&entry(), &error_outcome());
    assert_eq!(recv_datagrams_udp(&server, 4), expected_datagrams());
    assert_eq!(metrics.send_error_count(), 0);
}

#[test]
fn dogstatsd_udp_emits_every_trait_method() {
    let (metrics, server) = udp_metrics(Duration::from_secs(60));
    exercise_all_methods(&metrics);
    let expected = expected_all_methods();
    assert_eq!(recv_datagrams_udp(&server, expected.len()), expected);
}

#[test]
fn dogstatsd_omits_env_tag_when_unset() {
    let (server, url) = udp_server();
    let target = DogstatsdTarget::parse(&url).unwrap();
    let mut tags = tags();
    tags.env = None;
    let metrics = DogstatsdMetrics::connect(&target, &tags, UDP_ROTATION_INTERVAL)
        .unwrap()
        .0;
    metrics.heartbeat();
    assert_eq!(
        recv_datagrams_udp(&server, 1),
        ["ferry.heartbeat:1|g|#service:ferry,version:1.2.3"]
    );
}

#[test]
fn dogstatsd_unix_datagram_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dsd.sock");
    let server = UnixDatagram::bind(&path).unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let target = DogstatsdTarget::parse(&format!("unix://{}", path.display())).unwrap();
    let metrics = DogstatsdMetrics::connect(&target, &tags(), UDP_ROTATION_INTERVAL)
        .unwrap()
        .0;

    exercise_all_methods(&metrics);

    let expected = expected_all_methods();
    let got = recv_datagrams(expected.len(), |buf| server.recv(buf));
    assert_eq!(got, expected);
    assert_eq!(metrics.send_error_count(), 0);
}

#[test]
fn dogstatsd_unix_send_error_is_counted_not_propagated() {
    let dir = tempfile::tempdir().unwrap();
    let target = DogstatsdTarget::Unix {
        path: dir.path().join("nobody-listens.sock"),
    };
    let metrics = DogstatsdMetrics::connect(&target, &tags(), UDP_ROTATION_INTERVAL)
        .unwrap()
        .0;
    metrics.heartbeat();
    metrics.heartbeat();
    assert_eq!(metrics.send_error_count(), 2);
}

#[test]
fn dogstatsd_udp_socket_is_recreated_after_the_rotation_interval() {
    let (metrics, server) = udp_metrics(Duration::from_millis(100));

    metrics.heartbeat();
    let mut buf = [0_u8; 512];
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (_, first_peer) = server.recv_from(&mut buf).unwrap();

    // Past the interval, a send starts a background refresh and later sends
    // use the new socket (new source port). Poll instead of guessing how long
    // the refresh thread takes.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let new_peer = loop {
        std::thread::sleep(Duration::from_millis(120));
        metrics.heartbeat();
        let (n, peer) = server.recv_from(&mut buf).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf[..n]).unwrap(),
            format!("ferry.heartbeat:1|g|#{CONST_TAGS}")
        );
        if peer.port() != first_peer.port() {
            break peer;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the socket was never re-created"
        );
    };
    assert_ne!(new_peer.port(), first_peer.port(), "socket was re-created");
    assert_eq!(metrics.send_error_count(), 0);
}

// ---------------------------------------------------------------------------
// logging (plan test 4)
// ---------------------------------------------------------------------------

#[test]
fn json_log_has_reserved_fields_and_event_fields_outside_a_span() {
    let capture = Capture::default();
    let (provider, _exporter) = memory_provider();
    with_default(&json_dispatch(&capture, &provider), || {
        info!(answer = 42, ok = true, name = "x", "hello world");
    });

    let lines = capture.json_lines();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["message"], "hello world");
    assert_eq!(line["level"], "info");
    assert_eq!(line["service"], "ferry");
    assert_eq!(line["env"], "test");
    assert_eq!(line["version"], "1.2.3");
    assert_eq!(line["answer"], 42);
    assert_eq!(line["ok"], true);
    assert_eq!(line["name"], "x");
    assert!(line["target"].as_str().unwrap().contains("telemetry"));
    let timestamp = line["timestamp"].as_str().unwrap();
    // RFC 3339 UTC: 2026-01-02T03:04:05[.fraction]Z
    assert!(timestamp.ends_with('Z') && timestamp.as_bytes()[10] == b'T');
    assert!(line.get("dd.trace_id").is_none(), "no span, no ids");
}

#[test]
fn json_log_flattens_span_fields_and_adds_dd_ids_matching_the_exported_span() {
    let capture = Capture::default();
    let (provider, exporter) = memory_provider();
    with_default(&json_dispatch(&capture, &provider), || {
        let root = info_span!(
            "ferry.sync_repo",
            repo = "acme/widget",
            forgejo_repo = "mirror/widget",
            result = Empty
        );
        let _root = root.enter();
        info!("sync started");
        root.record("result", "synced");
        let child = info_span!("git.fetch", step = "fetch", git.side = "github");
        let _child = child.enter();
        warn!(exit = 128, "fetch failed");
    });
    provider.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let root = spans.iter().find(|s| s.name == "ferry.sync_repo").unwrap();
    let child = spans.iter().find(|s| s.name == "git.fetch").unwrap();

    let lines = capture.json_lines();
    assert_eq!(lines.len(), 2);

    let (root_trace, root_span) = dd_ids(root.span_context.trace_id(), root.span_context.span_id());
    assert_eq!(lines[0]["message"], "sync started");
    assert_eq!(lines[0]["repo"], "acme/widget");
    assert_eq!(lines[0]["forgejo_repo"], "mirror/widget");
    assert_eq!(lines[0]["dd.trace_id"], root_trace);
    assert_eq!(lines[0]["dd.span_id"], root_span);

    let (child_trace, child_span) =
        dd_ids(child.span_context.trace_id(), child.span_context.span_id());
    assert_eq!(lines[1]["level"], "warn");
    assert_eq!(lines[1]["exit"], 128);
    assert_eq!(
        lines[1]["repo"], "acme/widget",
        "parent span field flattened"
    );
    assert_eq!(lines[1]["step"], "fetch");
    assert_eq!(lines[1]["git.side"], "github");
    assert_eq!(
        lines[1]["result"], "synced",
        "late-recorded field is included"
    );
    assert_eq!(lines[1]["dd.trace_id"], child_trace);
    assert_eq!(lines[1]["dd.span_id"], child_span);
    assert_eq!(child_trace, root_trace, "same trace");
    assert_ne!(child_span, root_span);

    // The encoding: decimal strings; trace id is the low 64 bits of the 128-bit id.
    let trace_bytes = root.span_context.trace_id().to_bytes();
    let low = u64::from_be_bytes(trace_bytes[8..].try_into().unwrap());
    assert_eq!(root_trace, low.to_string());
    assert!(root_trace.bytes().all(|b| b.is_ascii_digit()));
    let span_u64 = u64::from_be_bytes(root.span_context.span_id().to_bytes());
    assert_eq!(root_span, span_u64.to_string());
}

#[test]
fn reserved_fields_cannot_be_overridden_by_spans_or_events() {
    let capture = Capture::default();
    let (provider, _exporter) = memory_provider();
    with_default(&json_dispatch(&capture, &provider), || {
        let span = info_span!("s", service = "evil", level = "bogus");
        let _span = span.enter();
        info!(version = "9.9.9", "real message");
    });
    let line = &capture.json_lines()[0];
    assert_eq!(line["service"], "ferry");
    assert_eq!(line["level"], "info");
    assert_eq!(line["version"], "1.2.3");
}

#[test]
fn text_format_writes_plain_lines() {
    let capture = Capture::default();
    let settings = Settings {
        log_format: LogFormat::Text,
        ..Settings::default()
    };
    let (_guard, dispatch) = telemetry::build(settings, capture.clone());
    with_default(&dispatch, || info!("plain text line"));
    let text = capture.text();
    assert!(text.contains("plain text line"));
    assert!(serde_json::from_str::<serde_json::Value>(text.trim()).is_err());
}

#[test]
fn log_level_filters_events() {
    let capture = Capture::default();
    let settings = Settings {
        log_level: "warn".to_owned(),
        ..Settings::default()
    };
    let (_guard, dispatch) = telemetry::build(settings, capture.clone());
    with_default(&dispatch, || {
        info!("dropped");
        warn!("kept");
    });
    let lines = capture.json_lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["message"], "kept");
}

/// Raising the log level must not strip the repository and the trace ID
/// from the error lines that remain.
#[test]
fn error_lines_above_info_keep_span_fields_and_correlation_ids() {
    let capture = Capture::default();
    let (provider, exporter) = memory_provider();
    let dispatch = json_dispatch_at(&capture, &provider, "warn");
    with_default(&dispatch, || {
        let span = info_span!("ferry.sync_repo", repo = "owner/alpha");
        let _entered = span.enter();
        info!("filtered out");
        ::tracing::error!(error_kind = "network", "sync failed");
    });

    let lines = capture.json_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["message"], "sync failed");
    assert_eq!(lines[0]["repo"], "owner/alpha");
    let span = &exporter.get_finished_spans().unwrap()[0];
    let (trace_id, span_id) = dd_ids(span.span_context.trace_id(), span.span_context.span_id());
    assert_eq!(lines[0]["dd.trace_id"], trace_id.as_str());
    assert_eq!(lines[0]["dd.span_id"], span_id.as_str());
}

/// With the Agent down the exporter crates log every failed export. Those
/// lines are limited so that they cannot bury real sync failures.
#[test]
fn exporter_error_lines_are_throttled() {
    let capture = Capture::default();
    let (provider, _exporter) = memory_provider();
    let dispatch = json_dispatch(&capture, &provider);
    with_default(&dispatch, || {
        for _ in 0..5 {
            ::tracing::error!(target: "libdd_trace_utils::send_with_retry", "Max retries exceeded");
            ::tracing::error!(target: "libdd_data_pipeline::trace_exporter", "Error sending traces");
        }
        ::tracing::error!("sync failed");
        ::tracing::error!("sync failed");
    });

    let lines = capture.json_lines();
    let from_exporter = lines
        .iter()
        .filter(|line| {
            line["target"]
                .as_str()
                .is_some_and(|t| t.starts_with("libdd_"))
        })
        .count();
    assert_eq!(from_exporter, 1, "{lines:?}");
    assert_eq!(lines.len(), 3, "ferry's own lines are never throttled");
}

// ---------------------------------------------------------------------------
// tracing, in-memory exporter (plan test 5, tracing half)
// ---------------------------------------------------------------------------

#[test]
fn sync_spans_form_one_trace_with_declared_attributes_and_error_marking() {
    let (provider, exporter) = memory_provider();
    let dispatch = Dispatch::new(tracing_subscriber::registry().with(otel_layer(&provider)));
    with_default(&dispatch, || {
        let root = info_span!(
            "ferry.sync_repo",
            repo = "acme/widget",
            forgejo_repo = "mirror/widget",
            result = Empty,
            error_kind = Empty,
            refs_changed = Empty,
        );
        let _root = root.enter();
        {
            let fetch = info_span!("git.fetch", git.side = "github", git.exit_code = Empty);
            let _fetch = fetch.enter();
            fetch.record("git.exit_code", 0_i64);
        }
        {
            let api = info_span!(
                "forgejo.api",
                http.method = "GET",
                http.route = "/api/v1/repos/{owner}/{repo}",
                http.status_code = Empty
            );
            let _api = api.enter();
            api.record("http.status_code", 502_i64);
        }
        root.record("result", "error");
        root.record("error_kind", "network");
        root.record("refs_changed", 0_i64);
        mark_error(&root, "network");
    });
    provider.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 3);
    let root = spans.iter().find(|s| s.name == "ferry.sync_repo").unwrap();
    let fetch = spans.iter().find(|s| s.name == "git.fetch").unwrap();
    let api = spans.iter().find(|s| s.name == "forgejo.api").unwrap();

    assert_eq!(root.parent_span_id, opentelemetry::SpanId::INVALID, "root");
    for child in [fetch, api] {
        assert_eq!(child.parent_span_id, root.span_context.span_id());
        assert_eq!(child.span_context.trace_id(), root.span_context.trace_id());
    }
    assert_eq!(
        spans
            .iter()
            .filter(|s| s.parent_span_id == opentelemetry::SpanId::INVALID)
            .count(),
        1,
        "exactly one root span"
    );

    assert_eq!(attr_str(root, "repo").as_deref(), Some("acme/widget"));
    assert_eq!(
        attr_str(root, "forgejo_repo").as_deref(),
        Some("mirror/widget")
    );
    assert_eq!(attr_str(root, "result").as_deref(), Some("error"));
    assert_eq!(attr_str(root, "error_kind").as_deref(), Some("network"));
    assert_eq!(attr(root, "refs_changed"), Some(Value::I64(0)));
    assert_eq!(attr_str(fetch, "git.side").as_deref(), Some("github"));
    assert_eq!(attr(fetch, "git.exit_code"), Some(Value::I64(0)));
    assert_eq!(attr_str(api, "http.method").as_deref(), Some("GET"));
    assert_eq!(
        attr_str(api, "http.route").as_deref(),
        Some("/api/v1/repos/{owner}/{repo}")
    );
    assert_eq!(attr(api, "http.status_code"), Some(Value::I64(502)));

    // mark_error: error status and error.type.
    assert!(
        matches!(root.status, Status::Error { .. }),
        "{:?}",
        root.status
    );
    assert_eq!(attr_str(root, "error.type").as_deref(), Some("network"));
    assert!(matches!(fetch.status, Status::Unset));
    assert!(attr(fetch, "error.type").is_none());
}

#[test]
fn operation_name_processor_makes_span_name_the_datadog_operation_name() {
    let (provider, exporter) = memory_provider();
    let dispatch = Dispatch::new(tracing_subscriber::registry().with(otel_layer(&provider)));
    with_default(&dispatch, || {
        let _root = info_span!("ferry.sync_repo").entered();
        let _explicit = info_span!("custom", operation.name = "kept.name").entered();
    });
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let root = spans.iter().find(|s| s.name == "ferry.sync_repo").unwrap();
    assert_eq!(
        attr_str(root, "operation.name").as_deref(),
        Some("ferry.sync_repo")
    );
    let custom = spans.iter().find(|s| s.name == "custom").unwrap();
    assert_eq!(
        attr_str(custom, "operation.name").as_deref(),
        Some("kept.name"),
        "an explicit operation.name wins"
    );
}

// ---------------------------------------------------------------------------
// disabled and invalid paths
// ---------------------------------------------------------------------------

#[test]
fn initial_udp_failure_is_returned_without_arming_send_error_throttle() {
    // No DNS is performed: this is not a socket address and has no port.
    let target = DogstatsdTarget::Udp {
        host_port: "invalid-address".into(),
    };
    let (metrics, warning) =
        DogstatsdMetrics::connect(&target, &tags(), UDP_ROTATION_INTERVAL).unwrap();
    assert!(warning.unwrap().contains("initial resolve failed"));
    assert_eq!(metrics.send_error_count(), 0);
    let capture = Capture::default();
    let (provider, _) = memory_provider();
    let dispatch = json_dispatch(&capture, &provider);
    with_default(&dispatch, || exercise_all_methods(&metrics));
    assert!(metrics.send_error_count() > 0);
    assert!(capture.json_lines().iter().any(|line| {
        line["message"]
            .as_str()
            .is_some_and(|message| message.contains("dogstatsd send failed"))
    }));
}

#[test]
fn unset_urls_disable_both_signals() {
    let capture = Capture::default();
    let (mut guard, dispatch) = telemetry::build(Settings::default(), capture.clone());
    assert!(!guard.metrics_enabled());
    assert!(!guard.tracing_enabled());
    assert!(guard.tracer_provider().is_none());
    // The no-op backend accepts every call.
    exercise_all_methods(guard.metrics().as_ref());
    with_default(&dispatch, || info!("still logs"));
    assert_eq!(capture.json_lines().len(), 1, "no warnings, one event");
    guard.shutdown();
    guard.shutdown();
}

#[test]
fn invalid_urls_warn_and_disable_instead_of_failing() {
    for (dogstatsd, trace) in [
        ("not a url", "ftp://agent"),
        ("http://wrong-scheme:8125", "unix://"),
        ("udp://", "http://"),
    ] {
        let capture = Capture::default();
        let settings = Settings {
            dogstatsd_url: Some(dogstatsd.to_owned()),
            trace_agent_url: Some(trace.to_owned()),
            ..Settings::default()
        };
        let (guard, _dispatch) = telemetry::build(settings, capture.clone());
        assert!(!guard.metrics_enabled(), "{dogstatsd}");
        assert!(!guard.tracing_enabled(), "{trace}");
        let text = capture.text();
        assert!(text.contains("metrics are disabled"), "{text}");
        assert!(text.contains("tracing is disabled"), "{text}");
        for line in capture.json_lines() {
            assert_eq!(line["level"], "warn");
        }
        guard.metrics().heartbeat();
    }
}

#[test]
fn invalid_log_level_falls_back_to_info() {
    let capture = Capture::default();
    let settings = Settings {
        log_level: "[[[".to_owned(),
        ..Settings::default()
    };
    let (_guard, dispatch) = telemetry::build(settings, capture.clone());
    with_default(&dispatch, || info!("after fallback"));
    let text = capture.text();
    assert!(text.contains("FERRY_LOG_LEVEL"));
    assert!(text.contains("after fallback"));
}

#[test]
fn valid_urls_enable_both_signals_and_metrics_reach_the_socket() {
    let (server, url) = udp_server();
    let settings = Settings {
        env: Some("test".to_owned()),
        version: "1.2.3".to_owned(),
        dogstatsd_url: Some(url),
        // Nothing listens here; export failures must not matter.
        trace_agent_url: Some("http://127.0.0.1:9".to_owned()),
        ..Settings::default()
    };
    let (mut guard, _dispatch) = telemetry::build(settings, Capture::default());
    assert!(guard.metrics_enabled());
    assert!(guard.tracing_enabled());
    guard.metrics().heartbeat();
    assert_eq!(
        recv_datagrams_udp(&server, 1),
        [format!("ferry.heartbeat:1|g|#{CONST_TAGS}")]
    );
    guard.shutdown();
    assert!(!guard.tracing_enabled(), "shutdown consumed the provider");
}

#[test]
fn agent_url_validation() {
    for good in [
        "http://127.0.0.1:8126",
        "https://agent.example:8126",
        "unix:///var/run/datadog/apm.socket",
    ] {
        assert!(parse_agent_url(good).is_ok(), "{good}");
    }
    for bad in ["", "agent:8126", "udp://agent:8126", "unix://", "http://"] {
        assert!(parse_agent_url(bad).is_err(), "{bad:?}");
    }
}

#[tokio::test]
async fn init_installs_global_subscriber_once() {
    let settings = Settings {
        log_level: "info".to_owned(),
        ..Settings::default()
    };
    let guard = telemetry::init(settings.clone());
    assert!(tracing::dispatcher::has_been_set());
    info!("global subscriber is live");
    // A second init must not panic; it keeps the first subscriber.
    let second = telemetry::init(settings);
    drop(second);
    drop(guard);
}
