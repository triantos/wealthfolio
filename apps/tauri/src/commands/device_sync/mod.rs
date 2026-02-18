//! Commands for device sync and E2EE pairing.
//!
//! This module provides Tauri commands that wrap the shared device sync client,
//! handling token/device ID storage via the keyring.

mod engine;
mod snapshot;

use log::{debug, info};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{AppHandle, State};

use crate::context::ServiceContext;
use crate::secret_store::KeyringSecretStore;
use wealthfolio_core::secrets::SecretStore;
use wealthfolio_core::sync::{SyncEntity, SyncOperation, APP_SYNC_TABLES};
use wealthfolio_device_sync::{
    ClaimPairingRequest, ClaimPairingResponse, CommitInitializeKeysRequest,
    CommitInitializeKeysResponse, CommitRotateKeysRequest, CommitRotateKeysResponse,
    CompletePairingRequest, ConfirmPairingRequest, ConfirmPairingResponse, CreatePairingRequest,
    CreatePairingResponse, Device, DevicePlatform, DeviceSyncClient, EnrollDeviceResponse,
    GetPairingResponse, InitializeKeysResult, PairingMessagesResponse, RegisterDeviceRequest,
    ResetTeamSyncResponse, RotateKeysResponse, SnapshotRequestPayload, SuccessResponse,
    UpdateDeviceRequest,
};

// Re-export public items consumed by lib.rs
pub use engine::{ensure_background_engine_started, ensure_background_engine_stopped};

// ─────────────────────────────────────────────────────────────────────────────
// Shared Constants & Helpers
// ─────────────────────────────────────────────────────────────────────────────

const CLOUD_ACCESS_TOKEN_KEY: &str = "sync_access_token";

fn cloud_api_base_url() -> Result<String, String> {
    std::env::var("CONNECT_API_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            "CONNECT_API_URL not configured. Connect API operations are disabled.".to_string()
        })
}

