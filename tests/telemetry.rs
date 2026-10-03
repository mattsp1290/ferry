//! Metric, log, trace, and health tests for `ferry::telemetry` and `ferry::health`.
//!
//! No test here mutates the process environment. Only
//! `init_installs_global_subscriber_once` installs a global subscriber; every
//! other test uses a scoped `Dispatch`.

mod support;

use std::net::UdpSocket;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::Value;
use opentelemetry::trace::Status;
use tracing::dispatcher::with_default;
use tracing::field::Empty;
use tracing::{Dispatch, info, info_span, warn};
use tracing_subscriber::layer::SubscriberExt;

use ferry::config::RepoEntry;
use ferry::sync::outcome::{ErrorKind, SyncOutcome, SyncResult};
use ferry::telemetry::logging::dd_ids;
use ferry::telemetry::metrics::{ConstTags, DogstatsdMetrics, DogstatsdTarget};
use ferry::telemetry::tracing::{build_datadog_provider, mark_error, otel_layer, parse_agent_url};
use ferry::telemetry::{LogFormat, Metrics, Settings, Telemetry};
use support::capture::{Capture, attr, attr_str, json_dispatch, memory_provider};

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

fn tags() -> ConstTags {
    ConstTags {
        service: "ferry".to_owned(),
        env: Some("test".to_owned()),
        version: "1.2.3".to_owned(),
    }
}

const CONST_TAGS: &str = "service:ferry,env:test,version:1.2.3";

fn recv_datagrams_udp(socket: &UdpSocket, count: usize) -> Vec<String> {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut out = Vec::new();
    let mut buf = [0_u8; 2048];
    for _ in 0..count {
        let (n, _) = socket.recv_from(&mut buf).expect("datagram arrives");
        out.push(String::from_utf8(buf[..n].to_vec()).unwrap());
    }
    out
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
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let target = DogstatsdTarget::parse(&format!("udp://127.0.0.1:{port}")).unwrap();
    let metrics = DogstatsdMetrics::connect_with_rotation(&target, &tags(), rotation).unwrap();
    (metrics, server)
}

fn expected_datagrams() -> Vec<String> {
    vec![
        format!(
            "ferry.sync.runs:1|c|#{CONST_TAGS},repo:acme/widget,result:error,error_kind:network"
        ),
        format!("ferry.sync.duration:1.5|d|#{CONST_TAGS},repo:acme/widget,result:error"),
        format!("ferry.sync.refs_changed:3|c|#{CONST_TAGS},repo:acme/widget"),
        format!("ferry.sync.refs_pruned:1|c|#{CONST_TAGS},repo:acme/widget"),
    ]
}

fn error_outcome() -> SyncOutcome {
    let mut outcome = SyncOutcome::error(ErrorKind::Network, Duration::from_millis(1500));
    outcome.refs_changed = 3;
    outcome.refs_pruned = 1;
    outcome
}

fn exercise_all_methods(metrics: &dyn Metrics) {
    metrics.sync_finished(&entry(), &error_outcome());
    metrics.sync_finished(
        &entry(),
        &SyncOutcome::success(SyncResult::Noop, Duration::from_secs(2)),
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
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let target = DogstatsdTarget::parse(&format!(
        "udp://127.0.0.1:{}",
        server.local_addr().unwrap().port()
    ))
    .unwrap();
    let mut tags = tags();
    tags.env = None;
    let metrics = DogstatsdMetrics::connect(&target, &tags).unwrap();
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
    let metrics = DogstatsdMetrics::connect(&target, &tags()).unwrap();

    exercise_all_methods(&metrics);

    let expected = expected_all_methods();
    let mut buf = [0_u8; 2048];
    let mut got = Vec::new();
    for _ in 0..expected.len() {
        let n = server.recv(&mut buf).expect("datagram arrives");
        got.push(String::from_utf8(buf[..n].to_vec()).unwrap());
    }
    assert_eq!(got, expected);
    assert_eq!(metrics.send_error_count(), 0);
}

#[test]
fn dogstatsd_unix_send_error_is_counted_not_propagated() {
    let dir = tempfile::tempdir().unwrap();
    let target = DogstatsdTarget::Unix {
        path: dir.path().join("nobody-listens.sock"),
    };
    let metrics = DogstatsdMetrics::connect(&target, &tags()).unwrap();
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

    // Past the interval, the next send starts a background refresh; it still
    // goes out on the old socket.
    std::thread::sleep(Duration::from_millis(250));
    metrics.heartbeat();
    let (_, still_first) = server.recv_from(&mut buf).unwrap();
    assert_eq!(still_first, first_peer);

    // After the refresh the new socket (new source port) carries the sends.
    std::thread::sleep(Duration::from_millis(500));
    metrics.heartbeat();
    let (n, new_peer) = server.recv_from(&mut buf).unwrap();
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        format!("ferry.heartbeat:1|g|#{CONST_TAGS}")
    );
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
    let (_guard, dispatch) = Telemetry::build(settings, capture.clone());
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
    let (_guard, dispatch) = Telemetry::build(settings, capture.clone());
    with_default(&dispatch, || {
        info!("dropped");
        warn!("kept");
    });
    let lines = capture.json_lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["message"], "kept");
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
// real Datadog provider against a fake agent (spike items 2 and 3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AgentRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn is_trace_payload(request: &AgentRequest) -> bool {
    request.method == "POST"
        && (request.path.starts_with("/v0.4/traces")
            || request.path.starts_with("/v0.5/traces")
            || request.path.starts_with("/v1.0/traces"))
}

