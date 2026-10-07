//! A scripted in-memory LSP client for tests.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId};
use serde_json::{Value, json};

pub struct Client {
    pub conn: Connection,
    next_id: i32,
    notifications: Vec<Notification>,
    _server: std::thread::JoinHandle<()>,
}

impl Client {
    pub fn start(init_options: Value) -> Client {
        let (server, conn) = Connection::memory();
        let handle = std::thread::spawn(move || {
            pliron_lsp::server::run(server).unwrap();
        });
        let mut c = Client {
            conn,
            next_id: 0,
            notifications: Vec::new(),
            _server: handle,
        };
        c.request(
            "initialize",
            json!({
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "window": { "workDoneProgress": true },
                    "workspace": {
                        "semanticTokens": { "refreshSupport": true },
                        "inlayHint": { "refreshSupport": true },
                        "didChangeWatchedFiles": { "dynamicRegistration": true }
                    }
                },
                "initializationOptions": init_options,
            }),
        );
        c.notify("initialized", json!({}));
        c
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.conn
            .sender
            .send(Message::Notification(Notification::new(method.into(), params)))
            .unwrap();
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = RequestId::from(self.next_id);
        self.conn
            .sender
            .send(Message::Request(Request::new(id.clone(), method.into(), params)))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let msg = self
                .conn
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("response timeout");
            match msg {
                Message::Response(r) if r.id == id => {
                    if let Some(e) = r.error {
                        panic!("{method} failed: {e:?}");
                    }
                    return r.result.unwrap_or(Value::Null);
                }
                Message::Notification(n) => self.notifications.push(n),
                _ => {}
            }
        }
    }

    /// Wait for a `publishDiagnostics` for URI satisfying `pred`.
    pub fn wait_diagnostics(&mut self, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            while let Some(i) = self
                .notifications
                .iter()
                .position(|n| n.method == "textDocument/publishDiagnostics")
            {
                let n = self.notifications.remove(i);
                let diags = n.params["diagnostics"].as_array().cloned().unwrap_or_default();
                if pred(&diags) {
                    return diags;
                }
            }
            let msg = self
                .conn
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("diagnostics timeout");
            if let Message::Notification(n) = msg {
                self.notifications.push(n);
            }
        }
    }

    pub fn open_uri(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "pliron", "version": 1, "text": text } }),
        );
    }

    pub fn change_uri(&self, uri: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }
}

