//! Error types for the device sync crate.

use thiserror::Error;

/// Result type alias for device sync operations.
pub type Result<T> = std::result::Result<T, DeviceSyncError>;

/// Retry policy class for API failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiRetryClass {
    Retryable,
    Permanent,
    ReauthRequired,
}

/// Errors that can occur during device sync operations.
#[derive(Debug, Error)]
pub enum DeviceSyncError {
    /// HTTP client error
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// API error response from the cloud service
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },

    /// Invalid request (missing required data, etc.)
    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    /// Authentication error (missing or invalid token)
    #[error("Authentication error: {0}")]
    Auth(String),
}

impl DeviceSyncError {
    /// Create an API error from status and message
    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            status,
            message: message.into(),
        }
    }

    /// Create an invalid request error
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    /// Create an auth error
    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth(message.into())
    }

    /// HTTP status if this is an API error.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Classify error for retry policy.
    pub fn retry_class(&self) -> ApiRetryClass {
        match self {
            Self::Api { status, .. } => match *status {
                401 | 403 => ApiRetryClass::ReauthRequired,
                408 | 409 | 423 | 425 | 429 => ApiRetryClass::Retryable,
                500..=599 => ApiRetryClass::Retryable,
                _ => ApiRetryClass::Permanent,
            },
            Self::Http(_) => ApiRetryClass::Retryable,
            Self::Json(_) => ApiRetryClass::Permanent,
            Self::InvalidRequest(_) => ApiRetryClass::Permanent,
            Self::Auth(_) => ApiRetryClass::ReauthRequired,
        }
    }

    /// Returns true when server-side validation rejected snapshotId UUID format.
    pub fn is_snapshot_id_validation_error(&self) -> bool {
        match self {
            Self::Api { status, message } => {
                *status == 400
                    && message.contains("snapshotId")
                    && (message.contains("Invalid UUID") || message.contains("invalid_format"))
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_validation_error_detected() {
        let err = DeviceSyncError::api(
            400,
            "Request failed: {\"path\":[\"snapshotId\"],\"message\":\"Invalid UUID\"}",
        );
        assert!(err.is_snapshot_id_validation_error());
    }

    #[test]
    fn retry_class_for_auth_error_is_reauth() {
        let err = DeviceSyncError::api(401, "unauthorized");
        assert_eq!(err.retry_class(), ApiRetryClass::ReauthRequired);
    }
}