const INFO_JSON: &str = r#"{"version":"7.60.0","endpoints":["/v0.4/traces","/v0.5/traces","/v0.6/stats","/info"],"client_drop_p0s":false,"config":{}}"#;
const TRACES_JSON: &str = r#"{"rate_by_service":{"service:ferry,env:test":1}}"#;

fn emit_sync_span() {
    let root = info_span!(
        "ferry.sync_repo",
        repo = "acme/widget",
        forgejo_repo = "mirror/widget"
    );
    let _root = root.enter();
    let _child = info_span!("git.fetch", git.side = "github").entered();
}

/// Creates spans through `tracing`, ends them, and shuts the provider down so
/// the exporter flushes. Runs the blocking shutdown off the async runtime.
async fn emit_and_shutdown(agent_url: &str) {
    let settings = Settings {
        env: Some("test".to_owned()),
        version: "1.2.3".to_owned(),
        ..Settings::default()
    };
    let url = parse_agent_url(agent_url).expect("agent URL is valid");
    let provider = build_datadog_provider(&settings, &url);
    let dispatch = Dispatch::new(tracing_subscriber::registry().with(otel_layer(&provider)));
    with_default(&dispatch, emit_sync_span);
    tokio::task::spawn_blocking(move || {
        provider
            .shutdown_with_timeout(Duration::from_secs(10))
            .expect("provider shuts down");
    })
    .await
    .unwrap();
}

/// The log level must not decide whether a sync is traced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level_above_info_still_exports_traces() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/info"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(INFO_JSON, "application/json"))
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(TRACES_JSON, "application/json"))
        .mount(&server)
        .await;

    let capture = Capture::default();
    let (mut guard, dispatch) = Telemetry::build(
        Settings {
            env: Some("test".to_owned()),
            version: "1.2.3".to_owned(),
            trace_agent_url: Some(server.uri()),
            log_level: "error".to_owned(),
            ..Settings::default()
        },
        capture.clone(),
    );
    assert!(guard.tracing_enabled());
    with_default(&dispatch, || {
        emit_sync_span();
        info!("filtered out of the log");
    });
    tokio::task::spawn_blocking(move || guard.shutdown())
        .await
        .unwrap();

    assert_eq!(
        capture.text(),
        "",
        "info lines must be filtered from the log"
    );
    let received = server.received_requests().await.unwrap();
    let requests: Vec<AgentRequest> = received
        .iter()
        .map(|r| AgentRequest {
            method: r.method.to_string(),
            path: r.url.path().to_owned(),
            body: r.body.clone(),
        })
        .collect();
    assert_trace_payload(&requests);
}

