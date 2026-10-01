//! Client for TypeSafe's System One endpoint, which answers yes/no (`noul`) questions with Jev.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const MODEL: &str = "jev-latest";
/// The longest one HTTP attempt can take, as in jevgrep's evaluator for TypeSafe.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const ATTEMPTS: usize = 2;
/// The wait before a retry when the server does not send `retry-after`, as in jevgrep.
const RETRY_WAIT: Duration = Duration::from_secs(1);
/// TypeSafe's status for an overloaded server.
const OVERLOADED: u16 = 529;

pub struct Question {
    pub key: String,
    pub instructions: String,
}

/// Which retry rule applies. jevgrep gives a navigation request with several questions one
/// attempt on 529, because its caller splits the batch instead.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    Standard,
    SplitOnOverload,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One HTTP request, for the usage and latency that every record keeps.
#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub usage: Option<Usage>,
}

#[derive(Debug)]
pub struct Answers {
    pub model: String,
    /// One probability of yes per question, in question order.
    pub probabilities: Vec<f64>,
    pub usage: Usage,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Unauthorized,
    TooLarge,
    /// A 529 under `Retry::SplitOnOverload`: the caller can split the batch.
    Overloaded,
    Unavailable {
        status: Option<u16>,
    },
    TimedOut,
    InvalidResponse,
    Cancelled,
}

impl Failure {
    /// The error code of the MCP tools.
    pub fn code(&self) -> &'static str {
        match self {
            Failure::TooLarge => "input_too_large",
            Failure::InvalidResponse => "invalid_provider_response",
            Failure::Cancelled => "cancelled",
            Failure::Unauthorized
            | Failure::Overloaded
            | Failure::Unavailable { .. }
            | Failure::TimedOut => "provider_unavailable",
        }
    }
}

// The text never holds a response body: a body can echo the request, which holds file content.
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Unauthorized => f.write_str("TypeSafe rejected the API key"),
            Failure::TooLarge => f.write_str("the input is larger than Jev accepts"),
            Failure::Overloaded => f.write_str("TypeSafe is overloaded (HTTP 529)"),
            Failure::Unavailable {
                status: Some(status),
            } => {
                write!(f, "TypeSafe returned HTTP {status}")
            }
            Failure::Unavailable { status: None } => f.write_str("TypeSafe cannot be reached"),
            Failure::TimedOut => f.write_str("TypeSafe did not answer in time"),
            Failure::InvalidResponse => f.write_str("TypeSafe returned an invalid answer set"),
            Failure::Cancelled => f.write_str("the request was cancelled"),
        }
    }
}

pub struct Outcome {
    pub result: Result<Answers, Failure>,
    pub attempts: Vec<Attempt>,
}

pub struct Client {
    http: reqwest::Client,
    endpoint: String,
    key: String,
}

impl Client {
    pub fn new(endpoint: impl Into<String>, key: impl Into<String>) -> Client {
        Client {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            key: key.into(),
        }
    }

