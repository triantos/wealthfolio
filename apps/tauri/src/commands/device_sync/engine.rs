//! Sync cycle engine: push/pull/replay and background loop.

use chrono::Utc;
use log::{debug, info};
use std::sync::Arc;

use crate::context::ServiceContext;
use wealthfolio_core::sync::{
    backoff_seconds as core_sync_backoff_seconds, SyncEntity, SyncOperation,
    DEVICE_SYNC_FOREGROUND_INTERVAL_SECS, DEVICE_SYNC_INTERVAL_JITTER_SECS,
};
use wealthfolio_device_sync::{ApiRetryClass, SyncPushEventRequest, SyncPushRequest, SyncState};

use super::{
    create_client, decrypt_sync_payload, encrypt_sync_payload, get_access_token,
    get_sync_identity_from_store, millis_until_rfc3339, parse_event_operation,
    persist_device_config_from_identity, retry_class_code, sync_entity_name, sync_operation_name,
    SyncCycleResult,
};

use super::snapshot::maybe_generate_snapshot_for_policy;

/// A decoded remote event ready for LWW replay.
struct DecodedRemoteEvent {
    entity: SyncEntity,
    entity_id: String,
    op: SyncOperation,
    event_id: String,
    client_timestamp: String,
    seq: i64,
    payload: serde_json::Value,
}

/// Tracks mutable progress during a sync cycle and provides a helper to record failures.
struct CycleContext {
    sync_repo: Arc<wealthfolio_storage_sqlite::sync::AppSyncRepository>,
    started_at: std::time::Instant,
    lock_version: i64,
    local_cursor: i64,
    pushed_count: usize,
    pulled_count: usize,
}

impl CycleContext {
    /// Record a cycle failure: persist error + outcome, then return a result.
    async fn fail(
        &self,
        status: &str,
        message: String,
        retry_secs: Option<i64>,
    ) -> Result<SyncCycleResult, String> {
        self.sync_repo
            .mark_engine_error(message)
            .await
            .map_err(|e| e.to_string())?;
        let retry_at = retry_secs.map(|s| (Utc::now() + chrono::Duration::seconds(s)).to_rfc3339());
        let _ = self
            .sync_repo
            .mark_cycle_outcome(
                status.to_string(),
                self.started_at.elapsed().as_millis() as i64,
                retry_at,
            )
            .await;
        Ok(SyncCycleResult {
            status: status.to_string(),
            lock_version: self.lock_version,
            pushed_count: self.pushed_count,
            pulled_count: self.pulled_count,
            cursor: self.local_cursor,
            needs_bootstrap: status == "stale_cursor",
        })
    }
}

