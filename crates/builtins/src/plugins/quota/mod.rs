// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! Experimental per-consumer token quota, enforced as a policy against a
//! standalone Limitador. A pre-invoke check on `cmf.llm_input` and a post-invoke
//! debit on `cmf.llm_output`, keyed on the resolved identity. The counter lives
//! in Limitador, so the budget survives restarts and stays correct across replicas.
//!
//! Stability: experimental (`experimental-quota` feature). Not covered by semver;
//! not for production.
//!
//! Config must declare `capabilities: [read_subject]`, plus `read_claims` for a
//! claim key. An undeclared identity filters to `None` and nothing meters.
//!
//! Trust boundary: the debited amount is the upstream's self-reported usage.
//! When usage cannot be determined (streaming, or an absent field) the plugin
//! debits a conservative `missing_usage_charge`, not nothing, so the balance
//! always moves. `identity_claim` must name a verified subject id.
//!
//! Fail-closed reconciliation: a failed `/report` is recorded per principal and
//! re-reported on the next admission, which is denied until it lands. Limitador
//! `/report` is not idempotent, so a retry after an ambiguous loss may over-charge
//! (the safe direction). The guard state is in-process: lost on restart and not
//! shared across replicas, so the residual leak is bounded per replica, not the
//! unbounded free-request stream a dropped debit caused before. The Limitador
//! counter stays the source of truth.
//!
//! Transport: the plugin performs no HTTP itself; it uses the host transport from
//! `Extensions`. An in-cluster Limitador needs a transport that allows private
//! destinations (`HyperTransport::with_allow_private_destinations`); the default
//! helper refuses RFC 1918 addresses.
//!
//! Consistency: the check precedes the request and the debit follows the response,
//! so concurrent requests can briefly burst past the budget before the counter
//! catches up. Strict pre-charge admission needs Limitador's gRPC Reserve and
//! Commit, absent from released Limitador. Deployment requires an authenticated,
//! network-restricted Limitador.

/// The backend contract: the trait, its verdict, and its error.
pub mod backend;
/// Plugin configuration and its validation.
pub mod config;
/// Constructs the plugin from configuration.
pub mod factory;
/// The pre-invoke check and post-invoke debit hook handlers.
pub mod handlers;

// Private so no Limitador type reaches the public surface.
mod client;

pub use backend::{BackendError, CheckOutcome, QuotaBackend};
pub use config::{OnErrorMode, QuotaConfig};
pub use factory::{KIND, QuotaFactory};
pub use handlers::{
    CODE_QUOTA_BACKEND_UNAVAILABLE, CODE_QUOTA_EXHAUSTED, Quota, QuotaCheck, QuotaReport,
};