    /// Asks every question about `state` in one request. Each answer is checked: every key is
    /// present once, nothing else is, and each value is a `noul` probability in [0, 1].
    pub async fn evaluate(
        &self,
        state: &Value,
        questions: &[Question],
        retry: Retry,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Outcome {
        let body = json!({
            "model": MODEL,
            "state": state,
            "questions": questions
                .iter()
                .map(|q| (q.key.clone(), json!({"type": "noul", "instructions": q.instructions})))
                .collect::<serde_json::Map<_, _>>(),
        });
        let mut attempts = Vec::new();
        for attempt in 0..ATTEMPTS {
            let started = Instant::now();
            let Some(left) = deadline
                .checked_duration_since(started)
                .filter(|d| !d.is_zero())
            else {
                return Outcome {
                    result: Err(Failure::TimedOut),
                    attempts,
                };
            };
            let send = self
                .http
                .post(&self.endpoint)
                .bearer_auth(&self.key)
                .json(&body)
                .timeout(left.min(REQUEST_TIMEOUT))
                .send();
            let reply = tokio::select! {
                _ = cancel.cancelled() => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    attempts.push(Attempt { status: None, latency_ms, usage: None });
                    return Outcome { result: Err(Failure::Cancelled), attempts };
                }
                reply = async {
                    let response = send.await?;
                    let status = response.status();
                    let retry_after = retry_after(response.headers());
                    let bytes = response.bytes().await?;
                    Ok::<_, reqwest::Error>((status, retry_after, bytes))
                } => reply,
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            let (status, retry_after, bytes) = match reply {
                Ok(reply) => reply,
                Err(error) => {
                    attempts.push(Attempt {
                        status: None,
                        latency_ms,
                        usage: None,
                    });
                    let failure = if error.is_timeout() {
                        Failure::TimedOut
                    } else {
                        Failure::Unavailable { status: None }
                    };
                    return Outcome {
                        result: Err(failure),
                        attempts,
                    };
                }
            };
            let code = status.as_u16();
            if status == StatusCode::OK {
                attempts.push(Attempt {
                    status: Some(code),
                    latency_ms,
                    usage: reported_usage(&bytes),
                });
                let result = parse(&bytes, questions);
                return Outcome { result, attempts };
            }
            attempts.push(Attempt {
                status: Some(code),
                latency_ms,
                usage: None,
            });
            let last = attempt + 1 == ATTEMPTS;
            let failure = match code {
                401 | 403 => Failure::Unauthorized,
                400 if is_too_large(&bytes) => Failure::TooLarge,
                429 if !last => {
                    if !wait(retry_after.unwrap_or(RETRY_WAIT), deadline, cancel).await {
                        return Outcome {
                            result: Err(stopped(cancel)),
                            attempts,
                        };
                    }
                    continue;
                }
                OVERLOADED if retry == Retry::SplitOnOverload => Failure::Overloaded,
                OVERLOADED if !last => {
                    if !wait(retry_after.unwrap_or(RETRY_WAIT), deadline, cancel).await {
                        return Outcome {
                            result: Err(stopped(cancel)),
                            attempts,
                        };
                    }
                    continue;
                }
                _ => Failure::Unavailable { status: Some(code) },
            };
            return Outcome {
                result: Err(failure),
                attempts,
            };
        }
        unreachable!("the last attempt always returns")
    }
}

fn stopped(cancel: &CancellationToken) -> Failure {
    if cancel.is_cancelled() {
        Failure::Cancelled
    } else {
        Failure::TimedOut
    }
}

/// Waits before a retry. False when the deadline would pass first or the call is cancelled.
async fn wait(duration: Duration, deadline: Instant, cancel: &CancellationToken) -> bool {
    if duration >= deadline.saturating_duration_since(Instant::now()) {
        return false;
    }
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

/// `retry-after` in seconds. An HTTP date is not used: TypeSafe sends seconds.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let text = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: f64 = text.trim().parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

fn is_too_large(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Body {
        detail: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        error_type: String,
    }
    serde_json::from_slice::<Body>(body)
        .is_ok_and(|body| body.detail.error_type == "max_tokens_exceeded")
}

/// The usage of a 200 response, also when its answers are invalid: the request was billed.
fn reported_usage(body: &[u8]) -> Option<Usage> {
    #[derive(Deserialize)]
    struct Body {
        usage: Usage,
    }
    serde_json::from_slice::<Body>(body)
        .ok()
        .map(|body| body.usage)
}

/// A JSON object whose keys must be unique. serde_json keeps the last of two equal keys, which
/// would hide a conflicting answer.
struct UniqueMap<V>(HashMap<String, V>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for UniqueMap<V> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<V> {
            type Value = UniqueMap<V>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object with unique keys")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    if entries.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(UniqueMap(entries))
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

fn parse(body: &[u8], questions: &[Question]) -> Result<Answers, Failure> {
    #[derive(Deserialize)]
    struct Response {
        model: String,
        answers: UniqueMap<Answer>,
        usage: Usage,
    }
    #[derive(Deserialize)]
    struct Answer {
        #[serde(rename = "type")]
        kind: String,
        noul: f64,
    }
    let response: Response = serde_json::from_slice(body).map_err(|_| Failure::InvalidResponse)?;
    let answers = response.answers.0;
    let keys: HashSet<&str> = questions.iter().map(|q| q.key.as_str()).collect();
    if answers.len() != keys.len() || !answers.keys().all(|key| keys.contains(key.as_str())) {
        return Err(Failure::InvalidResponse);
    }
    let probabilities = questions
        .iter()
        .map(|q| {
            let answer = &answers[&q.key];
            let valid = answer.kind == "noul" && (0.0..=1.0).contains(&answer.noul);
            valid.then_some(answer.noul).ok_or(Failure::InvalidResponse)
        })
        .collect::<Result<_, _>>()?;
    Ok(Answers {
        model: response.model,
        probabilities,
        usage: response.usage,
    })
}
