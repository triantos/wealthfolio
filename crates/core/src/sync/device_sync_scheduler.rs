//! Core scheduler constants/helpers for device sync.

/// Foreground pull cadence in seconds.
pub const DEVICE_SYNC_FOREGROUND_INTERVAL_SECS: u64 = 45;

/// Maximum jitter (seconds) added to periodic cycle intervals.
pub const DEVICE_SYNC_INTERVAL_JITTER_SECS: u64 = 5;

/// Snapshot generation cadence for trusted devices.
pub const DEVICE_SYNC_SNAPSHOT_INTERVAL_SECS: u64 = 60 * 60 * 24;

/// Number of new events after which snapshot generation should be considered.
pub const DEVICE_SYNC_SNAPSHOT_EVENT_THRESHOLD: i64 = 1000;
