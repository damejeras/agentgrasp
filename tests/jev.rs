mod support;

use std::time::{Duration, Instant};

use agentgrasp::jev::{Client, Failure, Outcome, Question, Retry};
use serde_json::{Value, json};
use support::{FakeJev, Reply};
use tokio_util::sync::CancellationToken;

fn questions(texts: &[&str]) -> Vec<Question> {
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| Question {
            key: format!("q{i}"),
            instructions: text.to_string(),
        })
        .collect()
}

async fn evaluate(server: &FakeJev, qs: &[Question], retry: Retry, within: Duration) -> Outcome {
    let client = Client::new(&server.url, "test-key");
    let state = json!({"text": "SECRET-SOURCE-MARKER"});
    client
        .evaluate(
            &state,
            qs,
            retry,
            Instant::now() + within,
            &CancellationToken::new(),
        )
        .await
}

fn answer_body(answers: Value) -> String {
    json!({"model": "jev-fake", "answers": answers, "usage": {"input_tokens": 1, "output_tokens": 1}})
        .to_string()
}

#[tokio::test]
async fn sends_noul_questions_and_returns_answers_in_question_order() {
    let server = FakeJev::start(|_, request| {
        Reply::answers(request, |text| if text.contains("yes") { 0.9 } else { 0.1 })
    })
    .await;
    let qs = questions(&["no one", "yes two", "no three"]);
    let outcome = evaluate(&server, &qs, Retry::Standard, Duration::from_secs(10)).await;
    let answers = outcome.result.unwrap();
    assert_eq!(answers.probabilities, vec![0.1, 0.9, 0.1]);
    assert_eq!(answers.model, "jev-fake");
    assert_eq!(outcome.attempts.len(), 1);
    assert_eq!(outcome.attempts[0].usage.unwrap().input_tokens, 10);
    let body = server.body(0);
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(
        body["questions"]["q1"],
        json!({"type": "noul", "instructions": "yes two"})
    );
    assert_eq!(body["state"]["text"], "SECRET-SOURCE-MARKER");
    assert_eq!(server.authorization(0), "Bearer test-key");
}

