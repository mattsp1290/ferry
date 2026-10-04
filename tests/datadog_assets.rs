//! Offline checks of the Datadog monitor and dashboard definitions in
//! `datadog/`. They guard the contract between `src/telemetry/metrics.rs`
//! (metric names), `src/sync/outcome.rs` (tag values), and the files that
//! query them. Nothing here talks to Datadog.

use std::collections::BTreeSet;
use std::path::PathBuf;

use ferry::sync::ErrorKind;
use ferry::telemetry::METRIC_NAMES;
use serde_json::Value;

const MONITOR_FILES: [&str; 3] = ["repo-stale", "sync-failing", "heartbeat-missing"];

/// Metrics that legitimately have no monitor or dashboard widget, with the
/// reason. Empty: all nine metrics are visualised.
const UNVISUALISED: &[(&str, &str)] = &[];

/// Metrics that carry no `repo` tag, so a `$repo` filter would hide them.
const NO_REPO_TAG: [&str; 3] = [
    "ferry.repos.configured",
    "ferry.cache.bytes",
    "ferry.heartbeat",
];

/// Space aggregators and percentile aggregators that may precede a metric
/// name as `agg:metric`.
const AGGREGATORS: [&str; 11] = [
    "avg", "sum", "min", "max", "count", "last", "p50", "p75", "p90", "p95", "p99",
];

fn datadog_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("datadog")
        .join(rel)
}

fn load(rel: &str) -> Value {
    let path = datadog_path(rel);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()))
}

fn monitor(name: &str) -> Value {
    load(&format!("monitors/{name}.json"))
}

fn dashboard() -> Value {
    load("dashboard.json")
}

/// Removes every `{...}` group (tag filters and `by {...}` groupings).
fn strip_braces(query: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in query.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Extracts the ferry metric names from a metric query string.
///
/// A token is `ferry.` followed by `[a-z0-9_.]`, with trailing dots removed.
/// Tag filters are stripped first. A token that follows a `:` counts only when
/// the word before the colon is an aggregator (`min:ferry.x`), so free-text
/// values such as `operation_name:ferry.sync_repo` are ignored.
fn metric_names_in(query: &str) -> Vec<String> {
    let text = strip_braces(query);
    let chars: Vec<char> = text.chars().collect();
    let mut names = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts_here = chars[i..].iter().take(6).collect::<String>() == "ferry.";
        if !starts_here {
            i += 1;
            continue;
        }
        let boundary_ok = match i.checked_sub(1).map(|p| chars[p]) {
            None => true,
            Some(':') => {
                let word: String = chars[..i - 1]
                    .iter()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                AGGREGATORS.contains(&word.as_str())
            }
            Some(p) => !(p.is_ascii_alphanumeric() || p == '_' || p == '.'),
        };
        let mut j = i;
        while j < chars.len()
            && (chars[j].is_ascii_lowercase()
                || chars[j].is_ascii_digit()
                || "_.".contains(chars[j]))
        {
            j += 1;
        }
        if boundary_ok {
            let token: String = chars[i..j].iter().collect();
            names.push(token.trim_end_matches('.').to_string());
        }
        i = j.max(i + 1);
    }
    names
}

/// Values used with `key:` (`result` or `error_kind`) in a query, including
/// negated filters such as `!error_kind:none`.
fn tag_values_in(query: &str, key: &str) -> Vec<String> {
    let needle = format!("{key}:");
    let mut values = Vec::new();
    let mut from = 0;
    while let Some(pos) = query[from..].find(&needle) {
        let start = from + pos;
        let before = query[..start].chars().next_back();
        let end = start + needle.len();
        from = end;
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        let value: String = query[end..]
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        values.push(value);
    }
    values
}

/// Every string stored under a `query` or `q` key, at any depth.
fn query_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                match (k.as_str(), v) {
                    ("query" | "q", Value::String(s)) => out.push(s.clone()),
                    _ => query_strings(v, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| query_strings(v, out)),
        _ => {}
    }
}

