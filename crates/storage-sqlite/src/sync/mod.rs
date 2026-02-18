//! SQLite storage implementation for sync (platforms, app sync state, import runs).

pub mod app_sync;
pub mod import_run;
pub mod platform;
pub mod state;

/// Broker ingest aliases. `import_run` includes both broker ingest and manual CSV imports.
pub mod broker_ingest {
    pub use super::import_run::{ImportRunDB, ImportRunRepository};
    pub use super::platform::{Platform, PlatformDB, PlatformRepository};
    pub use super::state::{BrokerSyncStateDB, BrokerSyncStateRepository};
}

// Re-export for convenience
pub use app_sync::{write_outbox_event, AppSyncRepository, OutboxWriteRequest};
pub use import_run::{ImportRunDB, ImportRunRepository};
pub use platform::{Platform, PlatformDB, PlatformRepository};
pub use state::{BrokerSyncStateDB, BrokerSyncStateRepository};

// Re-export domain model from core
pub use wealthfolio_core::sync::BrokerSyncState;