#[tokio::test]
async fn rejects_partial_extra_and_malformed_answers() {
    let bad = [
        answer_body(json!({"q0": {"type": "noul", "noul": 0.5}})),
        answer_body(json!({
            "q0": {"type": "noul", "noul": 0.5},
            "q1": {"type": "noul", "noul": 0.5},
            "q2": {"type": "noul", "noul": 0.5},
        })),
        answer_body(
            json!({"q0": {"type": "noul", "noul": 0.5}, "q1": {"type": "noul", "noul": 1.5}}),
        ),
        answer_body(
            json!({"q0": {"type": "noul", "noul": 0.5}, "q1": {"type": "score", "noul": 0.5}}),
        ),
        answer_body(json!({"q0": {"type": "noul", "noul": 0.5}, "q1": {"type": "noul"}})),
        "not json".to_string(),
    ];
    for body in bad {
        let reply = body.clone();
        let server = FakeJev::start(move |_, _| Reply::status(200, reply.clone())).await;
        let outcome = evaluate(
            &server,
            &questions(&["a", "b"]),
            Retry::Standard,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(
            outcome.result.unwrap_err(),
            Failure::InvalidResponse,
            "{body}"
        );
        assert_eq!(server.requests(), 1, "an invalid answer is not retried");
    }
}

#[tokio::test]
async fn retries_429_once_and_honours_retry_after() {
    let server = FakeJev::start(|index, request| match index {
        0 => Reply::status(429, "{}").header("retry-after", "0.2"),
        _ => Reply::answers(request, |_| 0.5),
    })
    .await;
    let started = Instant::now();
    let outcome = evaluate(
        &server,
        &questions(&["a"]),
        Retry::SplitOnOverload,
        Duration::from_secs(10),
    )
    .await;
    assert!(outcome.result.is_ok());
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].status, Some(429));

    let server = FakeJev::start(|_, _| Reply::status(429, "{}").header("retry-after", "0")).await;
    let outcome = evaluate(
        &server,
        &questions(&["a"]),
        Retry::Standard,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        outcome.result.unwrap_err(),
        Failure::Unavailable { status: Some(429) }
    );
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn overload_retries_once_or_asks_for_a_split() {
    let server = FakeJev::start(|_, _| Reply::status(529, "{}").header("retry-after", "0")).await;
    let outcome = evaluate(
        &server,
        &questions(&["a", "b"]),
        Retry::Standard,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        outcome.result.unwrap_err(),
        Failure::Unavailable { status: Some(529) }
    );
    assert_eq!(server.requests(), 2);

    let server = FakeJev::start(|_, _| Reply::status(529, "{}")).await;
    let outcome = evaluate(
        &server,
        &questions(&["a", "b"]),
        Retry::SplitOnOverload,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::Overloaded);
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn other_errors_are_not_retried() {
    let cases = [
        (500, "{}", Failure::Unavailable { status: Some(500) }),
        (
            401,
            r#"{"detail":{"error_type":"authentication_error"}}"#,
            Failure::Unauthorized,
        ),
        (
            400,
            r#"{"detail":{"error_type":"max_tokens_exceeded"}}"#,
            Failure::TooLarge,
        ),
        (
            400,
            r#"{"detail":{"error_type":"other"}}"#,
            Failure::Unavailable { status: Some(400) },
        ),
        (422, "{}", Failure::Unavailable { status: Some(422) }),
    ];
    for (status, body, expected) in cases {
        let server = FakeJev::start(move |_, _| Reply::status(status, body)).await;
        let outcome = evaluate(
            &server,
            &questions(&["a"]),
            Retry::Standard,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(outcome.result.unwrap_err(), expected);
        assert_eq!(server.requests(), 1);
    }
}

#[tokio::test]
async fn the_deadline_bounds_the_call() {
    let server = FakeJev::start(|_, request| {
        Reply::answers(request, |_| 0.5).delayed(Duration::from_secs(5))
    })
    .await;
    let started = Instant::now();
    let outcome = evaluate(
        &server,
        &questions(&["a"]),
        Retry::Standard,
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));

    // A retry that cannot finish before the deadline is not started.
    let server = FakeJev::start(|_, _| Reply::status(429, "{}").header("retry-after", "30")).await;
    let started = Instant::now();
    let outcome = evaluate(
        &server,
        &questions(&["a"]),
        Retry::Standard,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::TimedOut);
    assert_eq!(server.requests(), 1);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn cancellation_stops_the_call() {
    let server = FakeJev::start(|_, request| {
        Reply::answers(request, |_| 0.5).delayed(Duration::from_secs(5))
    })
    .await;
    let client = Client::new(&server.url, "k");
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let outcome = client
        .evaluate(
            &json!({}),
            &questions(&["a"]),
            Retry::Standard,
            Instant::now() + Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn failure_text_never_holds_the_response_body() {
    let body = r#"{"detail":{"error_type":"other","message":"SECRET-SOURCE-MARKER"}}"#;
    for status in [400, 401, 429, 500, 529] {
        let server =
            FakeJev::start(move |_, _| Reply::status(status, body).header("retry-after", "0"))
                .await;
        let outcome = evaluate(
            &server,
            &questions(&["a"]),
            Retry::Standard,
            Duration::from_secs(10),
        )
        .await;
        let text = outcome.result.unwrap_err().to_string();
        assert!(!text.contains("SECRET"), "{text}");
    }
}

#[tokio::test]
async fn an_unreachable_server_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    drop(listener);
    let client = Client::new(url, "k");
    let outcome = client
        .evaluate(
            &json!({}),
            &questions(&["a"]),
            Retry::Standard,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(
        outcome.result.unwrap_err(),
        Failure::Unavailable { status: None }
    );
    assert_eq!(outcome.attempts.len(), 1);
}

#[tokio::test]
async fn duplicate_answer_keys_are_invalid() {
    let body = r#"{"model":"m","answers":{"q0":{"type":"noul","noul":0.1},"q0":{"type":"noul","noul":0.9}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
    let server = FakeJev::start(move |_, _| Reply::status(200, body)).await;
    let outcome = evaluate(
        &server,
        &questions(&["a"]),
        Retry::Standard,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::InvalidResponse);
}

#[tokio::test]
async fn usage_of_an_invalid_answer_set_is_kept() {
    let body = answer_body(json!({"q0": {"type": "noul", "noul": 7.0}}));
    let server = FakeJev::start(move |_, _| Reply::status(200, body.clone())).await;
    let outcome = evaluate(
        &server,
        &questions(&["a"]),
        Retry::Standard,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::InvalidResponse);
    assert_eq!(outcome.attempts[0].usage.unwrap().input_tokens, 1);
}

#[tokio::test]
async fn a_cancelled_request_is_recorded() {
    let server = FakeJev::start(|_, request| {
        Reply::answers(request, |_| 0.5).delayed(Duration::from_secs(5))
    })
    .await;
    let client = Client::new(&server.url, "k");
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let outcome = client
        .evaluate(
            &json!({}),
            &questions(&["a"]),
            Retry::Standard,
            Instant::now() + Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert_eq!(outcome.result.unwrap_err(), Failure::Cancelled);
    assert_eq!(outcome.attempts.len(), 1);
    assert!(outcome.attempts[0].latency_ms >= 100);
}

#[tokio::test]
async fn an_absurd_retry_after_does_not_panic() {
    for value in ["1e100", "18446744073709551615", "-1", "soon"] {
        let server =
            FakeJev::start(move |_, _| Reply::status(429, "{}").header("retry-after", value)).await;
        let outcome = evaluate(
            &server,
            &questions(&["a"]),
            Retry::Standard,
            Duration::from_secs(3),
        )
        .await;
        assert!(outcome.result.is_err(), "{value}");
    }
}
