// What the Reset column says for each state of Claude's session-limit reset,
// and that the usage request asks for that state (`at_wall=1`).

use std::sync::{Arc, Mutex};

use axum::{Router, extract::RawQuery, routing::get};
use claude_switch::claude_api::{Endpoints, SessionReset, fetch_usage};
use claude_switch::claude_usage::session_reset_label;
use serde_json::json;

fn reset(eligible: bool, reason: Option<&str>, available: bool, next: Option<&str>, per_week: u32) -> SessionReset {
    SessionReset {
        eligible,
        ineligible_reason: reason.map(str::to_owned),
        available,
        next_available_at: next.map(str::to_owned),
        resets_per_week: per_week,
        billing_period: None,
    }
}

#[test]
fn not_at_the_wall_shows_how_many_resets_are_left() {
    assert_eq!(session_reset_label(Some(&reset(false, Some("not_at_wall"), false, None, 1))).0, "1");
    assert_eq!(session_reset_label(Some(&reset(false, Some("not_at_wall"), false, None, 2))).0, "2");
}

/// Outside Claude Code the server answers "surface" and says nothing about
/// whether this week's reset was used, so the count is unknown, not 1.
#[test]
fn asked_from_outside_claude_code_the_count_is_unknown() {
    assert_eq!(session_reset_label(Some(&reset(false, Some("surface"), false, None, 1))).0, "?");
}

#[test]
fn an_offer_ready_now_says_ready() {
    let (text, ready) = session_reset_label(Some(&reset(true, None, true, None, 1)));
    assert_eq!(text, "1 ready");
    assert!(ready);
}

/// A reset already used this week: none left, and when the next one comes,
/// whatever the eligibility says. A next time in the past means it is back.
#[test]
fn a_used_reset_shows_zero_and_when_the_next_one_comes() {
    let used = session_reset_label(Some(&reset(true, None, false, Some("2099-10-12T09:00:00Z"), 1))).0;
    assert!(used.starts_with("0 \u{2192} 10-1"), "{used}");
    let before_wall =
        session_reset_label(Some(&reset(false, Some("not_at_wall"), false, Some("2099-10-12T09:00:00Z"), 1))).0;
    assert!(before_wall.starts_with("0 \u{2192} 10-1"), "{before_wall}");
    let back = session_reset_label(Some(&reset(false, Some("not_at_wall"), false, Some("2000-01-01T00:00:00Z"), 1))).0;
    assert_eq!(back, "1");
}

#[test]
fn an_account_that_cannot_get_resets_says_na() {
    assert_eq!(session_reset_label(Some(&reset(false, Some("tier"), false, None, 1))).0, "n/a");
    assert_eq!(session_reset_label(Some(&reset(false, Some("weekly_limit"), false, None, 1))).0, "n/a");
}

#[test]
fn no_data_is_a_dash() {
    assert_eq!(session_reset_label(None).0, "--");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_usage_request_asks_for_the_reset_state() {
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

    let queries = seen.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    let query = queries[0].clone().unwrap_or_default();
    assert!(query.split('&').any(|part| part == "at_wall=1"), "query: {query}");
}
