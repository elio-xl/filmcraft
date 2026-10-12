//! Read-only, bounded analysis of project source media.

pub mod frames;
pub mod request;
pub mod source;

/// Machine-readable failure shared by analysis jobs and per-frame results.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AnalysisFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[cfg(test)]
mod tests;
