// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

// LimitadorClient, a QuotaBackend over Limitador's HTTP API. `check` probes
// {endpoint}/check with a delta of one (200 within limit, 429 over) and
// `report` debits via {endpoint}/report. /check_and_report is avoided: it
// skips the increment on a 429, freezing the counter at the limit.

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::json;

use crate::backend::{BackendError, CheckOutcome, QuotaBackend};

/// The check probes with a delta of one, so it denies once the counter
/// reaches the limit. It charges nothing. The debit happens in `report`.
const CHECK_PROBE_DELTA: u64 = 1;

/// HTTP client bound to one Limitador endpoint and namespace. Holds one
/// pooled `reqwest::Client` built at construction.
#[derive(Debug)]
pub(crate) struct LimitadorClient {
    http: reqwest::Client,
    namespace: String,
    check_url: String,
    report_url: String,
}

impl LimitadorClient {
    /// Build the client for `endpoint`/`namespace` with a per-call `timeout`.
    /// A trailing slash on `endpoint` is trimmed.
    ///
    /// # Errors
    ///
    /// A message when the `reqwest::Client` cannot be built.
    pub(crate) fn new(
        endpoint: &str,
        namespace: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("Limitador HTTP client build failed: {e}"))?;
        let endpoint = endpoint.trim_end_matches('/');
        Ok(Self {
            http,
            namespace: namespace.to_owned(),
            check_url: format!("{endpoint}/check"),
            report_url: format!("{endpoint}/report"),
        })
    }

    /// POST `body` as JSON to `url`, returning the response status.
    async fn send(&self, url: &str, body: serde_json::Value) -> Result<StatusCode, BackendError> {
        self.http
            .post(url)
            .json(&body)
            .send()
            .await
            .map(|r| r.status())
            .map_err(|e| BackendError {
                message: format!("Limitador POST {url} failed: {e}"),
            })
    }
}

#[async_trait]
impl QuotaBackend for LimitadorClient {
    async fn check(
        &self,
        descriptor_key: &str,
        descriptor_value: &str,
    ) -> Result<CheckOutcome, BackendError> {
        let body = json!({
            "namespace": self.namespace,
            "values": { descriptor_key: descriptor_value },
            "delta": CHECK_PROBE_DELTA,
        });
        let status = self.send(&self.check_url, body).await?;
        match status {
            StatusCode::OK => Ok(CheckOutcome::WithinLimit),
            StatusCode::TOO_MANY_REQUESTS => Ok(CheckOutcome::OverLimit),
            other => Err(BackendError {
                message: format!("Limitador /check returned unexpected status {other}"),
            }),
        }
    }

    async fn report(
        &self,
        descriptor_key: &str,
        descriptor_value: &str,
        delta: u64,
    ) -> Result<(), BackendError> {
        let body = json!({
            "namespace": self.namespace,
            "values": { descriptor_key: descriptor_value },
            "delta": delta,
        });
        let status = self.send(&self.report_url, body).await?;
        if status == StatusCode::OK {
            return Ok(());
        }
        Err(BackendError {
            message: format!("Limitador /report returned unexpected status {status}"),
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use std::time::Duration;

    fn client(server: &mockito::Server) -> LimitadorClient {
        LimitadorClient::new(&server.url(), "grid-tokens", Duration::from_secs(5))
            .expect("client builds")
    }

    #[tokio::test]
    async fn check_200_is_within_limit() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/check")
            .with_status(200)
            .create_async()
            .await;
        let outcome = client(&server).check("sub", "bob").await.unwrap();
        assert_eq!(outcome, CheckOutcome::WithinLimit);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn check_429_is_over_limit() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/check")
            .with_status(429)
            .create_async()
            .await;
        let outcome = client(&server).check("sub", "bob").await.unwrap();
        assert_eq!(outcome, CheckOutcome::OverLimit);
    }

    #[tokio::test]
    async fn check_sends_namespace_and_descriptor() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/check")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"namespace":"grid-tokens","values":{"sub":"bob"},"delta":1}"#.to_owned(),
            ))
            .with_status(200)
            .create_async()
            .await;
        client(&server).check("sub", "bob").await.unwrap();
        m.assert_async().await;
    }

    #[tokio::test]
    async fn check_unexpected_status_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/check")
            .with_status(500)
            .create_async()
            .await;
        let err = client(&server).check("sub", "bob").await.unwrap_err();
        assert!(err.message.contains("500"), "{}", err.message);
    }

    #[tokio::test]
    async fn report_sends_delta() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/report")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"namespace":"grid-tokens","values":{"sub":"bob"},"delta":42}"#.to_owned(),
            ))
            .with_status(200)
            .create_async()
            .await;
        client(&server).report("sub", "bob", 42).await.unwrap();
        m.assert_async().await;
    }

    #[tokio::test]
    async fn report_treats_a_non_200_as_an_error() {
        // /report increments unconditionally and answers 200. Anything else
        // is a real failure the caller logs.
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/report")
            .with_status(500)
            .create_async()
            .await;
        let err = client(&server).report("sub", "bob", 10).await.unwrap_err();
        assert!(err.message.contains("500"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_call_to_a_dead_endpoint_errors() {
        // Nothing is listening on this port, so the client must surface a
        // transport error rather than hang or panic.
        let c = LimitadorClient::new("http://127.0.0.1:1", "ns", Duration::from_millis(200))
            .expect("client builds");
        let err = c.check("sub", "bob").await.unwrap_err();
        assert!(err.message.contains("failed"), "{}", err.message);
    }
}
