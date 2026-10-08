// Usage-limit reset grants ("cedar_ember" in Anthropic's usage reply): the
// resets an account has been given, like Codex's reset cards. The Resets
// column shows how many of them are left.

use std::sync::{Arc, Mutex};

use axum::{Router, extract::RawQuery, routing::get};
use claude_switch::claude_api::{Endpoints, ResetGrant, fetch_usage, parse_usage};
use claude_switch::claude_usage::resets_label;
use serde_json::json;

fn grant(left: u32, total: u32, ends_at: Option<&str>) -> ResetGrant {
    ResetGrant {
        label: "launch reset".into(),
        resets_left: left,
        resets_total: total,
        ends_at: ends_at.map(str::to_owned),
        usable_now: false,
    }
}

#[test]
fn grants_are_parsed_from_the_usage_reply() {
    let usage = parse_usage(&json!({
        "five_hour": {"utilization": 10.0},
        "cedar_ember": {
            "eligible": true,
            "grants": [
                {"id": "g1", "label": "Claude Opus 5.5 launch: one usage-limit reset", "resets_total": 1,
                 "resets_left": 0, "starts_at": "2026-09-22T16:00:00+00:00",
                 "ends_at": "2026-10-22T16:00:00+00:00", "usable_now": false},
                {"id": "g2", "label": "bonus", "resets_total": 2, "resets_left": 2, "usable_now": true},
                {"id": "bad", "resets_left": "many"}
            ]
        }
    }))
    .unwrap();
    let grants = usage.reset_grants.expect("grants");
    assert_eq!(grants.len(), 2, "the malformed grant is skipped");
    assert_eq!(grants[0].label, "Claude Opus 5.5 launch: one usage-limit reset");
    assert_eq!((grants[0].resets_left, grants[0].resets_total), (0, 1));
    assert_eq!(grants[0].ends_at.as_deref(), Some("2026-10-22T16:00:00+00:00"));
    assert_eq!((grants[1].resets_left, grants[1].resets_total, grants[1].usable_now), (2, 2, true));
}

#[test]
fn no_cedar_ember_block_means_no_grant_data() {
    let usage = parse_usage(&json!({"five_hour": {"utilization": 10.0}})).unwrap();
    assert_eq!(usage.reset_grants, None);
    let empty = parse_usage(&json!({"five_hour": {"utilization": 10.0}, "cedar_ember": {"eligible": true, "grants": []}})).unwrap();
    assert_eq!(empty.reset_grants, Some(vec![]));
}

#[test]
fn the_column_shows_resets_left_out_of_the_total() {
    let used = [grant(0, 1, Some("2099-10-22T16:00:00Z"))];
    assert_eq!(resets_label(Some(&used), None), ("0/1".to_string(), false));
    let some = [grant(1, 1, Some("2099-10-22T16:00:00Z")), grant(2, 2, None)];
    assert_eq!(resets_label(Some(&some), None), ("3/3".to_string(), true));
}

#[test]
fn expired_grants_do_not_count() {
    let grants = [grant(1, 1, Some("2000-01-01T00:00:00Z")), grant(0, 1, Some("2099-01-01T00:00:00Z"))];
    assert_eq!(resets_label(Some(&grants), None), ("0/1".to_string(), false));
    let all_expired = [grant(1, 1, Some("2000-01-01T00:00:00Z"))];
    assert_eq!(resets_label(Some(&all_expired), None).0, "0");
}

#[test]
fn without_grant_data_the_column_is_a_dash() {
    assert_eq!(resets_label(None, None).0, "--");
    assert_eq!(resets_label(Some(&[]), None).0, "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_usage_request_asks_for_the_grants() {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let record = seen.clone();
    let app = Router::new().route(
        "/api/oauth/usage",
        get(move |RawQuery(query): RawQuery| {
            let record = record.clone();
            async move {
                record.lock().unwrap().push(query);
                axum::Json(json!({"five_hour": {"utilization": 10.0}}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let endpoints = Endpoints { api_base: base.clone(), token_url: format!("{base}/token") };

    fetch_usage(&client, &endpoints, "tok").await.unwrap();

    let query = seen.lock().unwrap()[0].clone().unwrap_or_default();
    let parts: Vec<&str> = query.split('&').collect();
    assert!(parts.contains(&"cedar_ember=1") && parts.contains(&"at_wall=1"), "query: {query}");
}
