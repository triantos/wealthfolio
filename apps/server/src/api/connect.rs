//! Wealthfolio Connect API endpoints for broker synchronization.
//!
//! This module provides REST endpoints for syncing broker accounts and activities
//! from the Wealthfolio Connect cloud service.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Query, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::error::{ApiError, ApiResult};
use crate::events::{
    EventBus, ServerEvent, BROKER_SYNC_COMPLETE, BROKER_SYNC_ERROR, BROKER_SYNC_START,
};
use crate::main_lib::AppState;
use axum::http::StatusCode;
use wealthfolio_connect::{
    broker::{
        BrokerApiClient, PlansResponse, SyncAccountsResponse, SyncActivitiesResponse,
        SyncConnectionsResponse, UserInfo,
    },
    fetch_subscription_plans_public, ConnectApiClient, SyncConfig, SyncOrchestrator,
    SyncProgressPayload, SyncProgressReporter, SyncResult, DEFAULT_CLOUD_API_URL,
};
use wealthfolio_core::accounts::TrackingMode;
use wealthfolio_device_sync::{EnableSyncResult, SyncStateResult};

// Storage keys (without prefix - the SecretStore adds "wealthfolio_" prefix)
const CLOUD_REFRESH_TOKEN_KEY: &str = "sync_refresh_token";
const CLOUD_ACCESS_TOKEN_KEY: &str = "sync_access_token";

/// Seconds before actual expiry to treat a cached token as expired (buffer for clock skew / latency).
const TOKEN_EXPIRY_BUFFER_SECS: u64 = 60;
/// Default TTL assumed when storing a token received from the frontend (no expires_in available).
const DEFAULT_TOKEN_TTL_SECS: u64 = 55 * 60;

/// Default Supabase auth URL for token refresh
const DEFAULT_SUPABASE_AUTH_URL: &str = "https://vvalcadcvxqwligwzxaw.supabase.co";

