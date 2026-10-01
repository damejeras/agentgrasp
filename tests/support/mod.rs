//! A fake TypeSafe server for the tests. It speaks just enough HTTP/1.1 for reqwest.

#![allow(dead_code)]

pub mod mcp;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Reply {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
    pub headers: Vec<(String, String)>,
    /// The reply is sent only once this gate opens.
    pub gate: Option<Gate>,
}

/// Holds replies until the test opens it, so a test can order events without timing.
#[derive(Clone, Default)]
pub struct Gate(Arc<tokio::sync::Notify>, Arc<std::sync::atomic::AtomicBool>);

impl Gate {
    pub fn open(&self) {
        self.1.store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify_waiters();
    }

    async fn passed(&self) {
        loop {
            let notified = self.0.notified();
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

impl Reply {
    pub fn status(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            headers: Vec::new(),
            gate: None,
        }
    }

    /// A valid answer set: every question of `request` gets `p(instructions)`.
    pub fn answers(request: &Value, p: impl Fn(&str) -> f64) -> Reply {
        let answers: serde_json::Map<_, _> = request["questions"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, q)| {
                let p = p(q["instructions"].as_str().unwrap());
                (key.clone(), json!({"type": "noul", "noul": p}))
            })
            .collect();
        Reply::status(
            200,
            json!({
                "model": "jev-fake",
                "answers": answers,
                "usage": {"input_tokens": 10, "output_tokens": 2},
            })
            .to_string(),
        )
    }

    pub fn delayed(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }

    /// The reply waits for `gate`.
    pub fn held(mut self, gate: &Gate) -> Reply {
        self.gate = Some(gate.clone());
        self
    }

    /// A reply that never comes in a test's lifetime.
    pub fn never(self) -> Reply {
        self.delayed(Duration::from_secs(3600))
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }
}

pub struct Seen {
    pub authorization: String,
    pub body: Value,
}

type Handler = dyn Fn(usize, &Value) -> Reply + Send + Sync;

pub struct FakeJev {
    pub url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeJev {
    /// `handler` gets the index of the request (from 0) and its JSON body.
    pub async fn start(
        handler: impl Fn(usize, &Value) -> Reply + Send + Sync + 'static,
    ) -> FakeJev {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let Some((authorization, body)) = read_request(&mut socket).await else {
                        return;
                    };
                    let index = {
                        let mut log = log.lock().unwrap();
                        log.push(Seen {
                            authorization,
                            body: body.clone(),
                        });
                        log.len() - 1
                    };
                    let reply = handler(index, &body);
                    if let Some(gate) = &reply.gate {
                        gate.passed().await;
                    }
                    tokio::time::sleep(reply.delay).await;
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (name, value) in &reply.headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(reply.body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        FakeJev { url, seen }
    }

    /// Waits until the server has received `count` requests.
    pub async fn received(&self, count: usize) {
        let started = std::time::Instant::now();
        while self.requests() < count {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the server got no request"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub fn body(&self, index: usize) -> Value {
        self.seen.lock().unwrap()[index].body.clone()
    }

    pub fn authorization(&self, index: usize) -> String {
        self.seen.lock().unwrap()[index].authorization.clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, Value)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 65536];
    let header_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let length: usize = header("content-length")?.parse().ok()?;
    while buffer.len() < header_end + length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buffer[header_end..header_end + length]).ok()?;
    Some((header("authorization").unwrap_or_default(), body))
}