fn all_queries() -> Vec<(String, String)> {
    let mut all = Vec::new();
    for name in MONITOR_FILES {
        let mut qs = Vec::new();
        query_strings(&monitor(name), &mut qs);
        all.extend(qs.into_iter().map(|q| (name.to_string(), q)));
    }
    let mut qs = Vec::new();
    query_strings(&dashboard(), &mut qs);
    all.extend(qs.into_iter().map(|q| ("dashboard".to_string(), q)));
    all
}

#[test]
fn scanner_extracts_metric_names_and_ignores_tag_values() {
    assert_eq!(
        metric_names_in(
            "min(last_10m):min:ferry.repo.consecutive_failures{service:ferry} by {repo} >= 3"
        ),
        vec!["ferry.repo.consecutive_failures"]
    );
    assert_eq!(
        metric_names_in("sum:ferry.sync.runs{service:ferry,$env} by {result}.as_count()"),
        vec!["ferry.sync.runs"]
    );
    assert_eq!(
        metric_names_in("p95:ferry.sync.duration{service:ferry} by {repo}"),
        vec!["ferry.sync.duration"]
    );
    // Negative: tag values and free-text log/trace queries name no metric.
    assert!(metric_names_in("service:ferry operation_name:ferry.sync_repo").is_empty());
    assert!(metric_names_in("service:ferry status:error").is_empty());
    assert!(metric_names_in("sum:other.metric{service:ferry,repo:ferry.x}").is_empty());
    // A trailing dot is stripped.
    assert_eq!(
        metric_names_in("sum:ferry.heartbeat."),
        vec!["ferry.heartbeat"]
    );
}

#[test]
fn scanner_extracts_tag_values() {
    assert_eq!(
        tag_values_in(
            "sum:ferry.sync.runs{a,!error_kind:none,result:synced}",
            "error_kind"
        ),
        vec!["none"]
    );
    assert_eq!(
        tag_values_in("{result:synced,result:error}", "result"),
        vec!["synced", "error"]
    );
    assert!(tag_values_in("by {result}", "result").is_empty());
}

#[test]
fn every_file_parses() {
    for name in MONITOR_FILES {
        assert!(monitor(name).is_object(), "{name}");
    }
    assert!(dashboard().is_object());
}

#[test]
fn every_queried_metric_exists_in_metric_names() {
    let known: BTreeSet<&str> = METRIC_NAMES.iter().copied().collect();
    let mut seen = 0;
    for (file, query) in all_queries() {
        for name in metric_names_in(&query) {
            seen += 1;
            assert!(
                known.contains(name.as_str()),
                "{file}: query names unknown metric `{name}`: {query}"
            );
        }
    }
    assert!(seen > 0, "scanner found no metric names at all");
}

#[test]
fn every_metric_is_used_or_explicitly_allowed() {
    let used: BTreeSet<String> = all_queries()
        .iter()
        .flat_map(|(_, q)| metric_names_in(q))
        .collect();
    for name in METRIC_NAMES {
        let allowed = UNVISUALISED.iter().any(|(n, _)| *n == name);
        assert!(
            used.contains(name) || allowed,
            "metric `{name}` has no monitor or dashboard widget; add one or list it in UNVISUALISED with a reason"
        );
    }
}