fn get_access_token() -> Result<String, String> {
    KeyringSecretStore
        .get_secret(CLOUD_ACCESS_TOKEN_KEY)
        .map_err(|e| format!("Failed to get access token: {}", e))?
        .ok_or_else(|| "No access token configured. Please sign in first.".to_string())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncIdentity {
    device_id: Option<String>,
    root_key: Option<String>,
    key_version: Option<i32>,
}

fn get_sync_identity_from_store() -> Option<SyncIdentity> {
    const SYNC_IDENTITY_KEY: &str = "sync_identity";

    match KeyringSecretStore.get_secret(SYNC_IDENTITY_KEY) {
        Ok(Some(json)) => match serde_json::from_str::<SyncIdentity>(&json) {
            Ok(identity) => {
                if let Some(ref device_id) = identity.device_id {
                    debug!(
                            "[DeviceSync] Loaded sync_identity (device_id={}, has_root_key={}, key_version={})",
                            device_id,
                            identity.root_key.is_some(),
                            identity.key_version.unwrap_or_default()
                        );
                } else {
                    debug!(
                            "[DeviceSync] sync_identity exists but deviceId is not set (has_root_key={}, key_version={})",
                            identity.root_key.is_some(),
                            identity.key_version.unwrap_or_default()
                        );
                }
                Some(identity)
            }
            Err(e) => {
                log::warn!("[DeviceSync] Failed to parse sync_identity: {}", e);
                None
            }
        },
        Ok(None) => {
            debug!("[DeviceSync] No sync_identity found in keyring");
            None
        }
        Err(e) => {
            log::warn!("[DeviceSync] Failed to read sync_identity: {}", e);
            None
        }
    }
}

fn get_device_id_from_store() -> Option<String> {
    get_sync_identity_from_store().and_then(|identity| identity.device_id)
}

async fn persist_device_config_from_identity(
    context: &ServiceContext,
    identity: &SyncIdentity,
    trust_state: &str,
) {
    if let Some(device_id) = &identity.device_id {
        if let Err(err) = context
            .app_sync_repository()
            .upsert_device_config(
                device_id.clone(),
                identity.key_version,
                trust_state.to_string(),
            )
            .await
        {
            log::warn!("[DeviceSync] Failed to persist device config: {}", err);
        }
    }
}

fn create_client() -> Result<DeviceSyncClient, String> {
    Ok(DeviceSyncClient::new(&cloud_api_base_url()?))
}

// ─────────────────────────────────────────────────────────────────────────────
// Result types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncBootstrapResult {
    pub status: String,
    pub message: String,
    pub snapshot_id: Option<String>,
    pub cursor: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncEngineStatusResult {
    pub cursor: i64,
    pub last_push_at: Option<String>,
    pub last_pull_at: Option<String>,
    pub last_error: Option<String>,
    pub consecutive_failures: i32,
    pub next_retry_at: Option<String>,
    pub last_cycle_status: Option<String>,
    pub last_cycle_duration_ms: Option<i64>,
    pub background_running: bool,
    pub bootstrap_required: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCycleResult {
    pub status: String,
    pub lock_version: i64,
    pub pushed_count: usize,
    pub pulled_count: usize,
    pub cursor: i64,
    pub needs_bootstrap: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncBackgroundEngineResult {
    pub status: String,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSnapshotUploadResult {
    pub status: String,
    pub snapshot_id: Option<String>,
    pub oplog_seq: Option<i64>,
    pub message: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared utility functions
// ─────────────────────────────────────────────────────────────────────────────

fn is_sqlite_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"SQLite format 3\0")
}

fn sha256_checksum(bytes: &[u8]) -> String {
    wealthfolio_device_sync::crypto::sha256_checksum(bytes)
}

fn sync_entity_name(entity: &SyncEntity) -> &'static str {
    match entity {
        SyncEntity::Account => "account",
        SyncEntity::Asset => "asset",
        SyncEntity::AssetTaxonomyAssignment => "asset_taxonomy_assignment",
        SyncEntity::Activity => "activity",
        SyncEntity::ActivityImportProfile => "activity_import_profile",
        SyncEntity::Goal => "goal",
        SyncEntity::GoalsAllocation => "goals_allocation",
        SyncEntity::AiThread => "ai_thread",
        SyncEntity::AiMessage => "ai_message",
        SyncEntity::AiThreadTag => "ai_thread_tag",
        SyncEntity::ContributionLimit => "contribution_limit",
        SyncEntity::Platform => "platform",
        SyncEntity::Snapshot => "snapshot",
    }
}

fn sync_operation_name(op: &SyncOperation) -> &'static str {
    match op {
        SyncOperation::Create => "create",
        SyncOperation::Update => "update",
        SyncOperation::Delete => "delete",
        SyncOperation::Request => "request",
    }
}

use wealthfolio_device_sync::ApiRetryClass;

fn retry_class_code(class: ApiRetryClass) -> &'static str {
    match class {
        ApiRetryClass::Retryable => "retryable",
        ApiRetryClass::Permanent => "permanent",
        ApiRetryClass::ReauthRequired => "reauth_required",
    }
}

fn parse_event_operation(event_type: &str) -> Option<SyncOperation> {
    let mut parts = event_type.split('.');
    let _entity = parts.next()?;
    match parts.next()? {
        "create" => Some(SyncOperation::Create),
        "update" => Some(SyncOperation::Update),
        "delete" => Some(SyncOperation::Delete),
        "request" => Some(SyncOperation::Request),
        _ => None,
    }
}

fn millis_until_rfc3339(target: &str) -> Option<u64> {
    let target = chrono::DateTime::parse_from_rfc3339(target).ok()?;
    let now = chrono::Utc::now();
    let diff = target.with_timezone(&chrono::Utc) - now;
    if diff <= chrono::Duration::zero() {
        return Some(0);
    }
    Some(diff.num_milliseconds() as u64)
}

