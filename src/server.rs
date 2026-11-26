//! The API the SwiftUI front end drives SwiftSpot through.
//!
//! The engine owns the Spotify session, playback, the queue and the
//! library; the front end asks it for what to show and tells it what the
//! person did. Both live in one process: the front end hands over requests
//! through [`Inbox::submit`] (see `crate::ffi`) and reads one JSON object
//! per message back:
//!
//! - a request: `{"id": 1, "method": "playlist.get", "params": {"id": "…"}}`
//! - its reply: `{"id": 1, "result": …}` or `{"id": 1, "error": "…"}`
//! - an event the engine pushes unasked: `{"event": "player", "data": …}`
//!
//! Requests queue up for the app's next pass, which answers them
//! (`App::serve_api`) and hands each reply to the front end's channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::backend::Waker;

/// A request waiting for the app's next pass.
pub struct Request {
    pub client: u64,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(serde::Deserialize)]
struct Incoming {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Default)]
struct Shared {
    requests: Vec<Request>,
    /// Each front end's outgoing messages.
    clients: Vec<(u64, Sender<String>)>,
}

/// What the front ends asked for, and where their answers go.
#[derive(Clone, Default)]
pub struct Server {
    shared: Arc<Mutex<Shared>>,
}

/// A front end's side of the API: requests go in here.
pub struct Inbox {
    shared: Arc<Mutex<Shared>>,
    waker: Waker,
    client: u64,
}

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

impl Inbox {
    /// Queues one JSON request for the app's next pass and wakes it.
    pub fn submit(&self, line: &[u8]) {
        let Ok(incoming) = serde_json::from_slice::<Incoming>(line) else {
            log::warn!("API: ignored a message that is not a request");
            return;
        };
        if let Ok(mut shared) = self.shared.lock() {
            shared.requests.push(Request {
                client: self.client,
                id: incoming.id,
                method: incoming.method,
                params: incoming.params,
            });
        }
        self.waker.wake();
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.clients.retain(|(each, _)| *each != self.client);
        }
    }
}

impl Server {
    /// Adds a front end. Its replies and events arrive on the receiver.
    pub fn connect(&self, waker: Waker) -> (Inbox, Receiver<String>) {
        let client = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        let (sender, messages) = channel();
        if let Ok(mut shared) = self.shared.lock() {
            shared.clients.push((client, sender));
        }
        waker.wake();
        (
            Inbox {
                shared: Arc::clone(&self.shared),
                waker,
                client,
            },
            messages,
        )
    }

    /// The requests that arrived since the last call.
    pub fn take_requests(&self) -> Vec<Request> {
        self.lock()
            .map_or_else(Vec::new, |mut shared| std::mem::take(&mut shared.requests))
    }

    pub fn has_clients(&self) -> bool {
        self.lock().is_some_and(|shared| !shared.clients.is_empty())
    }

    /// Answers a request with pre-serialized JSON bytes, avoiding a `Value` DOM round-trip.
    pub fn reply_raw(&self, request: &Request, result: Result<String, String>) {
        let text = match result {
            Ok(json) => {
                let id = serde_json::to_string(&request.id).unwrap_or_else(|_| "null".into());
                format!(r#"{{"id":{id},"result":{json}}}"#)
            }
            Err(error) => {
                let line = serde_json::json!({ "id": request.id, "error": error });
                serde_json::to_string(&line).unwrap_or_default()
            }
        };
        send_raw(&self.shared, Some(request.client), text);
    }

    /// Answers a request.
    pub fn reply(&self, request: &Request, result: Result<Value, String>) {
        let line = match result {
            Ok(result) => serde_json::json!({ "id": request.id, "result": result }),
            Err(error) => serde_json::json!({ "id": request.id, "error": error }),
        };
        self.send(Some(request.client), &line);
    }

    /// Answers a request later, from any thread: for an answer that needs
    /// the network.
    pub fn later(&self, request: &Request) -> impl FnOnce(Result<Value, String>) + Send + 'static {
        let shared = Arc::clone(&self.shared);
        let id = request.id.clone();
        let client = request.client;
        move |result| {
            let line = match result {
                Ok(result) => serde_json::json!({ "id": id, "result": result }),
                Err(error) => serde_json::json!({ "id": id, "error": error }),
            };
            send(&shared, Some(client), &line);
        }
    }

    /// Pushes a pre-serialized JSON event payload to every connection.
    pub fn broadcast_raw(&self, event: &str, data_json: &str) {
        let name = serde_json::to_string(event).unwrap_or_else(|_| "\"\"".into());
        send_raw(
            &self.shared,
            None,
            format!(r#"{{"event":{name},"data":{data_json}}}"#),
        );
    }

    /// Pushes an event to every connection.
    pub fn broadcast(&self, event: &str, data: Value) {
        self.send(None, &serde_json::json!({ "event": event, "data": data }));
    }

    fn send(&self, to: Option<u64>, line: &Value) {
        send(&self.shared, to, line);
    }

    fn lock(&self) -> Option<std::sync::MutexGuard<'_, Shared>> {
        self.shared.lock().ok()
    }
}

/// Writes a pre-serialized JSON line to one connection, or to every one.
fn send_raw(shared: &Mutex<Shared>, to: Option<u64>, text: String) {
    if let Ok(mut shared) = shared.lock() {
        shared.clients.retain(|(client, sender)| {
            to.is_some_and(|to| to != *client) || sender.send(text.clone()).is_ok()
        });
    }
}

/// Writes a line to one connection, or to every one.
fn send(shared: &Mutex<Shared>, to: Option<u64>, line: &Value) {
    let Ok(text) = serde_json::to_string(line) else {
        return;
    };
    send_raw(shared, to, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_front_end_gets_its_reply_and_the_events() {
        let server = Server::default();
        let (inbox, messages) = server.connect(Waker::default());
        inbox.submit(br#"{"id":7,"method":"player.state"}"#);
        let request = server.take_requests().pop().expect("a request");
        assert_eq!(request.method, "player.state");
        server.reply(&request, Ok(serde_json::json!({ "playing": true })));
        let reply: Value = serde_json::from_str(&messages.recv().unwrap()).unwrap();
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["playing"], true);

        server.broadcast("player", serde_json::json!({ "title": "Rosewood" }));
        let event: Value = serde_json::from_str(&messages.recv().unwrap()).unwrap();
        assert_eq!(event["event"], "player");
        assert_eq!(event["data"]["title"], "Rosewood");

        drop(inbox);
        assert!(!server.has_clients());
    }

    #[test]
    fn a_message_that_is_not_a_request_is_ignored() {
        let server = Server::default();
        let (inbox, _messages) = server.connect(Waker::default());
        inbox.submit(b"not json");
        assert!(server.take_requests().is_empty());
    }
}
