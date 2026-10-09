// Usage-limit reset grants ("cedar_ember" in Anthropic's usage reply): the
// resets an account has been given, like Codex's reset cards. The Resets
// column shows how many of them are left.

use std::sync::{Arc, Mutex};

use axum::{Router, extract::RawQuery, routing::get};
use claude_switch::claude_api::{
    Endpoints, ResetGrant, fetch_usage, new_request_id, parse_usage, usable_reset_grant,
};
use claude_switch::claude_usage::resets_label;
use serde_json::json;

fn grant(left: u32, total: u32, ends_at: Option<&str>) -> ResetGrant {
    ResetGrant {
        id: String::new(),
        paused: false,
        clears: vec![],
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
fn the_column_shows_how_many_resets_are_left() {
    let used = [grant(0, 1, Some("2099-10-22T12:00:00Z"))];
    assert_eq!(resets_label(Some(&used), None), ("0".to_string(), false));
    let no_end = [grant(2, 2, None)];
    assert_eq!(resets_label(Some(&no_end), None), ("2".to_string(), true));
}

/// Resets still left show when the first of them expires (local date).
#[test]
fn resets_left_show_their_earliest_expiry() {
    let some = [
        grant(1, 1, Some("2099-11-05T12:00:00Z")),
        grant(1, 1, Some("2099-10-22T12:00:00Z")),
        grant(0, 1, Some("2099-10-01T12:00:00Z")),
        grant(2, 2, None),
    ];
    assert_eq!(resets_label(Some(&some), None), ("4 (10-22)".to_string(), true));
}

#[test]
fn expired_grants_do_not_count() {
    let grants = [grant(1, 1, Some("2000-01-01T00:00:00Z")), grant(0, 1, Some("2099-01-01T12:00:00Z"))];
    assert_eq!(resets_label(Some(&grants), None), ("0".to_string(), false));
    let all_expired = [grant(1, 1, Some("2000-01-01T00:00:00Z"))];
    assert_eq!(resets_label(Some(&all_expired), None).0, "0");
}

#[test]
fn without_grant_data_the_column_is_a_dash() {
    assert_eq!(resets_label(None, None).0, "--");
    assert_eq!(resets_label(Some(&[]), None).0, "0");
}

fn usage_with_block(block: serde_json::Value) -> claude_switch::claude_api::ClaudeUsage {
    parse_usage(&json!({"five_hour": {"utilization": 10.0}, "cedar_ember": block})).unwrap()
}

fn two_grants(next: serde_json::Value, eligible: bool) -> serde_json::Value {
    json!({
        "eligible": eligible,
        "next_grant_id": next,
        "grants": [
            {"id": "g1", "label": "a", "resets_total": 1, "resets_left": 0},
            {"id": "g2", "label": "b", "resets_total": 2, "resets_left": 2, "paused": false,
             "clears": ["five_hour", "seven_day"], "ends_at": "2099-10-22T16:00:00+00:00"}
        ]
    })
}

#[test]
fn the_next_grant_and_what_it_clears_are_parsed() {
    let usage = usage_with_block(two_grants(json!("g2"), true));
    assert_eq!(usage.next_reset_grant.as_deref(), Some("g2"));
    let grants = usage.reset_grants.expect("grants");
    assert_eq!(grants[1].id, "g2");
    assert_eq!(grants[1].clears, vec!["five_hour".to_string(), "seven_day".to_string()]);
    assert!(!grants[0].paused);
}

#[test]
fn no_next_grant_unless_eligible_and_listed() {
    let unlisted = usage_with_block(two_grants(json!("zzz"), true));
    assert_eq!(unlisted.next_reset_grant, None, "id not in the list");
    let null = usage_with_block(two_grants(json!(null), true));
    assert_eq!(null.next_reset_grant, None, "null next_grant_id");
    let mut absent = two_grants(json!("g2"), true);
    absent.as_object_mut().unwrap().remove("next_grant_id");
    assert_eq!(usage_with_block(absent).next_reset_grant, None, "absent next_grant_id");
    let ineligible = usage_with_block(two_grants(json!("g2"), false));
    assert_eq!(ineligible.next_reset_grant, None, "account not eligible");
}

fn g2(left: u32, paused: bool, ends_at: &str) -> ResetGrant {
    ResetGrant {
        id: "g2".into(),
        paused,
        clears: vec!["five_hour".into()],
        label: "b".into(),
        resets_left: left,
        resets_total: 2,
        ends_at: Some(ends_at.into()),
        usable_now: true,
    }
}

#[test]
fn the_usable_grant_is_the_named_one_with_resets_left() {
    let now = chrono::Utc::now().timestamp();
    let grants = [g2(2, false, "2099-10-22T16:00:00+00:00")];
    assert_eq!(usable_reset_grant(Some(&grants), Some("g2"), now), Some(&grants[0]));
}

#[test]
fn no_usable_grant_when_it_is_spent_paused_ended_or_not_named() {
    let now = chrono::Utc::now().timestamp();
    let future = "2099-10-22T16:00:00+00:00";
    let spent = [g2(0, false, future)];
    assert_eq!(usable_reset_grant(Some(&spent), Some("g2"), now), None, "no resets left");
    let paused = [g2(2, true, future)];
    assert_eq!(usable_reset_grant(Some(&paused), Some("g2"), now), None, "paused");
    let ended = [g2(2, false, "2000-01-01T00:00:00+00:00")];
    assert_eq!(usable_reset_grant(Some(&ended), Some("g2"), now), None, "ended");
    let ok = [g2(2, false, future)];
    assert_eq!(usable_reset_grant(Some(&ok), None, now), None, "no next id");
    assert_eq!(usable_reset_grant(Some(&ok), Some("other"), now), None, "other id");
    assert_eq!(usable_reset_grant(None, Some("g2"), now), None, "no grants");
}

#[test]
fn request_ids_are_32_hex_characters_and_differ() {
    let (a, b) = (new_request_id(), new_request_id());
    for id in [&a, &b] {
        assert_eq!(id.len(), 32, "id: {id:?}");
        assert!(id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')), "id: {id:?}");
    }
    assert_ne!(a, b);
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