fn encrypt_sync_payload(
    plaintext_payload: &str,
    identity: &SyncIdentity,
    payload_key_version: i32,
) -> Result<String, String> {
    let root_key = identity
        .root_key
        .as_ref()
        .ok_or_else(|| "Sync root key is not configured".to_string())?;
    let key_version = payload_key_version.max(1) as u32;
    let dek = wealthfolio_device_sync::crypto::derive_dek(root_key, key_version)
        .map_err(|e| format!("Failed to derive event DEK: {}", e))?;
    wealthfolio_device_sync::crypto::encrypt(&dek, plaintext_payload)
        .map_err(|e| format!("Failed to encrypt sync payload: {}", e))
}

fn decrypt_sync_payload(
    encrypted_payload: &str,
    identity: &SyncIdentity,
    payload_key_version: i32,
) -> Result<String, String> {
    let root_key = identity
        .root_key
        .as_ref()
        .ok_or_else(|| "Sync root key is not configured".to_string())?;
    let key_version = payload_key_version.max(1) as u32;
    let dek = wealthfolio_device_sync::crypto::derive_dek(root_key, key_version)
        .map_err(|e| format!("Failed to derive event DEK: {}", e))?;
    wealthfolio_device_sync::crypto::decrypt(&dek, encrypted_payload)
        .map_err(|e| format!("Failed to decrypt sync payload: {}", e))
}

async fn request_snapshot_generation(
    client: &DeviceSyncClient,
    token: &str,
    device_id: &str,
    identity: &SyncIdentity,
    message: &str,
) -> Result<SyncBootstrapResult, String> {
    let payload_key_version = identity.key_version.unwrap_or(1).max(1);
    let request_response = client
        .request_snapshot(
            token,
            device_id,
            SnapshotRequestPayload {
                min_schema_version: Some(1),
                covers_tables: Some(APP_SYNC_TABLES.iter().map(|v| v.to_string()).collect()),
                payload: encrypt_sync_payload("{}", identity, payload_key_version)?,
                payload_key_version,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    debug!(
        "[DeviceSync] Snapshot request accepted: request_id={} status={} message={}",
        request_response.request_id, request_response.status, request_response.message
    );
    debug!(
        "[DeviceSync] Requested snapshot generation; no local upload performed in this path (device_id={} request_id={})",
        device_id, request_response.request_id
    );

    Ok(SyncBootstrapResult {
        status: "requested".to_string(),
        message: message.to_string(),
        snapshot_id: None,
        cursor: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// OS version detection
// ─────────────────────────────────────────────────────────────────────────────

fn get_os_version() -> Option<String> {
    let version = get_os_version_impl();
    if version.is_none() {
        debug!("[DeviceSync] Could not detect OS version");
    }
    version
}

#[cfg(target_os = "macos")]
fn get_os_version_impl() -> Option<String> {
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "windows")]
fn get_os_version_impl() -> Option<String> {
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[System.Environment]::OSVersion.Version.ToString()",
        ])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "linux")]
fn get_os_version_impl() -> Option<String> {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|l| l.starts_with("VERSION_ID="))
                .map(|l| {
                    l.trim_start_matches("VERSION_ID=")
                        .trim_matches('"')
                        .to_string()
                })
        })
        .or_else(|| {
            std::fs::read_to_string("/etc/lsb-release")
                .ok()
                .and_then(|content| {
                    content
                        .lines()
                        .find(|l| l.starts_with("DISTRIB_RELEASE="))
                        .map(|l| {
                            l.trim_start_matches("DISTRIB_RELEASE=")
                                .trim_matches('"')
                                .to_string()
                        })
                })
        })
        .or_else(|| {
            Command::new("uname")
                .arg("-r")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "ios")]
fn get_os_version_impl() -> Option<String> {
    None
}

#[cfg(target_os = "android")]
fn get_os_version_impl() -> Option<String> {
    None
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "ios",
    target_os = "android"
)))]
fn get_os_version_impl() -> Option<String> {
    None
}

