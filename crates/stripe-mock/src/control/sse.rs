//! Latest-state subscription over SSE and WebSocket (spec section 10.4).
//!
//! Both transports wrap the same [`Subscription`] the in-process API returns, with the
//! default options, and end when the mock shuts down.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use super::http::{ControlError, Query, mock, parse_filter};
use crate::SubscriptionItem;
use crate::handle::Shared;

/// How long a final close frame may take to send.
const CLOSE_GRACE: Duration = Duration::from_millis(500);
const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
pub(super) struct SubscribeQuery {
    types: Option<String>,
    account: Option<String>,
}

/// An item's frame name and its payload, if it has one. Shared by both transports.
fn frame(item: &SubscriptionItem) -> (&'static str, Option<Value>) {
    match item {
        SubscriptionItem::Update(u) => ("update", serde_json::to_value(u).ok()),
        SubscriptionItem::Synced => ("synced", None),
        SubscriptionItem::Lagged => ("lagged", None),
    }
}

/// Resolves once the mock has shut down.
async fn stopped(shared: Arc<Shared>) {
    let mut rx = shared.shutdown.subscribe();
    let _ = rx.wait_for(|stopped| *stopped).await;
}

/// `GET /control/subscribe`: server-sent events.
pub(super) async fn subscribe(
    State(s): State<Arc<Shared>>,
    Query(q): Query<SubscribeQuery>,
) -> Result<Response, ControlError> {
    let m = mock(s.clone());
    let filter = parse_filter(&m, q.types.as_deref(), q.account.as_deref())?;
    let events = m.subscribe(filter).map(|item| {
        let (name, payload) = frame(&item);
        let data = payload.unwrap_or_else(|| json!({})).to_string();
        Ok::<_, Infallible>(Event::default().event(name).data(data))
    });
    let events = events.take_until(stopped(s));
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE).text("keep-alive"))
        .into_response())
}

/// `GET /control/subscribe/ws`: one text frame per item.
pub(super) async fn subscribe_ws(
    State(s): State<Arc<Shared>>,
    Query(q): Query<SubscribeQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ControlError> {
    let m = mock(s.clone());
    let filter = parse_filter(&m, q.types.as_deref(), q.account.as_deref())?;
    let sub = m.subscribe(filter);
    Ok(upgrade.on_upgrade(move |socket| pump(socket, sub, s)))
}

async fn pump(mut socket: WebSocket, mut sub: crate::Subscription, shared: Arc<Shared>) {
    let stop = stopped(shared);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            item = sub.next() => {
                let Some(item) = item else { break };
                let (name, payload) = frame(&item);
                let text = match payload {
                    Some(update) => json!({"type": name, "update": update}),
                    None => json!({"type": name}),
                }
                .to_string();
                // Race the send against shutdown: a client that stopped reading
                // must not hold this task.
                tokio::select! {
                    sent = socket.send(Message::Text(text.into())) => {
                        if sent.is_err() {
                            return;
                        }
                    }
                    () = &mut stop => break,
                }
                if matches!(item, SubscriptionItem::Lagged) {
                    break;
                }
            }
            // Client messages are ignored; a close or error ends the stream.
            msg = socket.recv() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
            () = &mut stop => break,
        }
    }
    tokio::select! {
        _ = socket.send(Message::Close(None)) => {}
        // A stalled client must not hold the task: bound the close frame.
        () = tokio::time::sleep(CLOSE_GRACE) => {}
    }
}
