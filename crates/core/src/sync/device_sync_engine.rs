//! Core helpers for device sync engine orchestration.

use serde::{Deserialize, Serialize};

/// Retry policy classification for API failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRetryClass {
    Retryable,
    Permanent,
    ReauthRequired,
}

/// Lightweight cycle metrics emitted by orchestrators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCycleMetrics {
    pub pushed_count: usize,
    pub pulled_count: usize,
    pub duration_ms: i64,
    pub status: String,
}

/// Classify HTTP status into retry behavior.
pub fn classify_http_status(status: u16) -> SyncRetryClass {
    match status {
        401 | 403 => SyncRetryClass::ReauthRequired,
        408 | 409 | 423 | 425 | 429 => SyncRetryClass::Retryable,
        500..=599 => SyncRetryClass::Retryable,
        _ => SyncRetryClass::Permanent,
    }
}

/// Exponential backoff in seconds with cap.
pub fn backoff_seconds(consecutive_failures: i32) -> i64 {
    const MAX_EXPONENT: i32 = 8;
    const BASE_DELAY_SECONDS: i64 = 5;

    let capped = i64::from(consecutive_failures.clamp(0, MAX_EXPONENT));
    2_i64.pow(capped as u32) * BASE_DELAY_SECONDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_http_status_for_retry_policy() {
        assert_eq!(classify_http_status(500), SyncRetryClass::Retryable);
        assert_eq!(classify_http_status(429), SyncRetryClass::Retryable);
        assert_eq!(classify_http_status(401), SyncRetryClass::ReauthRequired);
        assert_eq!(classify_http_status(400), SyncRetryClass::Permanent);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_seconds(0), 5);
        assert_eq!(backoff_seconds(1), 10);
        assert_eq!(backoff_seconds(2), 20);
        assert_eq!(backoff_seconds(9), backoff_seconds(8));
    }
}