fn cloud_api_base_url() -> String {
    std::env::var("CONNECT_API_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CLOUD_API_URL.to_string())
}

fn supabase_auth_url() -> String {
    std::env::var("CONNECT_AUTH_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SUPABASE_AUTH_URL.to_string())
}

fn supabase_api_key() -> Option<String> {
    std::env::var("CONNECT_AUTH_PUBLISHABLE_KEY").ok()
}

/// Create a ConnectApiClient with a fresh access token
async fn create_connect_client(state: &AppState) -> ApiResult<ConnectApiClient> {
    let token = mint_access_token(state).await?;
    ConnectApiClient::new(&cloud_api_base_url(), &token)
        .map_err(|e| ApiError::Internal(e.to_string()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Request/Response Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSyncSessionRequest {
    pub access_token: Option<String>,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct SupabaseTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SupabaseErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSessionStatus {
    pub is_configured: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSyncEngineStatusResponse {
    cursor: i64,
    last_push_at: Option<String>,
    last_pull_at: Option<String>,
    last_error: Option<String>,
    consecutive_failures: i32,
    next_retry_at: Option<String>,
    last_cycle_status: Option<String>,
    last_cycle_duration_ms: Option<i64>,
    background_running: bool,
    bootstrap_required: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSyncBootstrapResponse {
    status: String,
    message: String,
    snapshot_id: Option<String>,
    cursor: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSyncCycleResponse {
    status: String,
    lock_version: i64,
    pushed_count: usize,
    pulled_count: usize,
    cursor: i64,
    needs_bootstrap: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSyncBackgroundResponse {
    status: String,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceSyncSnapshotUploadResponse {
    status: String,
    snapshot_id: Option<String>,
    oplog_seq: Option<i64>,
    message: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// EventBus Progress Reporter
// ─────────────────────────────────────────────────────────────────────────────

/// Progress reporter that publishes events to the EventBus for SSE delivery.
struct EventBusProgressReporter {
    event_bus: EventBus,
}

impl EventBusProgressReporter {
    fn new(event_bus: EventBus) -> Self {
        Self { event_bus }
    }
}

impl SyncProgressReporter for EventBusProgressReporter {
    fn report_progress(&self, payload: SyncProgressPayload) {
        self.event_bus.publish(ServerEvent::with_payload(
            "sync-progress",
            serde_json::to_value(&payload).unwrap_or_default(),
        ));
    }

    fn report_sync_start(&self) {
        self.event_bus.publish(ServerEvent::new(BROKER_SYNC_START));
    }

    fn report_sync_complete(&self, result: &SyncResult) {
        if result.success {
            self.event_bus.publish(ServerEvent::with_payload(
                BROKER_SYNC_COMPLETE,
                serde_json::to_value(result).unwrap_or_default(),
            ));
        } else {
            self.event_bus.publish(ServerEvent::with_payload(
                BROKER_SYNC_ERROR,
                serde_json::json!({ "error": result.message }),
            ));
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Session Management
// ─────────────────────────────────────────────────────────────────────────────

async fn store_sync_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StoreSyncSessionRequest>,
) -> ApiResult<Json<()>> {
    info!("[Connect] Storing sync session");

    state
        .secret_store
        .set_secret(CLOUD_REFRESH_TOKEN_KEY, &body.refresh_token)
        .map_err(|e| ApiError::Internal(format!("Failed to store refresh token: {}", e)))?;

    // Also persist the access token so DeviceEnrollService (which reads it directly from the
    // store) can function immediately without a round-trip to Supabase.
    if let Some(ref access_token) = body.access_token {
        if !access_token.is_empty() {
            state
                .secret_store
                .set_secret(CLOUD_ACCESS_TOKEN_KEY, access_token)
                .map_err(|e| ApiError::Internal(format!("Failed to store access token: {}", e)))?;

            // Populate in-memory cache. The frontend doesn't send expires_in, so use a
            // conservative default (55 min) that keeps us safely within Supabase's 1-hour TTL.
            let expires_at = Instant::now() + Duration::from_secs(DEFAULT_TOKEN_TTL_SECS);
            let mut cache = state.token_cache.write().await;
            *cache = Some(crate::main_lib::CachedAccessToken {
                token: access_token.clone(),
                expires_at,
            });
        }
    }

    info!("[Connect] Sync session stored successfully");
    Ok(Json(()))
}

async fn clear_sync_session(State(state): State<Arc<AppState>>) -> ApiResult<Json<()>> {
    info!("[Connect] Clearing sync session");

    let _ = state.secret_store.delete_secret(CLOUD_REFRESH_TOKEN_KEY);
    let _ = state.secret_store.delete_secret(CLOUD_ACCESS_TOKEN_KEY);

    // Clear in-memory cache
    let mut cache = state.token_cache.write().await;
    *cache = None;

    info!("[Connect] Sync session cleared");
    Ok(Json(()))
}

async fn get_sync_session_status(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<SyncSessionStatus>> {
    let is_configured = state
        .secret_store
        .get_secret(CLOUD_REFRESH_TOKEN_KEY)
        .map(|t| t.is_some())
        .unwrap_or(false);

    Ok(Json(SyncSessionStatus { is_configured }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Token Management
// ─────────────────────────────────────────────────────────────────────────────

/// Return a valid access token, using the in-memory cache when possible.
///
/// - Cache hit (token not yet expired): returns immediately, no network call.
/// - Cache miss / expired: exchanges the stored refresh token with Supabase, then:
///   - Persists the rotated refresh token (Supabase invalidates the old one on each use).
///   - Persists the new access token so `DeviceEnrollService` (which reads it from the store
///     directly) stays in sync after a background refresh.
///   - Updates the in-memory cache for subsequent requests.
///
/// A write-lock is held across the Supabase call to prevent concurrent refresh storms.
pub(super) async fn mint_access_token(state: &AppState) -> ApiResult<String> {
    // Fast path: check cache under a read lock.
    {
        let cache = state.token_cache.read().await;
        if let Some(ref cached) = *cache {
            if cached.expires_at > Instant::now() {
                return Ok(cached.token.clone());
            }
        }
    }

    // Slow path: acquire write lock, double-check, then refresh.
    let mut cache = state.token_cache.write().await;
    if let Some(ref cached) = *cache {
        if cached.expires_at > Instant::now() {
            return Ok(cached.token.clone());
        }
    }

    let refresh_token = state
        .secret_store
        .get_secret(CLOUD_REFRESH_TOKEN_KEY)
        .map_err(|e| ApiError::Internal(format!("Failed to get refresh token: {}", e)))?
        .ok_or_else(|| {
            ApiError::Unauthorized("No refresh token configured. Please sign in first.".to_string())
        })?;

    let auth_url = supabase_auth_url();
    let api_key = supabase_api_key().ok_or_else(|| {
        ApiError::Internal("CONNECT_AUTH_PUBLISHABLE_KEY not configured".to_string())
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| ApiError::Internal(format!("Failed to create HTTP client: {}", e)))?;

    let token_url = format!("{}/auth/v1/token?grant_type=refresh_token", auth_url);
    debug!("[Connect] Refreshing access token from Supabase");

    let response = client
        .post(&token_url)
        .header("apikey", &api_key)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to refresh token: {}", e)))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to read response: {}", e)))?;

    if !status.is_success() {
        if let Ok(err) = serde_json::from_str::<SupabaseErrorResponse>(&body) {
            let msg = err
                .error_description
                .or(err.error)
                .unwrap_or_else(|| "Unknown error".to_string());
            error!("[Connect] Token refresh failed: {}", msg);
            return Err(ApiError::Unauthorized(format!(
                "Session expired. Please sign in again. ({})",
                msg
            )));
        }
        error!(
            "[Connect] Token refresh failed with status {}: {}",
            status, body
        );
        return Err(ApiError::Unauthorized(
            "Session expired. Please sign in again.".to_string(),
        ));
    }

    let token_response: SupabaseTokenResponse = serde_json::from_str(&body)
        .map_err(|e| ApiError::Internal(format!("Failed to parse token response: {}", e)))?;

    // Persist the rotated refresh token — Supabase invalidates the old one on each use.
    state
        .secret_store
        .set_secret(CLOUD_REFRESH_TOKEN_KEY, &token_response.refresh_token)
        .map_err(|e| ApiError::Internal(format!("Failed to store refresh token: {}", e)))?;

    // Persist the new access token so DeviceEnrollService (reads from store directly) stays in sync.
    state
        .secret_store
        .set_secret(CLOUD_ACCESS_TOKEN_KEY, &token_response.access_token)
        .map_err(|e| ApiError::Internal(format!("Failed to store access token: {}", e)))?;

    // Update in-memory cache. Apply buffer so we refresh before actual expiry.
    let ttl =
        (token_response.expires_in.unwrap_or(3600) as u64).saturating_sub(TOKEN_EXPIRY_BUFFER_SECS);
    let expires_at = Instant::now() + Duration::from_secs(ttl);
    *cache = Some(crate::main_lib::CachedAccessToken {
        token: token_response.access_token.clone(),
        expires_at,
    });

    debug!("[Connect] Access token refreshed and cached (TTL {}s)", ttl);
    Ok(token_response.access_token)
}

// ─────────────────────────────────────────────────────────────────────────────
// Sync Operations
// ─────────────────────────────────────────────────────────────────────────────

async fn sync_broker_connections(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<SyncConnectionsResponse>> {
    info!("[Connect] Syncing broker connections...");

    let client = create_connect_client(&state).await?;

    // Fetch connections from cloud using the shared client
    let connections = client
        .list_connections()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!(
        "[Connect] Fetched {} connections from cloud",
        connections.len()
    );

    // Sync to local database
    let result = state
        .connect_sync_service
        .sync_connections(connections)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!(
        "[Connect] Synced connections: {} platforms created, {} updated",
        result.platforms_created, result.platforms_updated
    );

    Ok(Json(result))
}

async fn sync_broker_accounts(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<SyncAccountsResponse>> {
    info!("[Connect] Syncing broker accounts...");

    let client = create_connect_client(&state).await?;

    // Fetch accounts from cloud using the shared client
    let accounts = client
        .list_accounts(None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!("[Connect] Fetched {} accounts from cloud", accounts.len());

    // Sync to local database
    let result = state
        .connect_sync_service
        .sync_accounts(accounts)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!(
        "[Connect] Synced accounts: {} created, {} updated, {} skipped",
        result.created, result.updated, result.skipped
    );

    Ok(Json(result))
}

async fn sync_broker_activities(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<SyncActivitiesResponse>> {
    info!("[Connect] Running activities-only broker sync");
    let result = perform_broker_activities_only_sync(&state)
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(result))
}

// ─────────────────────────────────────────────────────────────────────────────
// Unified Sync Operation (non-blocking with SSE notifications)
// ─────────────────────────────────────────────────────────────────────────────

/// Trigger a full broker data sync (connections → accounts → activities).
/// Returns immediately with 202 Accepted. Sync runs in background and emits SSE events.
async fn sync_broker_data(State(state): State<Arc<AppState>>) -> StatusCode {
    info!("[Connect] Starting broker data sync (non-blocking)...");

    // Spawn background task to perform the sync
    tokio::spawn(async move {
        match perform_broker_sync(&state).await {
            Ok(_result) => {
                info!("[Connect] Broker sync completed successfully");
                // Events are emitted by the orchestrator via EventBusProgressReporter
            }
            Err(err) => {
                error!("[Connect] Broker sync failed: {}", err);
                // Error event is also emitted by the orchestrator
            }
        }
    });

    StatusCode::ACCEPTED
}

/// Core broker sync logic - syncs connections, accounts, and activities from cloud to local DB.
/// Uses the centralized SyncOrchestrator for full pagination support.
/// Also used by the background scheduler for periodic syncs.
pub async fn perform_broker_sync(state: &AppState) -> Result<SyncResult, String> {
    // Create API client
    let client = create_connect_client(state)
        .await
        .map_err(|e| e.to_string())?;

    // Create progress reporter and orchestrator
    let reporter = Arc::new(EventBusProgressReporter::new(state.event_bus.clone()));
    let orchestrator = SyncOrchestrator::new(
        state.connect_sync_service.clone(),
        reporter,
        SyncConfig::default(),
    );

    // Run the sync via the centralized orchestrator
    // Note: Asset enrichment is handled automatically via domain events (AssetsCreated)
    orchestrator.sync_all(&client).await
}

/// Sync only brokerage activities for existing TRANSACTIONS accounts.
/// This preserves legacy /connect/sync/activities behavior (no connections/accounts/holdings sync).
async fn perform_broker_activities_only_sync(
    state: &AppState,
) -> Result<SyncActivitiesResponse, String> {
    let client = create_connect_client(state)
        .await
        .map_err(|e| e.to_string())?;

    let synced_accounts = state
        .connect_sync_service
        .get_synced_accounts()
        .map_err(|e| e.to_string())?;

    let mut summary = SyncActivitiesResponse::default();
    let end_date = chrono::Utc::now().date_naive();
    let end_date_str = end_date.format("%Y-%m-%d").to_string();
    let page_limit: i64 = 1000;

    for account in synced_accounts {
        if account.tracking_mode != TrackingMode::Transactions {
            continue;
        }

        let Some(provider_account_id) = account.provider_account_id.clone() else {
            continue;
        };

        let account_id = account.id.clone();
        let account_name = account.name.clone();

        if let Err(err) = state
            .connect_sync_service
            .mark_activity_sync_attempt(account_id.clone())
            .await
        {
            error!(
                "[Connect] Failed to mark activity sync attempt for {}: {}",
                account_name, err
            );
            summary.accounts_failed += 1;
            continue;
        }

        let start_date = state
            .connect_sync_service
            .get_activity_sync_state(&account_id)
            .map_err(|e| e.to_string())?
            .and_then(|s| s.last_successful_at)
            .map(|dt| (dt.date_naive() - chrono::Days::new(1)).min(end_date))
            .map(|d| d.format("%Y-%m-%d").to_string());

        info!(
            "[Connect] Activities-only sync for account '{}' (provider={}): {} -> {}",
            account_name,
            provider_account_id,
            start_date.as_deref().unwrap_or("ALL"),
            end_date_str
        );

        let mut offset: i64 = 0;
        let mut account_upserted = 0usize;
        let mut account_assets = 0usize;
        let mut account_new_asset_ids: Vec<String> = Vec::new();
        let mut account_failed = false;

        loop {
            let page = match client
                .get_account_activities(
                    &provider_account_id,
                    start_date.as_deref(),
                    Some(&end_date_str),
                    Some(offset),
                    Some(page_limit),
                )
                .await
            {
                Ok(page) => page,
                Err(err) => {
                    error!(
                        "[Connect] Failed to fetch activities for {}: {}",
                        account_name, err
                    );
                    let _ = state
                        .connect_sync_service
                        .finalize_activity_sync_failure(account_id.clone(), err.to_string(), None)
                        .await;
                    summary.accounts_failed += 1;
                    account_failed = true;
                    break;
                }
            };

            let received = page.data.len() as i64;
            if received == 0 {
                break;
            }

            match state
                .connect_sync_service
                .upsert_account_activities(account_id.clone(), None, page.data)
                .await
            {
                Ok((upserted, assets, new_asset_ids, _needs_review)) => {
                    account_upserted += upserted;
                    account_assets += assets;
                    account_new_asset_ids.extend(new_asset_ids);
                }
                Err(err) => {
                    error!(
                        "[Connect] Failed to upsert activities for {}: {}",
                        account_name, err
                    );
                    let _ = state
                        .connect_sync_service
                        .finalize_activity_sync_failure(account_id.clone(), err.to_string(), None)
                        .await;
                    summary.accounts_failed += 1;
                    account_failed = true;
                    break;
                }
            }

            let next_offset = offset + received;
            let has_more = match page.pagination.as_ref() {
                Some(p) => match p.has_more {
                    Some(true) => true,
                    Some(false) => false,
                    None => {
                        if let Some(total) = p.total {
                            next_offset < total
                        } else if let Some(limit) = p.limit {
                            received >= limit
                        } else {
                            received >= page_limit
                        }
                    }
                },
                None => received >= page_limit,
            };

            offset = next_offset;

            if !has_more {
                break;
            }
        }

        if account_failed {
            continue;
        }

        let now = chrono::Utc::now().to_rfc3339();
        if let Err(err) = state
            .connect_sync_service
            .finalize_activity_sync_success(account_id.clone(), now, None)
            .await
        {
            error!(
                "[Connect] Failed to finalize activity sync success for {}: {}",
                account_name, err
            );
            summary.accounts_failed += 1;
            continue;
        }

        summary.accounts_synced += 1;
        summary.activities_upserted += account_upserted;
        summary.assets_inserted += account_assets;
        summary.new_asset_ids.extend(account_new_asset_ids);
    }

    Ok(summary)
}

async fn get_subscription_plans(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<PlansResponse>> {
    info!("[Connect] Getting subscription plans...");

    let client = create_connect_client(&state).await?;

    let plans = client
        .get_subscription_plans()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(plans))
}

async fn get_subscription_plans_public() -> ApiResult<Json<PlansResponse>> {
    info!("[Connect] Getting subscription plans (public)...");

    let base_url = cloud_api_base_url();

    let plans = fetch_subscription_plans_public(&base_url)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(plans))
}

async fn get_user_info(State(state): State<Arc<AppState>>) -> ApiResult<Json<UserInfo>> {
    info!("[Connect] Getting user info...");

    let client = create_connect_client(&state).await?;

    let user_info = client
        .get_user_info()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(user_info))
}

// ─────────────────────────────────────────────────────────────────────────────
// List Operations (fetch from cloud without syncing to local)
// ─────────────────────────────────────────────────────────────────────────────

async fn list_broker_connections(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<wealthfolio_connect::broker::BrokerConnection>>> {
    info!("[Connect] Listing broker connections from cloud...");

    let client = create_connect_client(&state).await?;

    let connections = client
        .list_connections()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!("[Connect] Found {} broker connections", connections.len());
    Ok(Json(connections))
}

async fn list_broker_accounts(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<wealthfolio_connect::broker::BrokerAccount>>> {
    info!("[Connect] Listing broker accounts from cloud...");

    let client = create_connect_client(&state).await?;

    let accounts = client
        .list_accounts(None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!("[Connect] Found {} broker accounts", accounts.len());
    Ok(Json(accounts))
}

// ─────────────────────────────────────────────────────────────────────────────
// Local Data Queries (from local database, not cloud)
// ─────────────────────────────────────────────────────────────────────────────

/// Request params for get_import_runs
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetImportRunsQuery {
    pub run_type: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Get all synced accounts (accounts with provider_account_id set)
async fn get_synced_accounts(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<wealthfolio_core::accounts::Account>>> {
    info!("[Connect] Getting synced accounts from local database...");

    let accounts = state
        .connect_sync_service
        .get_synced_accounts()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!("[Connect] Found {} synced accounts", accounts.len());
    Ok(Json(accounts))
}

/// Get all platforms from local database
async fn get_platforms(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<wealthfolio_connect::Platform>>> {
    info!("[Connect] Getting platforms from local database...");

    let platforms = state
        .connect_sync_service
        .get_platforms()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    info!("[Connect] Found {} platforms", platforms.len());
    Ok(Json(platforms))
}

/// Get all broker sync states from local database
async fn get_broker_sync_states(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<Vec<wealthfolio_core::sync::BrokerSyncState>>> {
    debug!("[Connect] Getting broker sync states from local database...");

    let states = state
        .connect_sync_service
        .get_all_sync_states()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(states))
}

/// Get import runs with optional type filter and pagination
async fn get_import_runs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GetImportRunsQuery>,
) -> ApiResult<Json<Vec<wealthfolio_core::sync::ImportRun>>> {
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    debug!(
        "[Connect] Getting import runs (type={:?}, limit={}, offset={})...",
        query.run_type, limit, offset
    );

    let runs = state
        .connect_sync_service
        .get_import_runs(query.run_type.as_deref(), limit, offset)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(runs))
}

// ─────────────────────────────────────────────────────────────────────────────
// Device Enroll Operations
// ─────────────────────────────────────────────────────────────────────────────

/// Get the current device sync state (FRESH, REGISTERED, READY, STALE, RECOVERY, ORPHANED)
async fn get_device_sync_state(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<SyncStateResult>> {
    info!("[Connect] Getting device sync state...");
    // Ensure store has a fresh access token for DeviceEnrollService (reads from store directly).
    mint_access_token(&state).await?;

    let result = state
        .device_enroll_service
        .get_sync_state()
        .await
        .map_err(|e| ApiError::Internal(e.message))?;

    Ok(Json(result))
}

/// Enable device sync - enrolls the device and initializes E2EE if first device
async fn enable_device_sync(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<EnableSyncResult>> {
    info!("[Connect] Enabling device sync...");
    mint_access_token(&state).await?;

    let result = state
        .device_enroll_service
        .enable_sync()
        .await
        .map_err(|e| ApiError::Internal(e.message))?;

    info!("[Connect] Device sync enabled successfully");
    Ok(Json(result))
}

/// Clear all device sync data and return to FRESH state
async fn clear_device_sync_data(State(state): State<Arc<AppState>>) -> ApiResult<Json<()>> {
    info!("[Connect] Clearing device sync data...");

    state
        .device_enroll_service
        .clear_sync_data()
        .map_err(|e| ApiError::Internal(e.message))?;

    info!("[Connect] Device sync data cleared");
    Ok(Json(()))
}

/// Reinitialize device sync - resets server data and enables sync in one operation
async fn reinitialize_device_sync(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<EnableSyncResult>> {
    info!("[Connect] Reinitializing device sync...");
    mint_access_token(&state).await?;

    let result = state
        .device_enroll_service
        .reinitialize_sync()
        .await
        .map_err(|e| ApiError::Internal(e.message))?;

    info!("[Connect] Device sync reinitialized successfully");
    Ok(Json(result))
}

/// Web runtime stub for device sync engine status (desktop engine owns authoritative state).
async fn get_device_sync_engine_status() -> ApiResult<Json<DeviceSyncEngineStatusResponse>> {
    Ok(Json(DeviceSyncEngineStatusResponse {
        cursor: 0,
        last_push_at: None,
        last_pull_at: None,
        last_error: None,
        consecutive_failures: 0,
        next_retry_at: None,
        last_cycle_status: Some("web_stub".to_string()),
        last_cycle_duration_ms: None,
        background_running: false,
        bootstrap_required: false,
    }))
}

/// Web runtime stub for device snapshot bootstrap.
async fn bootstrap_device_snapshot() -> ApiResult<Json<DeviceSyncBootstrapResponse>> {
    Ok(Json(DeviceSyncBootstrapResponse {
        status: "skipped".to_string(),
        message: "Snapshot bootstrap is desktop-only".to_string(),
        snapshot_id: None,
        cursor: None,
    }))
}

/// Web runtime stub for one sync cycle trigger.
async fn trigger_device_sync_cycle() -> ApiResult<Json<DeviceSyncCycleResponse>> {
    Ok(Json(DeviceSyncCycleResponse {
        status: "skipped".to_string(),
        lock_version: 0,
        pushed_count: 0,
        pulled_count: 0,
        cursor: 0,
        needs_bootstrap: false,
    }))
}

async fn start_device_sync_background_engine() -> ApiResult<Json<DeviceSyncBackgroundResponse>> {
    Ok(Json(DeviceSyncBackgroundResponse {
        status: "started".to_string(),
        message: "Web runtime keeps sync engine as stub".to_string(),
    }))
}

async fn stop_device_sync_background_engine() -> ApiResult<Json<DeviceSyncBackgroundResponse>> {
    Ok(Json(DeviceSyncBackgroundResponse {
        status: "stopped".to_string(),
        message: "Web runtime keeps sync engine as stub".to_string(),
    }))
}

async fn generate_device_snapshot_now() -> ApiResult<Json<DeviceSyncSnapshotUploadResponse>> {
    Ok(Json(DeviceSyncSnapshotUploadResponse {
        status: "skipped".to_string(),
        snapshot_id: None,
        oplog_seq: None,
        message: "Snapshot upload is desktop-only".to_string(),
    }))
}

async fn cancel_device_snapshot_upload() -> ApiResult<Json<DeviceSyncBackgroundResponse>> {
    Ok(Json(DeviceSyncBackgroundResponse {
        status: "cancel_requested".to_string(),
        message: "Web runtime keeps snapshot upload cancellation as stub".to_string(),
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Router
// ─────────────────────────────────────────────────────────────────────────────

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // Session management
        .route("/connect/session", post(store_sync_session))
        .route("/connect/session", delete(clear_sync_session))
        .route("/connect/session/status", get(get_sync_session_status))
        // List operations (fetch from cloud without syncing)
        .route("/connect/connections", get(list_broker_connections))
        .route("/connect/accounts", get(list_broker_accounts))
        // Unified sync (non-blocking, emits SSE events)
        .route("/connect/sync", post(sync_broker_data))
        // Individual sync operations (kept for backwards compatibility)
        .route("/connect/sync/connections", post(sync_broker_connections))
        .route("/connect/sync/accounts", post(sync_broker_accounts))
        .route("/connect/sync/activities", post(sync_broker_activities))
        // Local data queries (from local database)
        .route("/connect/synced-accounts", get(get_synced_accounts))
        .route("/connect/platforms", get(get_platforms))
        .route("/connect/sync-states", get(get_broker_sync_states))
        .route("/connect/import-runs", get(get_import_runs))
        // User & Subscription
        .route("/connect/plans", get(get_subscription_plans))
        .route("/connect/plans/public", get(get_subscription_plans_public))
        .route("/connect/user", get(get_user_info))
        // Device Sync / Enrollment
        .route("/connect/device/sync-state", get(get_device_sync_state))
        .route("/connect/device/enable", post(enable_device_sync))
        .route("/connect/device/sync-data", delete(clear_device_sync_data))
        .route(
            "/connect/device/reinitialize",
            post(reinitialize_device_sync),
        )
        .route(
            "/connect/device/engine-status",
            get(get_device_sync_engine_status),
        )
        .route(
            "/connect/device/bootstrap-snapshot",
            post(bootstrap_device_snapshot),
        )
        .route(
            "/connect/device/trigger-cycle",
            post(trigger_device_sync_cycle),
        )
        .route(
            "/connect/device/start-background",
            post(start_device_sync_background_engine),
        )
        .route(
            "/connect/device/stop-background",
            post(stop_device_sync_background_engine),
        )
        .route(
            "/connect/device/generate-snapshot",
            post(generate_device_snapshot_now),
        )
        .route(
            "/connect/device/cancel-snapshot",
            post(cancel_device_snapshot_upload),
        )
}
