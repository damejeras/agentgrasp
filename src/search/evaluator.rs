// Ported from jevgrep (https://github.com/dzhng/jevgrep) at commit
// 82ef1fd3f43161bb17395dba1e07a5338f3913db: packages/core/src/evaluator.ts and
// rate-budget.ts. There is no answer cache.
// Copyright (c) David Zhang. MIT License; see LICENSE-jevgrep.

//! Jev requests within the budgets of one search.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::requests::Request;
use crate::jev::{self, Attempt, Failure, Retry};

/// The most HTTP requests one search makes.
pub const REQUEST_LIMIT: usize = 50_000;
/// The most requests in flight at once.
pub const CONCURRENCY: usize = 32;
pub const TOKENS_PER_SECOND: u64 = 250_000;
pub const REQUESTS_PER_MINUTE: usize = 1_200;
/// jevgrep's ceiling on the token estimate: Jev's largest accepted input.
pub const MAX_ESTIMATED_TOKENS: u64 = 65_536;

#[derive(Debug)]
pub enum EvalError {
    Unauthorized,
    BudgetExhausted,
    Cancelled,
    Provider {
        failure: Failure,
        /// A navigation batch that failed for a passing reason: the caller can split it.
        split: bool,
    },
}

/// The rolling request and token budgets.
#[derive(Default)]
struct RateBudget {
    starts: VecDeque<(Instant, Arc<Mutex<u64>>)>,
}

impl RateBudget {
    fn wait(&mut self, tokens: u64, now: Instant) -> Duration {
        while self
            .starts
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= Duration::from_secs(60))
        {
            self.starts.pop_front();
        }
        let mut wait = Duration::ZERO;
        if self.starts.len() >= REQUESTS_PER_MINUTE {
            let (at, _) = &self.starts[self.starts.len() - REQUESTS_PER_MINUTE];
            wait = (*at + Duration::from_secs(60)).saturating_duration_since(now);
        }
        let recent: Vec<(Instant, u64)> = self
            .starts
            .iter()
            .filter(|(at, _)| now.duration_since(*at) < Duration::from_secs(1))
            .map(|(at, t)| (*at, *t.lock().unwrap()))
            .collect();
        let mut total: u64 = recent.iter().map(|(_, t)| t).sum::<u64>() + tokens;
        for (at, entry) in recent {
            if total <= TOKENS_PER_SECOND {
                break;
            }
            total -= entry;
            wait = wait.max((at + Duration::from_secs(1)).saturating_duration_since(now));
        }
        wait
    }

    fn reserve(&mut self, tokens: u64, now: Instant) -> Arc<Mutex<u64>> {
        let entry = Arc::new(Mutex::new(tokens));
        self.starts.push_back((now, entry.clone()));
        entry
    }
}

/// A conservative byte-based reservation, bounded by Jev's largest input. Reported usage
/// replaces it when the request returns.
pub fn estimated_tokens(request: &Request) -> u64 {
    let bytes = request.bytes() as u64 + 512 + 32 * request.questions.len() as u64;
    bytes.min(MAX_ESTIMATED_TOKENS)
}

pub struct Evaluator {
    client: jev::Client,
    cancel: CancellationToken,
    slots: Semaphore,
    budget: Mutex<RateBudget>,
    /// No attempt starts before this instant: a 429 asked the whole search to wait.
    cooldown: Mutex<Instant>,
    requests: AtomicUsize,
    unauthorized: AtomicBool,
    /// Cancelled with the search, or when TypeSafe rejects the key: it wakes every attempt
    /// that waits for admission or for a retry.
    stop: CancellationToken,
    attempts: Mutex<Vec<Attempt>>,
    model: Mutex<Option<String>>,
}

/// The gate of one evaluation: each HTTP attempt takes one request of the search's limit and
/// its token estimate from the rate budget.
struct Admission<'a> {
    evaluator: &'a Evaluator,
    tokens: u64,
}