fn assert_trace_payload(requests: &[AgentRequest]) {
    let traces: Vec<_> = requests.iter().filter(|r| is_trace_payload(r)).collect();
    assert!(
        !traces.is_empty(),
        "no trace payload reached the agent; saw: {:?}",
        requests
            .iter()
            .map(|r| format!("{} {} ({} bytes)", r.method, r.path, r.body.len()))
            .collect::<Vec<_>>()
    );
    // Spike finding: the exporter posts MessagePack to /v0.4/traces.
    assert_eq!(traces[0].path, "/v0.4/traces");
    let body = &traces[0].body;
    let contains = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
    // MessagePack fixstr entries: `name` is the Datadog operation name.
    let name_entry = |name: &str| {
        let mut entry = vec![0xa4];
        entry.extend_from_slice(b"name");
        entry.push(0xa0 | u8::try_from(name.len()).unwrap());
        entry.extend_from_slice(name.as_bytes());
        entry
    };
    assert!(
        contains(&name_entry("ferry.sync_repo")),
        "operation name of the root span is the tracing span name"
    );
    assert!(contains(&name_entry("git.fetch")), "child operation name");
    assert!(contains(b"acme/widget"), "span attribute is in the payload");
    assert!(contains(b"ferry"), "service name is in the payload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datadog_provider_exports_to_an_http_agent() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/info"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(INFO_JSON, "application/json"))
        .mount(&server)
        .await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(TRACES_JSON, "application/json"))
        .mount(&server)
        .await;

    emit_and_shutdown(&server.uri()).await;

    let received = server.received_requests().await.unwrap();
    let requests: Vec<AgentRequest> = received
        .iter()
        .map(|r| AgentRequest {
            method: r.method.to_string(),
            path: r.url.path().to_owned(),
            body: r.body.clone(),
        })
        .collect();
    assert_trace_payload(&requests);
}

/// A minimal HTTP/1.1 responder on a Unix socket that records every request.
async fn serve_unix_agent(listener: tokio::net::UnixListener, log: Arc<Mutex<Vec<AgentRequest>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let log = Arc::clone(&log);
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            loop {
                // Read until the end of the headers.
                let header_end = loop {
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                    let mut chunk = [0_u8; 8192];
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default().to_owned();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let header = |name: &str| {
                    lines.clone().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                    })
                };
                let chunked =
                    header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
                let content_length: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                buf.drain(..header_end);

                let mut body = Vec::new();
                if chunked {
                    loop {
                        // chunk-size line
                        let line_end = loop {
                            if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                                break pos;
                            }
                            let mut chunk = [0_u8; 8192];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let size_text = String::from_utf8_lossy(&buf[..line_end]).into_owned();
                        let size =
                            usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16)
                                .unwrap_or(0);
                        buf.drain(..line_end + 2);
                        while buf.len() < size + 2 {
                            let mut chunk = [0_u8; 8192];
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        body.extend_from_slice(&buf[..size]);
                        buf.drain(..size + 2);
                        if size == 0 {
                            break;
                        }
                    }
                } else {
                    while buf.len() < content_length {
                        let mut chunk = [0_u8; 8192];
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    body.extend_from_slice(&buf[..content_length]);
                    buf.drain(..content_length);
                }

                let reply = if path == "/info" {
                    INFO_JSON
                } else {
                    TRACES_JSON
                };
                log.lock()
                    .unwrap()
                    .push(AgentRequest { method, path, body });
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len()
                );
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datadog_provider_exports_to_a_unix_socket_agent() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("apm.socket");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(serve_unix_agent(listener, Arc::clone(&log)));

    emit_and_shutdown(&format!("unix://{}", socket.display())).await;

    let requests = log.lock().unwrap().clone();
    server.abort();
    assert_trace_payload(&requests);
}

// ---------------------------------------------------------------------------
// disabled and invalid paths
// ---------------------------------------------------------------------------

#[test]
fn unset_urls_disable_both_signals() {
    let capture = Capture::default();
    let (mut guard, dispatch) = Telemetry::build(Settings::default(), capture.clone());
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
        let (guard, _dispatch) = Telemetry::build(settings, capture.clone());
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
    let (_guard, dispatch) = Telemetry::build(settings, capture.clone());
    with_default(&dispatch, || info!("after fallback"));
    let text = capture.text();
    assert!(text.contains("FERRY_LOG_LEVEL"));
    assert!(text.contains("after fallback"));
}

#[test]
fn valid_urls_enable_both_signals_and_metrics_reach_the_socket() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let settings = Settings {
        env: Some("test".to_owned()),
        version: "1.2.3".to_owned(),
        dogstatsd_url: Some(format!(
            "udp://127.0.0.1:{}",
            server.local_addr().unwrap().port()
        )),
        // Nothing listens here; export failures must not matter.
        trace_agent_url: Some("http://127.0.0.1:9".to_owned()),
        ..Settings::default()
    };
    let (mut guard, _dispatch) = Telemetry::build(settings, Capture::default());
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
    let guard = Telemetry::init(settings.clone());
    assert!(tracing::dispatcher::has_been_set());
    info!("global subscriber is live");
    // A second init must not panic; it keeps the first subscriber.
    let second = Telemetry::init(settings);
    drop(second);
    drop(guard);
}

// ---------------------------------------------------------------------------
// health (plan test 6, health half)
// ---------------------------------------------------------------------------

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
