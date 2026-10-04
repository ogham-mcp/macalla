// SPDX-License-Identifier: MIT
//! A fake HTTP server, so a Tier 1 provider's tests exercise the real request
//! and reply parsing without ever touching the network. Shared by
//! [`crate::endpoint`] and [`crate::anthropic`].
#![cfg(test)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};

use macalla_core::block::Block;
use serde_json::Value;

/// Answer one request on `listener` with `status` and `body`, and hand back
/// what arrived: the request head, lower-cased, and the JSON body.
pub fn answer(listener: &TcpListener, status: &str, body: &str) -> (String, Value) {
    let (mut stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let (mut head, mut length) = (String::new(), 0);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        let line = line.to_ascii_lowercase();
        if let Some(value) = line.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        head.push_str(&line);
    }
    let mut sent = vec![0; length];
    reader.read_exact(&mut sent).unwrap();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    // A GET has no body at all; only a POST/PUT sends JSON to parse.
    let sent = if sent.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&sent).unwrap()
    };
    (head, sent)
}

/// A server that answers one request.
pub fn serve_once(status: &str, body: String) -> (String, JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let status = status.to_string();
    let server = thread::spawn(move || answer(&listener, &status, &body));
    (address, server)
}

/// A server that answers requests in turn, one reply each.
pub fn serve(replies: Vec<(&str, String)>) -> (String, JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let replies: Vec<(String, String)> = replies
        .into_iter()
        .map(|(status, body)| (status.to_string(), body))
        .collect();
    let server = thread::spawn(move || {
        replies
            .iter()
            .map(|(status, body)| answer(&listener, status, body))
            .collect()
    });
    (address, server)
}

/// A one-item ordered list block, the shape the tests exercise Tier 1 with.
pub fn item(text: &str) -> Vec<Block> {
    vec![Block::ListItem {
        text: text.into(),
        ordered: true,
    }]
}

/// One event a [`Capture`] subscriber recorded: its level, its own message
/// (the `"..."` literal every `tracing::info!`/`tracing::debug!` call
/// carries), and every other field as a debug-formatted string.
pub struct CapturedEvent {
    pub level: tracing::Level,
    pub message: String,
    pub fields: HashMap<String, String>,
}

impl tracing::field::Visit for CapturedEvent {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.insert(field.name().to_string(), text);
        }
    }
}

/// A minimal `tracing::Subscriber` that records every event's fields as
/// strings, so a test can assert on what a log line carried without a new
/// dev-dependency: `tracing` is already this crate's own dependency, and
/// every `Visit` method but `record_debug` defaults to calling it, so
/// overriding just that one captures every field shape [`crate::endpoint`]
/// and [`crate::anthropic`] log (`%model`, a bare `status`, and
/// `Option<&str>` detail fields). Shared by both, since a refused request's
/// own-message-at-debug-only shape is the same test in either module.
pub struct Capture {
    pub events: std::sync::Arc<Mutex<Vec<CapturedEvent>>>,
}

/// Run `f` with `capture` as this thread's subscriber, so a test sees every
/// event `f` logs.
///
/// Not just `tracing::subscriber::with_default`. Whether a callsite logs at
/// all is cached globally, per callsite. When only one dispatcher is
/// registered, tracing-core rebuilds that cache with the default dispatcher of
/// whichever thread asks, and another test's thread, with no subscriber, sees
/// `NoSubscriber` and caches the callsite as never interested, even while this
/// capture is live. The test then receives no events at all. That was seen:
/// the retry ran (two requests) at TRACE on this thread and nothing arrived,
/// 5 runs in 50 of the whole crate and never alone. A rebuild inside the
/// scope did not help, because a later rebuild from another thread undid it.
/// So every test thread gets an interested dispatcher: [`Discard`], set once as
/// the global default. It drops every event, as `NoSubscriber` did, but it
/// never lets a callsite be cached as never.
pub fn with_capture<R>(capture: Capture, f: impl FnOnce() -> R) -> R {
    static GLOBAL: std::sync::Once = std::sync::Once::new();
    GLOBAL.call_once(|| {
        // Only fails if a global default is already set, which nothing in
        // this crate's tests does; either way the cache then has an
        // interested dispatcher to consult.
        let _ = tracing::subscriber::set_global_default(Discard);
    });
    tracing::subscriber::with_default(capture, f)
}

/// Interested in every callsite, keeps nothing: see [`with_capture`].
pub struct Discard;

impl tracing::Subscriber for Discard {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut captured = CapturedEvent {
            level: *event.metadata().level(),
            message: String::new(),
            fields: HashMap::new(),
        };
        event.record(&mut captured);
        self.events.lock().unwrap().push(captured);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
