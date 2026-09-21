// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! End-to-end behavior of the two hook handlers against a mock Limitador.
//!
//! The four rows the plugin exists to get right: over budget denies, under
//! budget allows, an unreachable Limitador honors `on_error`, and an
//! output with no usage debits nothing and never denies.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]

use std::sync::Arc;

use praxis_policy_core::cmf::{Message, MessagePayload, Role};
use praxis_policy_core::extensions::{
    CompletionExtension, Extensions, SecurityExtension, SubjectExtension, TokenUsage,
};
use praxis_policy_core::hooks::HookHandler as _;
use praxis_policy_core::plugin::PluginConfig;
use praxis_policy_core::prelude::PluginContext;
use serde_json::json;

use praxis_policy_plugin_quota::factory::KIND;
use praxis_policy_plugin_quota::handlers::{Quota, QuotaCheck, QuotaReport};

/// Build a core pointed at `endpoint`, with an optional `on_error` override.
fn core(endpoint: &str, on_error: &str) -> Arc<Quota> {
    let cfg = PluginConfig {
        name: "token-quota".into(),
        kind: KIND.into(),
        config: Some(json!({
            "endpoint": endpoint,
            "namespace": "grid-tokens",
            "on_error": on_error,
            "timeout_seconds": 1,
        })),
        ..Default::default()
    };
    Arc::new(Quota::new(cfg).expect("core builds"))
}