impl jev::Gate for Admission<'_> {
    type Ticket = Arc<Mutex<u64>>;

    async fn admit(&self) -> Result<Self::Ticket, Failure> {
        let evaluator = self.evaluator;
        evaluator.check()?;
        let counted = evaluator
            .requests
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < REQUEST_LIMIT).then_some(n + 1)
            });
        if counted.is_err() {
            return Err(Failure::BudgetExhausted);
        }
        loop {
            // A wait can outlast the search or the key: give the request back then.
            if let Err(failure) = evaluator.check_running() {
                evaluator.requests.fetch_sub(1, Ordering::SeqCst);
                return Err(failure);
            }
            let wait = {
                // One lock for the check and the reservation, so two attempts cannot
                // both take the same remaining capacity.
                let mut budget = evaluator.budget.lock().unwrap();
                let now = Instant::now();
                let cooling = evaluator
                    .cooldown
                    .lock()
                    .unwrap()
                    .saturating_duration_since(now);
                let wait = budget.wait(self.tokens, now).max(cooling);
                if wait.is_zero() {
                    return Ok(budget.reserve(self.tokens, now));
                }
                wait
            };
            tokio::select! {
                _ = evaluator.stop.cancelled() => {}
                _ = tokio::time::sleep(wait.min(Duration::from_secs(60))) => {}
            }
        }
    }

    fn record(&self, ticket: Self::Ticket, attempt: &Attempt) {
        if let Some(usage) = attempt.usage {
            *ticket.lock().unwrap() = usage.input_tokens;
        }
        self.evaluator
            .attempts
            .lock()
            .unwrap()
            .push(attempt.clone());
    }

    fn back_off(&self, pause: Duration) {
        let mut cooldown = self.evaluator.cooldown.lock().unwrap();
        *cooldown = (*cooldown).max(Instant::now() + pause);
    }
}

impl Evaluator {
    pub fn new(client: jev::Client, cancel: CancellationToken) -> Evaluator {
        Evaluator {
            client,
            stop: cancel.child_token(),
            cancel,
            slots: Semaphore::new(CONCURRENCY),
            budget: Mutex::new(RateBudget::default()),
            cooldown: Mutex::new(Instant::now()),
            requests: AtomicUsize::new(0),
            unauthorized: AtomicBool::new(false),
            attempts: Mutex::new(Vec::new()),
            model: Mutex::new(None),
        }
    }

    /// Every HTTP request so far, for the report.
    pub fn attempts(&self) -> Vec<Attempt> {
        self.attempts.lock().unwrap().clone()
    }

    /// The model that Jev reported, once a request succeeded.
    pub fn model(&self) -> Option<String> {
        self.model.lock().unwrap().clone()
    }

    /// One answer per question, in question order.
    pub async fn evaluate(
        &self,
        request: &Request,
        navigation: bool,
    ) -> Result<Vec<f64>, EvalError> {
        self.check().map_err(|failure| self.error(failure, false))?;
        let _slot = tokio::select! {
            _ = self.cancel.cancelled() => return Err(EvalError::Cancelled),
            slot = self.slots.acquire() => slot.expect("the semaphore is never closed"),
        };
        let multiple = request.questions.len() > 1;
        let retry = if navigation && multiple {
            Retry::SplitOnOverload
        } else {
            Retry::Standard
        };
        // No deadline for the call: a wait for admission is not network time, and each
        // attempt has its own timeout from when it is sent.
        let gate = Admission {
            evaluator: self,
            tokens: estimated_tokens(request),
        };
        let outcome = self
            .client
            .evaluate(
                &request.state,
                &request.questions,
                retry,
                None,
                &self.stop,
                &gate,
            )
            .await;
        match outcome.result {
            Ok(answers) => {
                *self.model.lock().unwrap() = Some(answers.model);
                Ok(answers.probabilities)
            }
            Err(failure) => Err(self.error(failure, navigation && multiple)),
        }
    }

    fn error(&self, failure: Failure, batch: bool) -> EvalError {
        match failure {
            Failure::Unauthorized => {
                self.unauthorized.store(true, Ordering::Relaxed);
                self.stop.cancel();
                EvalError::Unauthorized
            }
            // The stop token also ends a call when another call's key was rejected.
            Failure::Cancelled if self.unauthorized.load(Ordering::Relaxed) => {
                EvalError::Unauthorized
            }
            Failure::Cancelled => EvalError::Cancelled,
            Failure::BudgetExhausted => EvalError::BudgetExhausted,
            failure => {
                // jevgrep splits a navigation batch on any passing failure except 429.
                let passing = matches!(
                    failure,
                    Failure::Overloaded
                        | Failure::TimedOut
                        | Failure::Unavailable { status: None }
                        | Failure::Unavailable {
                            status: Some(408 | 500..=599)
                        }
                );
                EvalError::Provider {
                    split: batch && passing,
                    failure,
                }
            }
        }
    }

