use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// A wire-stable failure returned by Tessivum protocol boundaries.
#[derive(Clone, Debug, Deserialize, Error, PartialEq, Serialize)]
#[error("{code}: {message}")]
pub struct TessivumError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Stable human-readable error summary.
    pub message: String,
    /// Stable pipeline phase that produced the error.
    pub phase: String,
    /// Lossless JSON facts specific to this failure.
    pub details: Value,
}

impl TessivumError {
    /// Creates a wire-stable error with caller-owned lossless details.
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        phase: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            phase: phase.into(),
            details,
        }
    }

    /// Creates a protocol validation error.
    pub fn protocol(code: impl Into<String>, message: impl Into<String>, details: Value) -> Self {
        Self::new(code, message, "protocol", details)
    }
}