fn ext_with_sub(sub: &str) -> Extensions {
    Extensions {
        security: Some(Arc::new(SecurityExtension {
            subject: Some(SubjectExtension {
                id: Some(sub.to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn input_payload() -> MessagePayload {
    MessagePayload {
        message: Message::text(Role::User, "hello"),
    }
}

fn output_payload(body: &str) -> MessagePayload {
    MessagePayload {
        message: Message::text(Role::Assistant, body),
    }
}

#[tokio::test]
async fn under_budget_check_allows() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/check")
        .with_status(200)
        .create_async()
        .await;

    let handler = QuotaCheck::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(
        !result.is_denied(),
        "a within-limit consumer must be allowed"
    );
}

#[tokio::test]
async fn over_budget_check_denies_with_quota_exhausted() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/check")
        .with_status(429)
        .create_async()
        .await;

    let handler = QuotaCheck::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(result.is_denied(), "an over-budget consumer must be denied");
    let violation = result.violation.expect("a denial carries a violation");
    assert_eq!(violation.code, "quota.exhausted");
}

#[tokio::test]
async fn unreachable_limitador_fails_open_when_on_error_allow() {
    // Port 1 has nothing listening — the check call fails at transport.
    let handler = QuotaCheck::new(core("http://127.0.0.1:1", "allow"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(
        !result.is_denied(),
        "on_error: allow must serve the request when Limitador is unreachable"
    );
}

#[tokio::test]
async fn unreachable_limitador_fails_closed_when_on_error_deny() {
    let handler = QuotaCheck::new(core("http://127.0.0.1:1", "deny"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(
        result.is_denied(),
        "on_error: deny must refuse the request when Limitador is unreachable"
    );
    let violation = result.violation.expect("a denial carries a violation");
    assert_eq!(violation.code, "quota.backend_unavailable");
}

#[tokio::test]
async fn report_debits_the_parsed_total() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/report")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"namespace":"grid-tokens","values":{"sub":"bob"},"delta":11}"#.to_owned(),
        ))
        .with_status(200)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let body = r#"{"choices":[],"usage":{"prompt_tokens":5,"total_tokens":11}}"#;
    let result = handler
        .handle(&output_payload(body), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(!result.is_denied(), "the report hook never denies");
    m.assert_async().await;
}

#[tokio::test]
async fn report_debits_nothing_when_usage_absent() {
    let mut server = mockito::Server::new_async().await;
    // If the handler ever posts here, the expect(0) mock fails the test.
    let m = server
        .mock("POST", "/report")
        .expect(0)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    // Streaming chunk with no usage object.
    let body = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
    let result = handler
        .handle(&output_payload(body), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(!result.is_denied(), "an absent total must never deny");
    m.assert_async().await;
}

#[tokio::test]
async fn report_never_denies_even_when_the_debit_fails() {
    // Limitador answers the debit with a 500. The response is already out,
    // so this must be swallowed, not turned into a denial.
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/report")
        .with_status(500)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let body = r#"{"usage":{"total_tokens":11}}"#;
    let result = handler
        .handle(&output_payload(body), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(!result.is_denied(), "a failed debit must never deny");
}

fn ext_with_sub_and_usage(sub: &str, total: u32) -> Extensions {
    let mut ext = ext_with_sub(sub);
    ext.completion = Some(Arc::new(CompletionExtension {
        tokens: Some(TokenUsage {
            total_tokens: total,
            ..Default::default()
        }),
        ..Default::default()
    }));
    ext
}

#[tokio::test]
async fn report_debits_the_typed_completion_usage() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/report")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"values":{"sub":"alice"},"delta":25}"#.to_owned(),
        ))
        .with_status(200)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    // Body carries no usage; the total must come from the typed slot.
    let result = handler
        .handle(
            &output_payload("done"),
            &ext_with_sub_and_usage("alice", 25),
            &mut ctx,
        )
        .await;
    assert!(!result.is_denied());
    m.assert_async().await;
}

#[tokio::test]
async fn report_prefers_the_typed_usage_over_a_body_total() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/report")
        .match_body(mockito::Matcher::PartialJsonString(
            r#"{"values":{"sub":"alice"},"delta":25}"#.to_owned(),
        ))
        .with_status(200)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    // A hostile body claiming a tiny cost must not win over the typed slot.
    let body = r#"{"usage":{"total_tokens":1}}"#;
    let result = handler
        .handle(
            &output_payload(body),
            &ext_with_sub_and_usage("alice", 25),
            &mut ctx,
        )
        .await;
    assert!(!result.is_denied());
    m.assert_async().await;
}

#[tokio::test]
async fn check_skips_limitador_without_a_resolved_identity() {
    let mut server = mockito::Server::new_async().await;
    let m = server.mock("POST", "/check").expect(0).create_async().await;

    let handler = QuotaCheck::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &Extensions::default(), &mut ctx)
        .await;
    assert!(
        !result.is_denied(),
        "no identity is skipped, not denied here"
    );
    m.assert_async().await;
}

#[tokio::test]
async fn report_skips_the_debit_without_a_resolved_identity() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/report")
        .expect(0)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let ext = Extensions {
        completion: Some(Arc::new(CompletionExtension {
            tokens: Some(TokenUsage {
                total_tokens: 99,
                ..Default::default()
            }),
            ..Default::default()
        })),
        ..Default::default()
    };
    let result = handler
        .handle(&output_payload("done"), &ext, &mut ctx)
        .await;
    assert!(!result.is_denied());
    m.assert_async().await;
}

#[tokio::test]
async fn report_refuses_a_fractional_body_total() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/report")
        .expect(0)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    // No typed slot; a fractional body total is not a token count.
    let body = r#"{"usage":{"total_tokens":1.5}}"#;
    let result = handler
        .handle(&output_payload(body), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(!result.is_denied());
    m.assert_async().await;
}

#[tokio::test]
async fn report_ignores_a_negative_body_total() {
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/report")
        .expect(0)
        .create_async()
        .await;

    let handler = QuotaReport::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let body = r#"{"usage":{"total_tokens":-5}}"#;
    let result = handler
        .handle(&output_payload(body), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(!result.is_denied());
    m.assert_async().await;
}

#[tokio::test]
async fn check_fails_closed_on_a_server_error_under_on_error_deny() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/check")
        .with_status(500)
        .create_async()
        .await;

    let handler = QuotaCheck::new(core(&server.url(), "deny"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(result.is_denied(), "a 500 under on_error: deny must refuse");
}

#[tokio::test]
async fn check_fails_open_on_a_server_error_under_on_error_allow() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/check")
        .with_status(500)
        .create_async()
        .await;

    let handler = QuotaCheck::new(core(&server.url(), "allow"));
    let mut ctx = PluginContext::new();
    let result = handler
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(
        !result.is_denied(),
        "a 500 under on_error: allow must serve"
    );
}

/// A stateful mock Limitador that models the real counter: `/check` with the
/// plugin's probe delta of 1 refuses once the counter reaches `max`, and
/// `/report` increments unconditionally. This is what a fixed-response mock
/// cannot express, and it is what proves the debit path accumulates.
async fn start_stateful_limitador(max: u64) -> String {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Lim {
        counter: Arc<Mutex<u64>>,
        max: u64,
    }

    fn delta(b: &serde_json::Value) -> u64 {
        b.get("delta")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    }

    async fn check(State(s): State<Lim>, Json(b): Json<serde_json::Value>) -> StatusCode {
        let over = *s.counter.lock().expect("counter lock") + delta(&b) > s.max;
        if over {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::OK
        }
    }

    async fn report(State(s): State<Lim>, Json(b): Json<serde_json::Value>) -> StatusCode {
        *s.counter.lock().expect("counter lock") += delta(&b);
        StatusCode::OK
    }

    let app = Router::new()
        .route("/check", post(check))
        .route("/report", post(report))
        .with_state(Lim {
            counter: Arc::new(Mutex::new(0)),
            max,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn a_principal_is_denied_once_cumulative_debits_reach_the_budget() {
    // Budget 100, debit 40 per round. The check runs before each round's
    // debit, so counters 0, 40, 80 all admit. The debits carry the counter
    // to 120, and the fourth check refuses. This drives the whole loop the
    // plugin exists to close, against a Limitador that counts.
    let url = start_stateful_limitador(100).await;
    let check = QuotaCheck::new(core(&url, "deny"));
    let report = QuotaReport::new(core(&url, "deny"));
    let mut ctx = PluginContext::new();

    for round in 0..3 {
        let admitted = check
            .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
            .await;
        assert!(!admitted.is_denied(), "round {round} must be admitted");
        let debit = report
            .handle(
                &output_payload("done"),
                &ext_with_sub_and_usage("bob", 40),
                &mut ctx,
            )
            .await;
        assert!(!debit.is_denied(), "the report hook never denies");
    }

    let denied = check
        .handle(&input_payload(), &ext_with_sub("bob"), &mut ctx)
        .await;
    assert!(denied.is_denied(), "a principal over budget must be denied");
    let violation = denied.violation.expect("a denial carries a violation");
    assert_eq!(violation.code, "quota.exhausted");
}
