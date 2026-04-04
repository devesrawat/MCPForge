//! Legacy SSE transport (MCP 2024-11-05 §3.2).
//!
//! Protocol:
//!  1. Client opens `GET /sse` — server sends `event: endpoint` with the
//!     URI the client should POST to (`/messages?session_id=<uuid>`).
//!  2. Client POSTs JSON-RPC requests to `/messages?session_id=<uuid>`.
//!  3. Server dispatches each request, serialises the JSON-RPC response,
//!     and pushes it back over the SSE stream as `event: message`.
//!
//! Concurrent sessions are tracked in a `DashMap` keyed by session UUID.
//! Dropping the SSE stream (on disconnect or completion) cleans up the
//! session entry via a [`SessionGuard`] held inside the stream state.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender, channel, error::TrySendError};
use uuid::Uuid;

use crate::{JsonRpcRequest, ProxyAppState, dispatch_request};

/// Bounded capacity for each SSE session channel.
/// Prevents unbounded memory growth for slow or disconnected clients.
const SSE_CHANNEL_CAPACITY: usize = 64;

/// Per-session SSE channel map: session_id → bounded sender for SSE events.
pub type SessionStore = Arc<DashMap<String, Sender<String>>>;

#[derive(Deserialize)]
pub struct SessionQuery {
    pub session_id: Option<String>,
}

/// RAII guard that removes a session from the store when dropped.
///
/// Owned by the SSE message stream so it is cleaned up whether the stream
/// completes normally or is dropped mid-way (client disconnect).
struct SessionGuard {
    session_id: String,
    sessions: SessionStore,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.sessions.remove(&self.session_id);
    }
}

/// `GET /sse` — open a new SSE session.
///
/// The handler immediately sends an `endpoint` event so the client knows
/// where to POST requests, then streams `message` events for each response.
pub async fn handle_sse_connect(
    State(state): State<ProxyAppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let session_id = Uuid::new_v4().to_string();
    let (tx, rx): (Sender<String>, Receiver<String>) = channel(SSE_CHANNEL_CAPACITY);
    state.sessions.insert(session_id.clone(), tx);

    let endpoint_event = Event::default()
        .event("endpoint")
        .data(format!("/messages?session_id={}", session_id));

    // The guard is owned by the stream state: it is dropped when the stream
    // is either exhausted or dropped (client disconnect), guaranteeing cleanup.
    let guard = SessionGuard {
        session_id,
        sessions: state.sessions.clone(),
    };

    // Convert the bounded receiver into a stream of SSE `message` events.
    // The guard travels with the receiver so both are dropped together.
    // When `rx.recv()` returns `None` (channel closed or sender dropped),
    // the closure returns `None`, ending iteration and dropping `(rx, guard)`.
    // The `Drop` impl on `SessionGuard` then removes the session entry,
    // guaranteeing cleanup whether the stream ends normally or is dropped
    // mid-way (client disconnect).
    let message_stream = stream::unfold((rx, guard), |(mut rx, guard)| async move {
        rx.recv().await.map(|data| {
            let event = Event::default().event("message").data(data);
            (event, (rx, guard))
        })
    });

    let combined = stream::once(async { endpoint_event }).chain(message_stream);
    let sse_stream = combined.map(Ok::<_, std::convert::Infallible>);

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

/// `POST /messages?session_id=<id>` — receive a JSON-RPC request from
/// the client and route the response back over the SSE channel.
///
/// Returns 202 Accepted immediately; the actual JSON-RPC response travels
/// through the SSE stream opened by `GET /sse`.
pub async fn handle_sse_message(
    State(state): State<ProxyAppState>,
    Query(q): Query<SessionQuery>,
    axum::Json(request): axum::Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let Some(session_id) = q.session_id else {
        return (
            StatusCode::BAD_REQUEST,
            "missing session_id query parameter",
        )
            .into_response();
    };

    let Some(tx) = state.sessions.get(&session_id).map(|e| e.clone()) else {
        return (
            StatusCode::NOT_FOUND,
            format!("unknown session_id '{}'", session_id),
        )
            .into_response();
    };

    let id = request.id.clone();
    let response = match dispatch_request(&state, request).await {
        Ok(result) => crate::JsonRpcResponse::success(result, id),
        Err(err) => crate::JsonRpcResponse::error(err.code(), err.to_string(), id),
    };

    let json = match serde_json::to_string(&response) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("sse: failed to serialize response: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    match tx.try_send(json) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                "sse: session '{}' backpressure — channel full, client is too slow",
                session_id
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "SSE backpressure: client is too slow",
            )
                .into_response();
        }
        Err(TrySendError::Closed(_)) => {
            tracing::warn!(
                "sse: session '{}' channel closed before response could be sent",
                session_id
            );
            return (StatusCode::GONE, "SSE session has ended").into_response();
        }
    }

    StatusCode::ACCEPTED.into_response()
}