#[test]
fn monitors_have_tags_scope_and_no_notification_handle() {
    for name in MONITOR_FILES {
        let m = monitor(name);
        assert_eq!(m["type"], "metric alert", "{name}");
        let tags: Vec<&str> = m["tags"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: tags"))
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(tags.contains(&"service:ferry"), "{name}: service tag");
        assert!(
            tags.contains(&"managed-by:ferry-repo"),
            "{name}: managed-by tag"
        );
        let query = m["query"].as_str().expect("query");
        assert!(query.contains("{service:ferry}"), "{name}: query scope");
        let message = m["message"].as_str().expect("message");
        // A handle is a word like `@team-ops`. A log attribute filter such
        // as `@repo:{{repo.name}}` also starts with `@` but has a colon.
        let handle = message
            .split_whitespace()
            .find(|word| word.starts_with('@') && !word.contains(':'));
        assert_eq!(handle, None, "{name}: message has a notification handle");
        if message.contains("repo:{{repo.name}}") {
            // `repo` is a log attribute, not a tag: without `@` the search
            // the runbook tells the operator to run matches nothing.
            assert!(
                message.contains("@repo:{{repo.name}}"),
                "{name}: the log query must use the @repo attribute"
            );
        }
        assert!(
            m["name"].as_str().is_some_and(|n| !n.contains('@')),
            "{name}: name has @"
        );
    }
}

#[test]
fn monitor_messages_reference_stop_switch_and_dashboard_by_title() {
    let title = dashboard()["title"].as_str().expect("title").to_string();
    for name in ["repo-stale", "sync-failing"] {
        let message = monitor(name)["message"].as_str().unwrap().to_string();
        assert!(message.contains("dest_unmanaged"), "{name}");
        assert!(message.contains("ferry-mirror"), "{name}");
        assert!(message.contains("{{repo.name}}"), "{name}");
    }
    for name in MONITOR_FILES {
        let message = monitor(name)["message"].as_str().unwrap().to_string();
        assert!(message.contains(&title), "{name}: dashboard title");
        assert!(
            !message.contains("http"),
            "{name}: dashboard must be named, not linked"
        );
        assert!(message.contains("{{#is_recovery}}"), "{name}");
    }
}

fn query_threshold(query: &str) -> f64 {
    query
        .split_whitespace()
        .last()
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no trailing threshold in `{query}`"))
}

#[test]
fn monitor_options_match_the_plan() {
    let stale = monitor("repo-stale");
    assert_eq!(stale["options"]["thresholds"]["critical"], 3600);
    assert_eq!(stale["options"]["thresholds"]["warning"], 1800);
    assert_eq!(stale["options"]["notify_no_data"], false);
    assert!(stale["query"].as_str().unwrap().contains("by {repo}"));

    let failing = monitor("sync-failing");
    assert_eq!(failing["options"]["thresholds"]["critical"], 3);
    assert!(failing["query"].as_str().unwrap().contains("by {repo}"));

    let heartbeat = monitor("heartbeat-missing");
    assert_eq!(heartbeat["options"]["notify_no_data"], true);
    assert_eq!(heartbeat["options"]["no_data_timeframe"], 10);

    for name in MONITOR_FILES {
        let m = monitor(name);
        let critical = m["options"]["thresholds"]["critical"]
            .as_f64()
            .expect("critical");
        assert_eq!(
            query_threshold(m["query"].as_str().unwrap()),
            critical,
            "{name}: query threshold differs from options.thresholds.critical"
        );
    }
}

#[test]
fn dashboard_has_template_variables_and_scoped_queries() {
    let d = dashboard();
    let vars: Vec<&str> = d["template_variables"]
        .as_array()
        .expect("template_variables")
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert!(vars.contains(&"env") && vars.contains(&"repo"), "{vars:?}");
    assert_eq!(d["layout_type"], "ordered");

    let mut qs = Vec::new();
    query_strings(&d["widgets"], &mut qs);
    for q in qs.iter().filter(|q| !metric_names_in(q).is_empty()) {
        assert!(q.contains("service:ferry"), "unscoped: {q}");
        assert!(q.contains("$env"), "no $env: {q}");
        let names = metric_names_in(q);
        let has_repo_tag = names.iter().all(|n| !NO_REPO_TAG.contains(&n.as_str()));
        assert_eq!(
            q.contains("$repo"),
            has_repo_tag,
            "$repo scoping wrong: {q}"
        );
    }
}

#[test]
fn tag_values_in_queries_are_real() {
    let results: BTreeSet<&str> = ["synced", "noop", "empty", "error"]
        .iter()
        .copied()
        .collect();
    let mut kinds: BTreeSet<&str> = ErrorKind::ALL.iter().map(|k| k.as_str()).collect();
    kinds.insert("none");

    for (file, query) in all_queries() {
        for v in tag_values_in(&query, "result") {
            assert!(
                results.contains(v.as_str()),
                "{file}: unknown result `{v}` in {query}"
            );
        }
        for v in tag_values_in(&query, "error_kind") {
            assert!(
                kinds.contains(v.as_str()),
                "{file}: unknown error_kind `{v}` in {query}"
            );
        }
    }
}