fn get_app_version() -> Option<String> {
    Some(env!("CARGO_PKG_VERSION").to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Device Management
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command(rename_all = "camelCase")]
pub async fn enroll_device(
    device_nonce: String,
    display_name: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<EnrollDeviceResponse, String> {
    info!("[DeviceSync] Enrolling device: {}", display_name);

    let token = get_access_token()?;
    let client = create_client()?;

    let platform = DevicePlatform::detect().to_string();
    let os_version = get_os_version();
    let app_version = get_app_version();

    info!(
        "[DeviceSync] Platform: {}, OS version: {:?}, App version: {:?}",
        platform, os_version, app_version
    );

    let request = RegisterDeviceRequest {
        device_nonce,
        display_name,
        platform,
        os_version,
        app_version,
    };

    let result = client
        .enroll_device(&token, request)
        .await
        .map_err(|e| e.to_string())?;

    let device_id = match &result {
        EnrollDeviceResponse::Bootstrap { device_id, .. } => device_id,
        EnrollDeviceResponse::Pair { device_id, .. } => device_id,
        EnrollDeviceResponse::Ready { device_id, .. } => device_id,
    };

    info!(
        "[DeviceSync] Device enrolled: {} (mode: {:?})",
        device_id,
        match &result {
            EnrollDeviceResponse::Bootstrap { .. } => "BOOTSTRAP",
            EnrollDeviceResponse::Pair { .. } => "PAIR",
            EnrollDeviceResponse::Ready { .. } => "READY",
        }
    );
    Ok(result)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_device(
    device_id: Option<String>,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<Device, String> {
    let token = get_access_token()?;
    let device_id = device_id
        .or_else(get_device_id_from_store)
        .ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .get_device(&token, &device_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_devices(
    scope: Option<String>,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<Vec<Device>, String> {
    info!("[DeviceSync] Listing devices (scope: {:?})...", scope);

    let token = get_access_token()?;

    let devices = create_client()?
        .list_devices(&token, scope.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    info!("[DeviceSync] Found {} devices", devices.len());
    Ok(devices)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn update_device(
    device_id: String,
    display_name: Option<String>,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<SuccessResponse, String> {
    info!(
        "[DeviceSync] Updating device {}: name={:?}",
        device_id, display_name
    );

    let token = get_access_token()?;

    create_client()?
        .update_device(
            &token,
            &device_id,
            UpdateDeviceRequest {
                display_name,
                metadata: None,
            },
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_device(
    device_id: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<SuccessResponse, String> {
    info!("[DeviceSync] Deleting device: {}", device_id);

    let token = get_access_token()?;

    create_client()?
        .delete_device(&token, &device_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn revoke_device(
    device_id: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<SuccessResponse, String> {
    info!("[DeviceSync] Revoking device: {}", device_id);

    let token = get_access_token()?;

    create_client()?
        .revoke_device(&token, &device_id)
        .await
        .map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Team Keys (E2EE)
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn initialize_team_keys(
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<InitializeKeysResult, String> {
    info!("[DeviceSync] Initializing team keys...");

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    let result = create_client()?
        .initialize_team_keys(&token, &device_id)
        .await
        .map_err(|e| e.to_string())?;

    info!(
        "[DeviceSync] Initialize team keys result: {:?}",
        match &result {
            InitializeKeysResult::Bootstrap { .. } => "BOOTSTRAP",
            InitializeKeysResult::PairingRequired { .. } => "PAIRING_REQUIRED",
            InitializeKeysResult::Ready { .. } => "READY",
        }
    );

    Ok(result)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn commit_initialize_team_keys(
    key_version: i32,
    device_key_envelope: String,
    signature: String,
    challenge_response: Option<String>,
    recovery_envelope: Option<String>,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<CommitInitializeKeysResponse, String> {
    info!("[DeviceSync] Committing team key initialization...");

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    let request = CommitInitializeKeysRequest {
        device_id: device_id.clone(),
        key_version,
        device_key_envelope,
        signature,
        challenge_response,
        recovery_envelope,
    };

    create_client()?
        .commit_initialize_team_keys(&token, request)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn rotate_team_keys(
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<RotateKeysResponse, String> {
    info!("[DeviceSync] Starting key rotation...");

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .rotate_team_keys(&token, &device_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn commit_rotate_team_keys(
    request: CommitRotateKeysRequest,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<CommitRotateKeysResponse, String> {
    info!("[DeviceSync] Committing key rotation...");

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .commit_rotate_team_keys(&token, &device_id, request)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reset_team_sync(
    reason: Option<String>,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<ResetTeamSyncResponse, String> {
    info!("[DeviceSync] Resetting team sync...");

    let token = get_access_token()?;

    create_client()?
        .reset_team_sync(&token, reason.as_deref())
        .await
        .map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Engine Status & Tauri Command Wrappers
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn sync_engine_status(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncEngineStatusResult, String> {
    let sync_repo = state.app_sync_repository();
    let status = sync_repo.get_engine_status().map_err(|e| e.to_string())?;
    let bootstrap_required = match get_device_id_from_store() {
        Some(device_id) => sync_repo
            .needs_bootstrap(&device_id)
            .map_err(|e| e.to_string())?,
        None => true,
    };
    let runtime = state.inner().device_sync_runtime();
    let background_running = runtime.background_task.lock().await.is_some();

    Ok(SyncEngineStatusResult {
        cursor: status.cursor,
        last_push_at: status.last_push_at,
        last_pull_at: status.last_pull_at,
        last_error: status.last_error,
        consecutive_failures: status.consecutive_failures,
        next_retry_at: status.next_retry_at,
        last_cycle_status: status.last_cycle_status,
        last_cycle_duration_ms: status.last_cycle_duration_ms,
        background_running,
        bootstrap_required,
    })
}

#[tauri::command]
pub async fn sync_trigger_cycle(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncCycleResult, String> {
    engine::run_sync_cycle(Arc::clone(state.inner())).await
}

#[tauri::command]
pub async fn device_sync_start_background_engine(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncBackgroundEngineResult, String> {
    ensure_background_engine_started(Arc::clone(state.inner())).await?;
    Ok(SyncBackgroundEngineResult {
        status: "started".to_string(),
        message: "Device sync background engine started".to_string(),
    })
}

#[tauri::command]
pub async fn device_sync_stop_background_engine(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncBackgroundEngineResult, String> {
    ensure_background_engine_stopped(Arc::clone(state.inner())).await?;
    Ok(SyncBackgroundEngineResult {
        status: "stopped".to_string(),
        message: "Device sync background engine stopped".to_string(),
    })
}

#[tauri::command]
pub async fn device_sync_generate_snapshot_now(
    handle: AppHandle,
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncSnapshotUploadResult, String> {
    snapshot::generate_snapshot_now_internal(Some(&handle), Arc::clone(state.inner())).await
}

#[tauri::command]
pub async fn device_sync_cancel_snapshot_upload(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncBackgroundEngineResult, String> {
    state
        .inner()
        .device_sync_runtime()
        .snapshot_upload_cancelled
        .store(true, Ordering::Relaxed);
    Ok(SyncBackgroundEngineResult {
        status: "cancel_requested".to_string(),
        message: "Snapshot upload cancellation requested".to_string(),
    })
}

#[tauri::command]
pub async fn device_sync_engine_status(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncEngineStatusResult, String> {
    sync_engine_status(state).await
}

#[tauri::command]
pub async fn device_sync_bootstrap_snapshot_if_needed(
    handle: AppHandle,
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncBootstrapResult, String> {
    let result = snapshot::sync_bootstrap_snapshot_if_needed(handle, state.inner()).await?;

    // Start the background sync engine after successful bootstrap.
    if result.status == "applied" {
        let engine_context = Arc::clone(state.inner());
        tauri::async_runtime::spawn(async move {
            if let Err(err) = ensure_background_engine_started(engine_context).await {
                log::warn!("[DeviceSync] Post-bootstrap engine start failed: {}", err);
            }
        });
    }

    Ok(result)
}

#[tauri::command]
pub async fn device_sync_trigger_cycle(
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SyncCycleResult, String> {
    sync_trigger_cycle(state).await
}

// ─────────────────────────────────────────────────────────────────────────────
// Pairing — Issuer Side
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command(rename_all = "camelCase")]
pub async fn create_pairing(
    code_hash: String,
    ephemeral_public_key: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<CreatePairingResponse, String> {
    debug!("[DeviceSync] Creating pairing session...");

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .create_pairing(
            &token,
            &device_id,
            CreatePairingRequest {
                code_hash,
                ephemeral_public_key,
            },
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_pairing(
    pairing_id: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<GetPairingResponse, String> {
    debug!("[DeviceSync] Getting pairing session: {}", pairing_id);

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .get_pairing(&token, &device_id, &pairing_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn approve_pairing(
    pairing_id: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<SuccessResponse, String> {
    debug!("[DeviceSync] Approving pairing session: {}", pairing_id);

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .approve_pairing(&token, &device_id, &pairing_id)
        .await
        .map_err(|e| e.to_string())
}

/// Complete a pairing session with key bundle.
/// On success, triggers a background snapshot generation so the new device can bootstrap.
#[tauri::command(rename_all = "camelCase")]
pub async fn complete_pairing(
    pairing_id: String,
    encrypted_key_bundle: String,
    sas_proof: serde_json::Value,
    signature: String,
    state: State<'_, Arc<ServiceContext>>,
) -> Result<SuccessResponse, String> {
    debug!("[DeviceSync] Completing pairing session: {}", pairing_id);

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    let result = create_client()?
        .complete_pairing(
            &token,
            &device_id,
            &pairing_id,
            CompletePairingRequest {
                encrypted_key_bundle,
                sas_proof,
                signature,
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    // Generate a snapshot in the background so the newly paired device can bootstrap.
    let snapshot_context = Arc::clone(state.inner());
    tauri::async_runtime::spawn(async move {
        if let Err(err) = snapshot::generate_snapshot_now_internal(None, snapshot_context).await {
            log::warn!(
                "[DeviceSync] Post-pairing snapshot generation failed: {}",
                err
            );
        }
    });

    // Ensure the background sync engine is running (may be a no-op if already started).
    let engine_context = Arc::clone(state.inner());
    tauri::async_runtime::spawn(async move {
        if let Err(err) = ensure_background_engine_started(engine_context).await {
            log::warn!("[DeviceSync] Post-pairing engine start failed: {}", err);
        }
    });

    Ok(result)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn cancel_pairing(
    pairing_id: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<SuccessResponse, String> {
    debug!("[DeviceSync] Canceling pairing session: {}", pairing_id);

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .cancel_pairing(&token, &device_id, &pairing_id)
        .await
        .map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Claimer-Side Pairing (New Device)
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command(rename_all = "camelCase")]
pub async fn claim_pairing(
    code: String,
    ephemeral_public_key: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<ClaimPairingResponse, String> {
    info!("[DeviceSync] Claiming pairing session...");

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .claim_pairing(
            &token,
            &device_id,
            ClaimPairingRequest {
                code,
                ephemeral_public_key,
            },
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_pairing_messages(
    pairing_id: String,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<PairingMessagesResponse, String> {
    debug!("[DeviceSync] Polling for pairing messages: {}", pairing_id);

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .get_pairing_messages(&token, &device_id, &pairing_id)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn confirm_pairing(
    pairing_id: String,
    proof: Option<String>,
    _state: State<'_, Arc<ServiceContext>>,
) -> Result<ConfirmPairingResponse, String> {
    info!("[DeviceSync] Confirming pairing: {}", pairing_id);

    let token = get_access_token()?;
    let device_id =
        get_device_id_from_store().ok_or_else(|| "No device ID configured".to_string())?;

    create_client()?
        .confirm_pairing(
            &token,
            &device_id,
            &pairing_id,
            ConfirmPairingRequest { proof },
        )
        .await
        .map_err(|e| e.to_string())
}
