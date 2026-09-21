// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

// The backend contract the quota plugin meters through: a check and a debit.
// The core holds a `dyn QuotaBackend`, so a second backend needs no handler
// change. LimitadorClient (client.rs) is the one implementor.

use async_trait::async_trait;

/// Verdict of a backend `check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Within budget: the request may proceed.
    WithinLimit,
    /// Over budget: the request must be refused.
    OverLimit,
}

/// A backend call that failed or answered unrecognizably. Distinct from an
/// over-limit verdict, which is a successful [`CheckOutcome`].
#[derive(Debug)]
pub struct BackendError {
    /// Human-readable cause, for logs and fail-closed denials.
    pub message: String,
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Where a per-principal budget is checked and debited, keyed on a descriptor
/// (`descriptor_key: descriptor_value`) resolved from identity.
#[async_trait]
pub trait QuotaBackend: std::fmt::Debug + Send + Sync {
    /// Whether the descriptor is within budget, charging nothing.
    ///
    /// # Errors
    ///
    /// [`BackendError`] when the call cannot complete or is unrecognizable.
    async fn check(
        &self,
        descriptor_key: &str,
        descriptor_value: &str,
    ) -> Result<CheckOutcome, BackendError>;

    /// Debit `delta` against the descriptor, recording the spend.
    ///
    /// # Errors
    ///
    /// [`BackendError`] when the call cannot complete. The caller logs it and
    /// never denies.
    async fn report(
        &self,
        descriptor_key: &str,
        descriptor_value: &str,
        delta: u64,
    ) -> Result<(), BackendError>;
}
