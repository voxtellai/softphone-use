//! The control API under `/api`. MCP and the e2e runner talk to it.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::audio::Mode;
use crate::call::Call;
use crate::phone::Phone;

type P = State<Arc<Phone>>;

pub fn router(phone: Arc<Phone>) -> Router {
    Router::new()
        .route("/api/status", get(|State(p): P| async move { Json(p.status()) }))
        .route("/api/calls", get(|State(p): P| async move { Json(p.calls().iter().map(|c| c.snapshot()).collect::<Vec<_>>()) }))
        .route("/api/wait", post(wait))
        .route("/api/dial", post(dial))
        .route("/api/calls/{id}", get(|s: P, id: Path<String>| with_call(s, id, |_, c| Json(c.snapshot()).into_response())))
        .route(
            "/api/calls/{id}/levels",
            get(|s: P, id: Path<String>| {
                with_call(s, id, |_, c| {
                    let (i, o) = c.levels();
                    Json(json!({"in": i, "out": o})).into_response()
                })
            }),
        )
        .route("/api/calls/{id}/sip", get(|s: P, id: Path<String>| with_call(s, id, |p, c| Json(c.sip(&p.trace)).into_response())))
        .route("/api/calls/{id}/recording/{dir}", get(recording))
        .route("/api/calls/{id}/answer", post(answer))
        .route("/api/calls/{id}/hangup", post(hangup))
        .route("/api/sip", get(recent_sip))
        .with_state(phone)
}

fn error(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({"error": msg.to_string()}))).into_response()
}

async fn with_call(State(p): P, Path(id): Path<String>, f: impl FnOnce(&Arc<Phone>, Arc<Call>) -> Response) -> Response {
    match p.call(&id) {
        Some(c) => f(&p, c),
        None => error(StatusCode::NOT_FOUND, "unknown call"),
    }
}

/// The body of answer and dial: a mode and, for ai, optional instructions and voice.
#[derive(Default, Deserialize)]
struct ModeRequest {
    #[serde(default)]
    to: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    instructions: String,
    #[serde(default)]
    voice: String,
}

impl ModeRequest {
    fn mode(&self) -> anyhow::Result<Mode> {
        if self.mode.is_empty() { Ok(Mode::Silent) } else { Mode::parse(&self.mode) }
    }
}

/// A missing or malformed body counts as empty, as the Go API decoded it.
fn body(b: &axum::body::Bytes) -> ModeRequest {
    serde_json::from_slice(b).unwrap_or_default()
}

#[derive(Deserialize)]
struct WaitQuery {
    timeout: Option<u64>,
}

async fn wait(State(p): P, Query(q): Query<WaitQuery>) -> Response {
    match p.wait_for_call(Duration::from_secs(q.timeout.unwrap_or(60))).await {
        Ok(c) => Json(c.snapshot()).into_response(),
        Err(e) => error(StatusCode::REQUEST_TIMEOUT, e),
    }
}

async fn dial(State(p): P, b: axum::body::Bytes) -> Response {
    let b = body(&b);
    if b.to.is_empty() {
        return error(StatusCode::BAD_REQUEST, "to is required");
    }
    let mode = match b.mode() {
        Ok(m) => m,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    match p.dial(&b.to, mode, &b.instructions, &b.voice) {
        Ok(c) => Json(c.snapshot()).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, e),
    }
}

async fn answer(State(p): P, Path(id): Path<String>, b: axum::body::Bytes) -> Response {
    let Some(c) = p.call(&id) else { return error(StatusCode::NOT_FOUND, "unknown call") };
    let b = body(&b);
    let mode = match b.mode() {
        Ok(m) => m,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    p.set_ai(&c, &b.instructions, &b.voice);
    match p.answer(&id, mode) {
        Ok(()) => Json(c.snapshot()).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

async fn hangup(State(p): P, Path(id): Path<String>) -> Response {
    let Some(c) = p.call(&id) else { return error(StatusCode::NOT_FOUND, "unknown call") };
    match p.hangup(&id, "api") {
        Ok(()) => Json(c.snapshot()).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

async fn recording(State(p): P, Path((id, dir)): Path<(String, String)>) -> Response {
    let Some(c) = p.call(&id) else { return error(StatusCode::NOT_FOUND, "unknown call") };
    let snap = c.snapshot();
    let path = if dir == "out" { snap.wav_out } else { snap.wav_in };
    if path.is_empty() {
        return error(StatusCode::NOT_FOUND, "no recording");
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "audio/wav")], bytes).into_response(),
        Err(e) => error(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct SipQuery {
    n: Option<usize>,
}

async fn recent_sip(State(p): P, Query(q): Query<SipQuery>) -> Response {
    Json(p.trace.recent(q.n.filter(|n| *n > 0).unwrap_or(50))).into_response()
}
