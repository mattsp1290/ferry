//! The real Datadog trace exporter against a fake Agent over HTTP and over a
//! Unix socket (tracing spike items 2 and 3), and the rule that the log level
//! never decides whether a sync is traced.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tracing::dispatcher::with_default;
use tracing::{Dispatch, info, info_span};
use tracing_subscriber::layer::SubscriberExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ferry::telemetry::tracing::{build_datadog_provider, otel_layer, parse_agent_url};
use ferry::telemetry::{self, Settings};
use support::capture::{Capture, tags};

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
    let url = parse_agent_url(agent_url).expect("agent URL is valid");
    let provider = build_datadog_provider(&tags(), &url);
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

/// A fake Datadog Agent over HTTP: answers `/info`, accepts everything else.
async fn http_agent() -> MockServer {
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
    server
}

/// What the HTTP fake agent received, in the shape the assertions use.
async fn agent_requests(server: &MockServer) -> Vec<AgentRequest> {
    let received = server.received_requests().await.unwrap();
    received
        .iter()
        .map(|r| AgentRequest {
            method: r.method.to_string(),
            path: r.url.path().to_owned(),
            body: r.body.clone(),
        })
        .collect()
}

/// Reads more bytes from `stream` into `buf`. False at EOF or on error.
async fn read_more(stream: &mut UnixStream, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0_u8; 8192];
    match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

/// The log level must not decide whether a sync is traced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_level_above_info_still_exports_traces() {
    let server = http_agent().await;

    let capture = Capture::default();
    let (mut guard, dispatch) = telemetry::build(
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
    let requests = agent_requests(&server).await;
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
    let server = http_agent().await;

    emit_and_shutdown(&server.uri()).await;

    let requests = agent_requests(&server).await;
    assert_trace_payload(&requests);
}

/// A minimal HTTP/1.1 responder on a Unix socket that records every request.
async fn serve_unix_agent(listener: tokio::net::UnixListener, log: Arc<Mutex<Vec<AgentRequest>>>) {
    use tokio::io::AsyncWriteExt;

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
                    if !read_more(&mut stream, &mut buf).await {
                        return;
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
                            if !read_more(&mut stream, &mut buf).await {
                                return;
                            }
                        };
                        let size_text = String::from_utf8_lossy(&buf[..line_end]).into_owned();
                        let size =
                            usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16)
                                .unwrap_or(0);
                        buf.drain(..line_end + 2);
                        while buf.len() < size + 2 {
                            if !read_more(&mut stream, &mut buf).await {
                                return;
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
                        if !read_more(&mut stream, &mut buf).await {
                            return;
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