    /// The search was cancelled or TypeSafe rejected the key.
    fn check_running(&self) -> Result<(), Failure> {
        if self.cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if self.unauthorized.load(Ordering::Relaxed) {
            return Err(Failure::Unauthorized);
        }
        Ok(())
    }

    fn check(&self) -> Result<(), Failure> {
        self.check_running()?;
        if self.requests.load(Ordering::SeqCst) >= REQUEST_LIMIT {
            return Err(Failure::BudgetExhausted);
        }
        Ok(())
    }
}

// The fake Jev server of the integration tests.
#[cfg(test)]
#[path = "../../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::*;
    use jev::Gate;

    use super::support;

    fn questions(n: usize) -> Vec<jev::Question> {
        (0..n)
            .map(|i| jev::Question {
                key: format!("q{i}"),
                instructions: "x?".into(),
            })
            .collect()
    }

    #[tokio::test]
    async fn the_request_limit_holds_under_concurrency_and_retries() {
        // Every first attempt gets a 429, so every evaluation would retry.
        let server = support::FakeJev::start(|_, _| {
            support::Reply::status(429, "{}").header("retry-after", "0")
        })
        .await;
        let evaluator = Arc::new(Evaluator::new(
            jev::Client::new(&server.url, "k"),
            CancellationToken::new(),
        ));
        evaluator
            .requests
            .store(REQUEST_LIMIT - 3, Ordering::SeqCst);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let evaluator = evaluator.clone();
            tasks.spawn(async move {
                let request = Request {
                    state: serde_json::json!({}),
                    questions: questions(1),
                };
                evaluator.evaluate(&request, false).await
            });
        }
        let mut exhausted = 0;
        while let Some(result) = tasks.join_next().await {
            if matches!(result.unwrap(), Err(EvalError::BudgetExhausted)) {
                exhausted += 1;
            }
        }
        assert_eq!(server.requests(), 3, "exactly the requests left were sent");
        assert_eq!(evaluator.requests.load(Ordering::SeqCst), REQUEST_LIMIT);
        assert_eq!(evaluator.attempts().len(), 3);
        assert!(exhausted >= 29, "{exhausted}");
    }

    #[tokio::test]
    async fn concurrent_admissions_share_the_token_budget() {
        let evaluator = Evaluator::new(
            jev::Client::new("http://127.0.0.1:9/", "k"),
            CancellationToken::new(),
        );
        let started = Instant::now();
        let first = Admission {
            evaluator: &evaluator,
            tokens: 200_000,
        };
        let second = Admission {
            evaluator: &evaluator,
            tokens: 200_000,
        };
        let (a, b) = tokio::join!(first.admit(), second.admit());
        assert!(a.is_ok() && b.is_ok());
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "the second waited for the first to age out"
        );
    }

    #[tokio::test]
    async fn a_429_cools_down_every_attempt() {
        let evaluator = Evaluator::new(
            jev::Client::new("http://127.0.0.1:9/", "k"),
            CancellationToken::new(),
        );
        let admission = Admission {
            evaluator: &evaluator,
            tokens: 10,
        };
        admission.back_off(Duration::from_millis(500));
        let started = Instant::now();
        admission.admit().await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(450));
    }

    #[tokio::test]
    async fn a_final_429_still_cools_down_the_search() {
        let server = support::FakeJev::start(|_, _| {
            support::Reply::status(429, "{}").header("retry-after", "0.6")
        })
        .await;
        let evaluator =
            Evaluator::new(jev::Client::new(&server.url, "k"), CancellationToken::new());
        let request = Request {
            state: serde_json::json!({}),
            questions: questions(1),
        };
        assert!(evaluator.evaluate(&request, false).await.is_err());
        let started = Instant::now();
        Admission {
            evaluator: &evaluator,
            tokens: 10,
        }
        .admit()
        .await
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_long_admission_wait_is_not_a_timeout() {
        let server =
            support::FakeJev::start(|_, request| support::Reply::answers(request, |_| 0.5)).await;
        let evaluator =
            Evaluator::new(jev::Client::new(&server.url, "k"), CancellationToken::new());
        Admission {
            evaluator: &evaluator,
            tokens: 10,
        }
        .back_off(Duration::from_millis(1500));
        let request = Request {
            state: serde_json::json!({}),
            questions: questions(1),
        };
        assert_eq!(
            evaluator.evaluate(&request, false).await.unwrap(),
            vec![0.5]
        );
        assert_eq!(server.requests(), 1);
        assert_eq!(evaluator.requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_rejected_key_stops_waiting_attempts() {
        let server = support::FakeJev::start(|_, _| {
            support::Reply::status(401, "{}").delayed(Duration::from_millis(100))
        })
        .await;
        let evaluator =
            Evaluator::new(jev::Client::new(&server.url, "k"), CancellationToken::new());
        let request = Request {
            state: serde_json::json!({}),
            questions: questions(1),
        };
        let waiting = Admission {
            evaluator: &evaluator,
            tokens: TOKENS_PER_SECOND,
        };
        let started = Instant::now();
        let (first, second) = tokio::join!(evaluator.evaluate(&request, false), async {
            // Let the first attempt take its share of the budget, so this one waits.
            tokio::time::sleep(Duration::from_millis(20)).await;
            waiting.admit().await
        });
        assert!(matches!(first, Err(EvalError::Unauthorized)));
        assert!(matches!(second, Err(Failure::Unauthorized)));
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(server.requests(), 1);
        assert_eq!(
            evaluator.requests.load(Ordering::SeqCst),
            1,
            "the waiting request was given back"
        );
    }

    #[tokio::test]
    async fn a_rejected_key_ends_a_retry_wait() {
        let server = support::FakeJev::start(|_, request| {
            let question = request["questions"]["q0"]["instructions"]
                .as_str()
                .unwrap_or("");
            if question == "slow?" {
                support::Reply::status(429, "{}").header("retry-after", "3600")
            } else {
                support::Reply::status(401, "{}").delayed(Duration::from_millis(200))
            }
        })
        .await;
        let evaluator =
            Evaluator::new(jev::Client::new(&server.url, "k"), CancellationToken::new());
        let ask = |text: &str| Request {
            state: serde_json::json!({}),
            questions: vec![jev::Question {
                key: "q0".into(),
                instructions: text.into(),
            }],
        };
        let (slow, fast) = (ask("slow?"), ask("fast?"));
        let started = Instant::now();
        let (waiting, rejected) = tokio::join!(
            evaluator.evaluate(&slow, false),
            evaluator.evaluate(&fast, false)
        );
        assert!(
            matches!(waiting, Err(EvalError::Unauthorized)),
            "{waiting:?}"
        );
        assert!(matches!(rejected, Err(EvalError::Unauthorized)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(server.requests(), 2, "the 429 was not retried");
    }

    #[test]
    fn the_rate_budget_counts_requests_per_minute_and_tokens_per_second() {
        let mut budget = RateBudget::default();
        let start = Instant::now();
        assert_eq!(budget.wait(1000, start), Duration::ZERO);
        budget.reserve(200_000, start);
        let wait = budget.wait(100_000, start + Duration::from_millis(200));
        assert_eq!(wait, Duration::from_millis(800));
        assert_eq!(
            budget.wait(100_000, start + Duration::from_secs(1)),
            Duration::ZERO
        );
        let entry = budget.reserve(60_000, start + Duration::from_secs(1));
        *entry.lock().unwrap() = 10;
        assert_eq!(
            budget.wait(240_000, start + Duration::from_secs(1)),
            Duration::ZERO,
            "usage replaces the estimate"
        );

        let mut budget = RateBudget::default();
        for i in 0..REQUESTS_PER_MINUTE {
            budget.reserve(1, start + Duration::from_millis(i as u64));
        }
        let wait = budget.wait(1, start + Duration::from_secs(30));
        assert_eq!(wait, Duration::from_secs(30));
        assert_eq!(
            budget.wait(1, start + Duration::from_secs(61)),
            Duration::ZERO
        );
    }
}
