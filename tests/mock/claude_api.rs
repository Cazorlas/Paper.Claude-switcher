use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use claude_switch::claude_api::Endpoints;
use serde_json::Value;

use super::MockResponse;

#[derive(Default, Clone)]
pub struct Calls {
    pub usage_headers: Vec<HeaderMap>,
    pub token_bodies: Vec<Value>,
    pub credentials_at_usage: Vec<Value>,
}

#[derive(Clone)]
struct ServerState {
    usage_reply: MockResponse,
    token_reply: MockResponse,
    retry_after: Option<String>,
    credentials_path: Option<PathBuf>,
    calls: Arc<Mutex<Calls>>,
}

pub struct ClaudeServer {
    pub endpoints: Endpoints,
    calls: Arc<Mutex<Calls>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ClaudeServer {
    pub async fn start(
        usage_reply: MockResponse,
        token_reply: MockResponse,
        retry_after: Option<&str>,
        credentials_path: Option<PathBuf>,
    ) -> Self {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let state = ServerState {
            usage_reply,
            token_reply,
            retry_after: retry_after.map(str::to_owned),
            credentials_path,
            calls: calls.clone(),
        };
        let app = Router::new()
            .route("/api/oauth/usage", get(usage_handler))
            .route("/v1/oauth/token", post(token_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async { let _ = stopped.await; })
                .await
                .unwrap();
        });
        Self {
            endpoints: Endpoints {
                api_base: base.clone(),
                token_url: format!("{base}/v1/oauth/token"),
            },
            calls,
            shutdown: Some(shutdown),
        }
    }

    pub fn calls(&self) -> Calls {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for ClaudeServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn response(reply: MockResponse) -> Response {
    match reply {
        MockResponse::Json(status, body) => (status, axum::Json(body)).into_response(),
        MockResponse::Text(status, body) => (status, body).into_response(),
    }
}

async fn usage_handler(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let mut calls = state.calls.lock().unwrap();
    calls.usage_headers.push(headers);
    if let Some(path) = state.credentials_path {
        let credentials = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        calls.credentials_at_usage.push(credentials);
    }
    let mut reply = response(state.usage_reply);
    if let Some(retry_after) = state.retry_after {
        reply.headers_mut().insert("retry-after", retry_after.parse().unwrap());
    }
    reply
}

async fn token_handler(
    State(state): State<ServerState>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    state.calls.lock().unwrap().token_bodies.push(body);
    response(state.token_reply)
}