/// Runs one sync engine cycle (push + pull skeleton with backoff and stale cursor handling).
pub(super) async fn run_sync_cycle(
    context: Arc<ServiceContext>,
) -> Result<SyncCycleResult, String> {
    let runtime = context.device_sync_runtime();
    let _cycle_guard = runtime.cycle_mutex.lock().await;
    let cycle_started_at = std::time::Instant::now();
    let sync_repo = context.app_sync_repository();

    let mut ctx = CycleContext {
        sync_repo: sync_repo.clone(),
        started_at: cycle_started_at,
        lock_version: 0,
        local_cursor: sync_repo.get_cursor().unwrap_or(0),
        pushed_count: 0,
        pulled_count: 0,
    };

    let identity = match get_sync_identity_from_store() {
        Some(value) => value,
        None => {
            return ctx
                .fail(
                    "config_error",
                    "No sync identity configured. Please enable sync first.".to_string(),
                    None,
                )
                .await;
        }
    };
    let device_id = match identity.device_id.clone() {
        Some(value) => value,
        None => {
            return ctx
                .fail("config_error", "No device ID configured".to_string(), None)
                .await;
        }
    };

    let runtime_state = match context.device_enroll_service().get_sync_state().await {
        Ok(value) => value,
        Err(err) => {
            return ctx
                .fail(
                    "state_error",
                    format!("Failed to read sync state: {}", err.message),
                    Some(15),
                )
                .await;
        }
    };
    if runtime_state.state != SyncState::Ready {
        persist_device_config_from_identity(context.as_ref(), &identity, "untrusted").await;
        let _ = sync_repo
            .mark_cycle_outcome(
                "not_ready".to_string(),
                cycle_started_at.elapsed().as_millis() as i64,
                None,
            )
            .await;
        return Ok(SyncCycleResult {
            status: "not_ready".to_string(),
            lock_version: 0,
            pushed_count: 0,
            pulled_count: 0,
            cursor: ctx.local_cursor,
            needs_bootstrap: false,
        });
    }

    persist_device_config_from_identity(context.as_ref(), &identity, "trusted").await;
    let token = match get_access_token() {
        Ok(value) => value,
        Err(err) => {
            return ctx
                .fail("auth_error", format!("Auth error: {}", err), Some(30))
                .await;
        }
    };

    ctx.lock_version = sync_repo
        .acquire_cycle_lock()
        .await
        .map_err(|e| e.to_string())?;
    ctx.local_cursor = sync_repo.get_cursor().map_err(|e| e.to_string())?;
    let lock_version = ctx.lock_version;
    let mut local_cursor = ctx.local_cursor;
    let client = create_client()?;

    let cursor_response = match client.get_events_cursor(&token, &device_id).await {
        Ok(response) => response,
        Err(err) => {
            return ctx
                .fail(
                    "cursor_error",
                    format!("Cursor check failed: {}", err),
                    Some(10),
                )
                .await;
        }
    };
    if let Some(gc_watermark) = cursor_response.gc_watermark {
        if local_cursor < gc_watermark {
            return ctx
                .fail(
                    "stale_cursor",
                    format!(
                        "Local cursor {} is older than GC watermark {}. Snapshot bootstrap required.",
                        local_cursor, gc_watermark
                    ),
                    None,
                )
                .await;
        }
    }

    let pending = sync_repo
        .list_pending_outbox(500)
        .map_err(|e| e.to_string())?;
    let mut push_events = Vec::new();
    let mut push_event_ids = Vec::new();
    let mut max_retry_count = 0;

    for event in pending {
        max_retry_count = max_retry_count.max(event.retry_count);
        let event_type = format!(
            "{}.{}.v1",
            sync_entity_name(&event.entity),
            sync_operation_name(&event.op)
        );
        push_event_ids.push(event.event_id.clone());
        let payload_key_version = event.payload_key_version.max(1);
        let encrypted_payload =
            match encrypt_sync_payload(&event.payload, &identity, payload_key_version) {
                Ok(payload) => payload,
                Err(err) => {
                    return ctx
                        .fail(
                            "push_prepare_error",
                            format!("Push payload encryption failed: {}", err),
                            Some(15),
                        )
                        .await;
                }
            };
        push_events.push(SyncPushEventRequest {
            event_id: event.event_id,
            device_id: device_id.clone(),
            event_type,
            entity: event.entity,
            entity_id: event.entity_id,
            client_timestamp: event.client_timestamp,
            payload: encrypted_payload,
            payload_key_version,
        });
    }

    let mut pushed_count = 0usize;
    if !push_events.is_empty() {
        match client
            .push_events(
                &token,
                &device_id,
                SyncPushRequest {
                    events: push_events,
                },
            )
            .await
        {
            Ok(push_response) => {
                let mut sent_ids: Vec<String> = push_response
                    .accepted
                    .into_iter()
                    .map(|item| item.event_id)
                    .collect();
                sent_ids.extend(
                    push_response
                        .duplicate
                        .into_iter()
                        .map(|item| item.event_id),
                );
                pushed_count = sent_ids.len();
                sync_repo
                    .mark_outbox_sent(sent_ids)
                    .await
                    .map_err(|e| e.to_string())?;
                sync_repo
                    .mark_push_completed()
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Err(err) => {
                let err_str = err.to_string();

                // Key version mismatch — re-pairing required
                if err_str.contains("KEY_VERSION_MISMATCH") {
                    sync_repo
                        .mark_outbox_dead(
                            push_event_ids,
                            Some(err_str.clone()),
                            Some("key_version_mismatch".to_string()),
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    return ctx
                        .fail(
                            "key_version_mismatch",
                            "Key version mismatch — re-pairing required".to_string(),
                            None,
                        )
                        .await;
                }

                let backoff = core_sync_backoff_seconds(max_retry_count);
                let retry_class = err.retry_class();
                match retry_class {
                    ApiRetryClass::ReauthRequired => {
                        sync_repo
                            .schedule_outbox_retry(
                                push_event_ids,
                                30, // longer delay for auth refresh
                                Some(err_str.clone()),
                                Some(retry_class_code(retry_class).to_string()),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        log::warn!("[DeviceSync] Auth error during push — token may need refresh");
                        return ctx
                            .fail(
                                "auth_error",
                                "Authentication required".to_string(),
                                Some(30),
                            )
                            .await;
                    }
                    ApiRetryClass::Retryable => {
                        sync_repo
                            .schedule_outbox_retry(
                                push_event_ids,
                                backoff,
                                Some(err_str),
                                Some(retry_class_code(retry_class).to_string()),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    ApiRetryClass::Permanent => {
                        sync_repo
                            .mark_outbox_dead(
                                push_event_ids,
                                Some(err_str),
                                Some(retry_class_code(retry_class).to_string()),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
                return ctx
                    .fail("push_error", format!("Push failed: {}", err), Some(backoff))
                    .await;
            }
        }
    }

    ctx.pushed_count = pushed_count;

    // Verify the cycle lock is still held before starting pull.
    if !sync_repo
        .verify_cycle_lock(lock_version)
        .map_err(|e| e.to_string())?
    {
        let _ = sync_repo
            .mark_cycle_outcome(
                "preempted".to_string(),
                cycle_started_at.elapsed().as_millis() as i64,
                None,
            )
            .await;
        return Ok(SyncCycleResult {
            status: "preempted".to_string(),
            lock_version,
            pushed_count,
            pulled_count: 0,
            cursor: local_cursor,
            needs_bootstrap: false,
        });
    }

    let mut pulled_count = 0usize;
    if cursor_response.cursor > local_cursor {
        loop {
            ctx.local_cursor = local_cursor;
            ctx.pulled_count = pulled_count;
            let pull_response = match client
                .pull_events(&token, &device_id, Some(local_cursor), Some(500))
                .await
            {
                Ok(value) => value,
                Err(err) => {
                    if err.retry_class() == ApiRetryClass::ReauthRequired {
                        log::warn!("[DeviceSync] Auth error during pull — token may need refresh");
                        return ctx
                            .fail(
                                "auth_error",
                                "Authentication required".to_string(),
                                Some(30),
                            )
                            .await;
                    }
                    return ctx
                        .fail("pull_error", format!("Pull failed: {}", err), Some(10))
                        .await;
                }
            };

            if let Some(gc_watermark) = pull_response.gc_watermark {
                if local_cursor < gc_watermark {
                    return ctx
                        .fail(
                            "stale_cursor",
                            format!(
                                "Cursor {} is older than pull GC watermark {}",
                                local_cursor, gc_watermark
                            ),
                            None,
                        )
                        .await;
                }
            }

            let mut decoded_events: Vec<DecodedRemoteEvent> =
                Vec::with_capacity(pull_response.events.len());
            for remote_event in pull_response.events {
                // Skip events originating from this device — already applied locally.
                if remote_event.device_id == device_id {
                    continue;
                }
                let local_entity = remote_event.entity;
                if local_entity == SyncEntity::Snapshot {
                    debug!(
                        "[DeviceSync] Skipping snapshot control event during replay: event_id={} event_type={} seq={}",
                        remote_event.event_id, remote_event.event_type, remote_event.seq
                    );
                    continue;
                }
                let local_op = match parse_event_operation(&remote_event.event_type) {
                    Some(op) => op,
                    None => {
                        log::warn!(
                            "[DeviceSync] Replay blocked: unsupported event type '{}' for event {}",
                            remote_event.event_type,
                            remote_event.event_id
                        );
                        return ctx
                            .fail(
                                "replay_blocked",
                                format!(
                                    "Replay blocked: unsupported event type '{}' for event {}",
                                    remote_event.event_type, remote_event.event_id
                                ),
                                Some(6 * 60 * 60),
                            )
                            .await;
                    }
                };
                let decrypted_payload = match decrypt_sync_payload(
                    &remote_event.payload,
                    &identity,
                    remote_event.payload_key_version,
                ) {
                    Ok(payload) => payload,
                    Err(err) => {
                        return ctx
                            .fail(
                                "replay_error",
                                format!(
                                    "Replay decrypt failed for event {}: {}",
                                    remote_event.event_id, err
                                ),
                                Some(10),
                            )
                            .await;
                    }
                };
                let payload_json: serde_json::Value = match serde_json::from_str(&decrypted_payload)
                {
                    Ok(payload) => payload,
                    Err(err) => {
                        return ctx
                            .fail(
                                "replay_error",
                                format!(
                                    "Replay payload decode failed for event {}: {}",
                                    remote_event.event_id, err
                                ),
                                Some(10),
                            )
                            .await;
                    }
                };

                decoded_events.push(DecodedRemoteEvent {
                    entity: local_entity,
                    entity_id: remote_event.entity_id,
                    op: local_op,
                    event_id: remote_event.event_id,
                    client_timestamp: remote_event.client_timestamp,
                    seq: remote_event.seq,
                    payload: payload_json,
                });
            }

            let batch_tuples: Vec<_> = decoded_events
                .into_iter()
                .map(|e| {
                    (
                        e.entity,
                        e.entity_id,
                        e.op,
                        e.event_id,
                        e.client_timestamp,
                        e.seq,
                        e.payload,
                    )
                })
                .collect();
            let applied_count = match sync_repo.apply_remote_events_lww_batch(batch_tuples).await {
                Ok(applied) => applied,
                Err(err) => {
                    return ctx
                        .fail(
                            "replay_error",
                            format!("Replay apply failed: {}", err),
                            Some(10),
                        )
                        .await;
                }
            };
            pulled_count += applied_count;

            local_cursor = pull_response.next_cursor;
            sync_repo
                .set_cursor(local_cursor)
                .await
                .map_err(|e| e.to_string())?;

            if !pull_response.has_more {
                break;
            }
        }
        sync_repo
            .mark_pull_completed()
            .await
            .map_err(|e| e.to_string())?;
    }

    if local_cursor > 20_000 {
        let prune_seq = local_cursor - 10_000;
        let _ = sync_repo.prune_applied_events_up_to_seq(prune_seq).await;
    }

    sync_repo
        .mark_cycle_outcome(
            "ok".to_string(),
            cycle_started_at.elapsed().as_millis() as i64,
            None,
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(SyncCycleResult {
        status: "ok".to_string(),
        lock_version,
        pushed_count,
        pulled_count,
        cursor: local_cursor,
        needs_bootstrap: false,
    })
}

pub async fn ensure_background_engine_started(context: Arc<ServiceContext>) -> Result<(), String> {
    // Don't spawn the loop if sync isn't configured yet.
    if get_sync_identity_from_store().is_none() {
        return Ok(());
    }

    let runtime = context.device_sync_runtime();
    let mut guard = runtime.background_task.lock().await;
    if let Some(handle) = guard.as_ref() {
        if !handle.is_finished() {
            return Ok(());
        }
        // Task finished (loop broke on revoked/not_ready) — clear and respawn.
        guard.take();
    }

    let handle = tokio::spawn(async move {
        let mut consecutive_not_ready: u32 = 0;
        loop {
            let cycle_result = run_sync_cycle(Arc::clone(&context)).await;
            if let Err(err) = &cycle_result {
                log::warn!("[DeviceSync] Background cycle failed: {}", err);
                consecutive_not_ready = 0;
            }
            if let Ok(result) = &cycle_result {
                debug!(
                    "[DeviceSync] Cycle complete status={} needs_bootstrap={} cursor={} pushed={} pulled={}",
                    result.status,
                    result.needs_bootstrap,
                    result.cursor,
                    result.pushed_count,
                    result.pulled_count
                );
                if result.status == "not_ready" || result.status == "config_error" {
                    consecutive_not_ready += 1;
                    // Check if device was revoked (has device_id but no root_key)
                    if let Some(identity) = get_sync_identity_from_store() {
                        if identity.root_key.is_none() && identity.device_id.is_some() {
                            info!(
                                "[DeviceSync] Device appears revoked. Stopping background engine."
                            );
                            break;
                        }
                    }
                    if consecutive_not_ready >= 5 {
                        info!("[DeviceSync] {} consecutive not_ready/config_error cycles. Stopping background engine.", consecutive_not_ready);
                        break;
                    }
                } else {
                    consecutive_not_ready = 0;
                }
                if result.status == "ok" {
                    maybe_generate_snapshot_for_policy(Arc::clone(&context)).await;
                } else {
                    debug!(
                        "[DeviceSync] Snapshot policy skipped because cycle status is '{}' (requires 'ok')",
                        result.status
                    );
                }
            }

            let jitter_bound = DEVICE_SYNC_INTERVAL_JITTER_SECS.saturating_mul(1000);
            let jitter_ms = if jitter_bound > 0 {
                Utc::now().timestamp_millis().unsigned_abs() % jitter_bound
            } else {
                0
            };
            let mut delay_ms =
                DEVICE_SYNC_FOREGROUND_INTERVAL_SECS.saturating_mul(1000) + jitter_ms;

            if let Ok(engine_status) = context.app_sync_repository().get_engine_status() {
                if let Some(next_retry_at) = engine_status.next_retry_at.as_deref() {
                    if let Some(wait_ms) = millis_until_rfc3339(next_retry_at) {
                        delay_ms = wait_ms.saturating_add(jitter_ms).max(1_000);
                    }
                }
            }

            if let Ok(pending) = context.app_sync_repository().list_pending_outbox(1) {
                if !pending.is_empty() {
                    delay_ms = delay_ms.min(2_000 + (jitter_ms % 500));
                }
            }

            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
    });
    *guard = Some(handle);
    Ok(())
}

pub async fn ensure_background_engine_stopped(context: Arc<ServiceContext>) -> Result<(), String> {
    let runtime = context.device_sync_runtime();
    let mut guard = runtime.background_task.lock().await;
    if let Some(handle) = guard.take() {
        handle.abort();
    }
    Ok(())
}
