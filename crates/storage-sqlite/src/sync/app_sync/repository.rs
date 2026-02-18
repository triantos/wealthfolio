//! Repository for app-side device sync tables.

use chrono::{Duration, Utc};
use diesel::prelude::*;
use diesel::r2d2::{self, Pool};
use diesel::sqlite::SqliteConnection;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

use wealthfolio_core::errors::{DatabaseError, Error, Result};
use wealthfolio_core::sync::{
    should_apply_lww, SyncEngineStatus, SyncEntity, SyncEntityMetadata, SyncOperation,
    SyncOutboxEvent, SyncOutboxStatus, APP_SYNC_TABLES,
};

use crate::db::{get_connection, WriteHandle};
use crate::errors::StorageError;
use crate::schema::{
    sync_applied_events, sync_cursor, sync_device_config, sync_engine_state, sync_entity_metadata,
    sync_outbox, sync_table_state,
};

use super::model::{
    SyncAppliedEventDB, SyncCursorDB, SyncDeviceConfigDB, SyncEngineStateDB, SyncEntityMetadataDB,
    SyncOutboxEventDB, SyncTableStateDB,
};

fn enum_to_db<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?.trim_matches('"').to_string())
}

fn enum_from_db<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    Ok(serde_json::from_str(&format!("\"{}\"", value))?)
}

fn validate_sync_table(table: &str) -> Result<()> {
    if APP_SYNC_TABLES.contains(&table) {
        return Ok(());
    }
    Err(Error::Database(DatabaseError::Internal(format!(
        "Unsupported sync table '{}'",
        table
    ))))
}

fn payload_columns_cache() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn escape_sqlite_str(value: &str) -> String {
    value.replace('\'', "''")
}

fn quote_identifier(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

#[derive(diesel::QueryableByName)]
struct PragmaTableInfoRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

#[derive(diesel::QueryableByName)]
struct PragmaTableXInfoRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    hidden: i32,
}

fn load_table_columns(
    conn: &mut SqliteConnection,
    db_name: &str,
    table_name: &str,
) -> Result<Vec<String>> {
    let pragma_xinfo_sql = format!(
        "PRAGMA {}.table_xinfo('{}')",
        db_name,
        escape_sqlite_str(table_name)
    );
    let xinfo_result = diesel::sql_query(pragma_xinfo_sql)
        .load::<PragmaTableXInfoRow>(conn)
        .map_err(StorageError::from);
    if let Ok(rows) = xinfo_result {
        let columns = rows
            .into_iter()
            .filter(|row| row.hidden == 0)
            .map(|row| row.name)
            .collect::<Vec<_>>();
        return Ok(columns);
    }

    let pragma_info_sql = format!(
        "PRAGMA {}.table_info('{}')",
        db_name,
        escape_sqlite_str(table_name)
    );
    let columns = diesel::sql_query(pragma_info_sql)
        .load::<PragmaTableInfoRow>(conn)
        .map_err(StorageError::from)?
        .into_iter()
        .map(|row| row.name)
        .collect::<Vec<_>>();
    Ok(columns)
}

fn payload_value_matches_entity_id(value: &serde_json::Value, entity_id: &str) -> bool {
    match value {
        serde_json::Value::String(v) => v == entity_id,
        serde_json::Value::Number(v) => v.to_string() == entity_id,
        serde_json::Value::Bool(v) => v.to_string() == entity_id,
        _ => false,
    }
}

fn validate_payload_columns(
    conn: &mut SqliteConnection,
    table_name: &str,
    fields: &[(String, serde_json::Value)],
) -> Result<()> {
    let known_columns = {
        let cache = payload_columns_cache().lock().map_err(|_| {
            Error::Database(DatabaseError::Internal(
                "Sync payload column cache is poisoned".to_string(),
            ))
        })?;
        cache.get(table_name).cloned()
    };
    let known_columns = if let Some(columns) = known_columns {
        columns
    } else {
        let columns = load_table_columns(conn, "main", table_name)?
            .into_iter()
            .collect::<HashSet<_>>();
        let mut cache = payload_columns_cache().lock().map_err(|_| {
            Error::Database(DatabaseError::Internal(
                "Sync payload column cache is poisoned".to_string(),
            ))
        })?;
        cache.insert(table_name.to_string(), columns.clone());
        columns
    };

    for (column, _) in fields {
        if !known_columns.contains(column) {
            return Err(Error::Database(DatabaseError::Internal(format!(
                "Sync payload column '{}' is not valid for table '{}'",
                column, table_name
            ))));
        }
    }

    Ok(())
}

fn entity_storage_mapping(entity: &SyncEntity) -> Option<(&'static str, &'static str)> {
    match entity {
        SyncEntity::Account => Some(("accounts", "id")),
        SyncEntity::Asset => Some(("assets", "id")),
        SyncEntity::AssetTaxonomyAssignment => Some(("asset_taxonomy_assignments", "id")),
        SyncEntity::Activity => Some(("activities", "id")),
        SyncEntity::ActivityImportProfile => Some(("activity_import_profiles", "account_id")),
        SyncEntity::Goal => Some(("goals", "id")),
        SyncEntity::GoalsAllocation => Some(("goals_allocation", "id")),
        SyncEntity::AiThread => Some(("ai_threads", "id")),
        SyncEntity::AiMessage => Some(("ai_messages", "id")),
        SyncEntity::AiThreadTag => Some(("ai_thread_tags", "id")),
        SyncEntity::ContributionLimit => Some(("contribution_limits", "id")),
        SyncEntity::Platform => Some(("platforms", "id")),
        SyncEntity::Snapshot => Some(("holdings_snapshots", "id")),
    }
}

fn json_value_to_sql_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(v) => {
            if *v {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::String(v) => format!("'{}'", escape_sqlite_str(v)),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            format!(
                "'{}'",
                escape_sqlite_str(&serde_json::to_string(value).unwrap_or_default())
            )
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutboxWriteRequest {
    pub event_id: Option<String>,
    pub entity: SyncEntity,
    pub entity_id: String,
    pub op: SyncOperation,
    pub client_timestamp: String,
    pub payload: serde_json::Value,
    pub payload_key_version: i32,
}

impl OutboxWriteRequest {
    pub fn new(
        entity: SyncEntity,
        entity_id: impl Into<String>,
        op: SyncOperation,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event_id: None,
            entity,
            entity_id: entity_id.into(),
            op,
            client_timestamp: Utc::now().to_rfc3339(),
            payload,
            // 0 means "resolve from local sync_device_config"; explicit callers can override.
            payload_key_version: 0,
        }
    }
}

fn resolve_payload_key_version(conn: &mut SqliteConnection, requested_version: i32) -> Result<i32> {
    if requested_version > 0 {
        return Ok(requested_version);
    }

    let maybe_row = sync_device_config::table
        .filter(sync_device_config::trust_state.eq("trusted"))
        .filter(sync_device_config::key_version.is_not_null())
        .order(sync_device_config::key_version.desc())
        .first::<SyncDeviceConfigDB>(conn)
        .optional()
        .map_err(StorageError::from)?;

    Ok(maybe_row
        .and_then(|row| row.key_version)
        .unwrap_or(1)
        .max(1))
}

fn resolve_local_device_id(conn: &mut SqliteConnection) -> Option<String> {
    sync_device_config::table
        .filter(sync_device_config::trust_state.eq("trusted"))
        .select(sync_device_config::device_id)
        .first::<String>(conn)
        .optional()
        .unwrap_or(None)
}

fn is_connect_configured() -> bool {
    std::env::var("CONNECT_API_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
}

pub fn write_outbox_event(
    conn: &mut SqliteConnection,
    request: OutboxWriteRequest,
) -> Result<String> {
    if !is_connect_configured() {
        return Ok(String::new());
    }

    let event_id = request
        .event_id
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let payload = serde_json::to_string(&request.payload)?;
    let now = Utc::now().to_rfc3339();

    let payload_key_version = resolve_payload_key_version(conn, request.payload_key_version)?;
    let device_id = resolve_local_device_id(conn);
    let row = SyncOutboxEventDB {
        event_id: event_id.clone(),
        entity: enum_to_db(&request.entity)?,
        entity_id: request.entity_id,
        op: enum_to_db(&request.op)?,
        client_timestamp: request.client_timestamp,
        payload,
        payload_key_version,
        sent: 0,
        status: enum_to_db(&SyncOutboxStatus::Pending)?,
        retry_count: 0,
        next_retry_at: None,
        last_error: None,
        last_error_code: None,
        device_id,
        created_at: now,
    };

    diesel::insert_into(sync_outbox::table)
        .values(&row)
        .execute(conn)
        .map_err(StorageError::from)?;

    Ok(event_id)
}

fn to_outbox_event(row: SyncOutboxEventDB) -> Result<SyncOutboxEvent> {
    Ok(SyncOutboxEvent {
        event_id: row.event_id,
        entity: enum_from_db(&row.entity)?,
        entity_id: row.entity_id,
        op: enum_from_db(&row.op)?,
        client_timestamp: row.client_timestamp,
        payload: row.payload,
        payload_key_version: row.payload_key_version,
        sent: row.sent != 0,
        status: enum_from_db(&row.status)?,
        retry_count: row.retry_count,
        next_retry_at: row.next_retry_at,
        last_error: row.last_error,
        last_error_code: row.last_error_code,
        created_at: row.created_at,
    })
}

fn to_entity_metadata(row: SyncEntityMetadataDB) -> Result<SyncEntityMetadata> {
    Ok(SyncEntityMetadata {
        entity: enum_from_db(&row.entity)?,
        entity_id: row.entity_id,
        last_event_id: row.last_event_id,
        last_client_timestamp: row.last_client_timestamp,
        last_seq: row.last_seq,
    })
}

#[allow(clippy::too_many_arguments)]
fn apply_remote_event_lww_tx(
    conn: &mut SqliteConnection,
    entity: SyncEntity,
    entity_id_value: String,
    op: SyncOperation,
    event_id_value: String,
    client_timestamp_value: String,
    seq_value: i64,
    payload_json: serde_json::Value,
) -> Result<bool> {
    let already_applied = sync_applied_events::table
        .find(&event_id_value)
        .first::<SyncAppliedEventDB>(conn)
        .optional()
        .map_err(StorageError::from)?
        .is_some();
    if already_applied {
        return Ok(false);
    }

    let entity_db = enum_to_db(&entity)?;
    let metadata_row = sync_entity_metadata::table
        .filter(sync_entity_metadata::entity.eq(&entity_db))
        .filter(sync_entity_metadata::entity_id.eq(&entity_id_value))
        .first::<SyncEntityMetadataDB>(conn)
        .optional()
        .map_err(StorageError::from)?;

    let should_apply = match metadata_row.as_ref() {
        Some(meta) => should_apply_lww(
            &meta.last_client_timestamp,
            &meta.last_event_id,
            &client_timestamp_value,
            &event_id_value,
        ),
        None => true,
    };

    if should_apply {
        if let Some((table_name, pk_name)) = entity_storage_mapping(&entity) {
            match op {
                SyncOperation::Delete => {
                    let sql = format!(
                        "DELETE FROM {} WHERE {} = '{}'",
                        quote_identifier(table_name),
                        quote_identifier(pk_name),
                        escape_sqlite_str(&entity_id_value)
                    );
                    diesel::sql_query(sql)
                        .execute(conn)
                        .map_err(StorageError::from)?;
                }
                SyncOperation::Create | SyncOperation::Update | SyncOperation::Request => {
                    let payload_obj = payload_json.as_object().ok_or_else(|| {
                        Error::Database(DatabaseError::Internal(
                            "Sync payload must be a JSON object".to_string(),
                        ))
                    })?;
                    if let Some(payload_pk) = payload_obj.get(pk_name) {
                        if !payload_value_matches_entity_id(payload_pk, &entity_id_value) {
                            return Err(Error::Database(DatabaseError::Internal(format!(
                                "Sync payload PK '{}' does not match entity_id '{}'",
                                pk_name, entity_id_value
                            ))));
                        }
                    }

                    let mut fields: Vec<(String, serde_json::Value)> = payload_obj
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if !fields.iter().any(|(k, _)| k == pk_name) {
                        fields.push((
                            pk_name.to_string(),
                            serde_json::Value::String(entity_id_value.clone()),
                        ));
                    }
                    validate_payload_columns(conn, table_name, &fields)?;

                    let columns = fields
                        .iter()
                        .map(|(k, _)| quote_identifier(k))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let values = fields
                        .iter()
                        .map(|(_, v)| json_value_to_sql_literal(v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let upserts = fields
                        .iter()
                        .map(|(k, _)| {
                            let quoted = quote_identifier(k);
                            format!("{quoted}=excluded.{quoted}")
                        })
                        .collect::<Vec<_>>()
                        .join(", ");

                    let sql = format!(
                        "INSERT INTO {} ({columns}) VALUES ({values}) \
                         ON CONFLICT({}) DO UPDATE SET {upserts}",
                        quote_identifier(table_name),
                        quote_identifier(pk_name)
                    );
                    diesel::sql_query(sql)
                        .execute(conn)
                        .map_err(StorageError::from)?;
                }
            }

            let now = Utc::now().to_rfc3339();
            diesel::insert_into(sync_table_state::table)
                .values(SyncTableStateDB {
                    table_name: table_name.to_string(),
                    enabled: 1,
                    last_snapshot_restore_at: None,
                    last_incremental_apply_at: Some(now.clone()),
                })
                .on_conflict(sync_table_state::table_name)
                .do_update()
                .set((
                    sync_table_state::enabled.eq(1),
                    sync_table_state::last_incremental_apply_at.eq(Some(now)),
                ))
                .execute(conn)
                .map_err(StorageError::from)?;
        }

        diesel::insert_into(sync_entity_metadata::table)
            .values(SyncEntityMetadataDB {
                entity: entity_db.clone(),
                entity_id: entity_id_value.clone(),
                last_event_id: event_id_value.clone(),
                last_client_timestamp: client_timestamp_value.clone(),
                last_seq: seq_value,
            })
            .on_conflict((
                sync_entity_metadata::entity,
                sync_entity_metadata::entity_id,
            ))
            .do_update()
            .set((
                sync_entity_metadata::last_event_id.eq(event_id_value.clone()),
                sync_entity_metadata::last_client_timestamp.eq(client_timestamp_value.clone()),
                sync_entity_metadata::last_seq.eq(seq_value),
            ))
            .execute(conn)
            .map_err(StorageError::from)?;
    }

    diesel::insert_into(sync_applied_events::table)
        .values(SyncAppliedEventDB {
            event_id: event_id_value,
            seq: seq_value,
            entity: entity_db,
            entity_id: entity_id_value,
            applied_at: Utc::now().to_rfc3339(),
        })
        .on_conflict(sync_applied_events::event_id)
        .do_nothing()
        .execute(conn)
        .map_err(StorageError::from)?;

    Ok(should_apply)
}

pub struct AppSyncRepository {
    pool: Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
    writer: WriteHandle,
}

impl AppSyncRepository {
    pub fn new(
        pool: Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
        writer: WriteHandle,
    ) -> Self {
        Self { pool, writer }
    }

    pub fn get_cursor(&self) -> Result<i64> {
        let mut conn = get_connection(&self.pool)?;
        let row = sync_cursor::table
            .find(1)
            .first::<SyncCursorDB>(&mut conn)
            .optional()
            .map_err(StorageError::from)?;
        Ok(row.map(|r| r.cursor).unwrap_or(0))
    }

    pub async fn set_cursor(&self, cursor_value: i64) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let now = Utc::now().to_rfc3339();
                let row = SyncCursorDB {
                    id: 1,
                    cursor: cursor_value,
                    updated_at: now.clone(),
                };

                diesel::insert_into(sync_cursor::table)
                    .values(&row)
                    .on_conflict(sync_cursor::id)
                    .do_update()
                    .set((
                        sync_cursor::cursor.eq(cursor_value),
                        sync_cursor::updated_at.eq(now),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;

                Ok(())
            })
            .await
    }

    pub fn get_engine_status(&self) -> Result<SyncEngineStatus> {
        let mut conn = get_connection(&self.pool)?;
        let cursor = self.get_cursor()?;
        let engine = sync_engine_state::table
            .find(1)
            .first::<SyncEngineStateDB>(&mut conn)
            .optional()
            .map_err(StorageError::from)?;

        Ok(SyncEngineStatus {
            cursor,
            last_push_at: engine.as_ref().and_then(|s| s.last_push_at.clone()),
            last_pull_at: engine.as_ref().and_then(|s| s.last_pull_at.clone()),
            last_error: engine.as_ref().and_then(|s| s.last_error.clone()),
            consecutive_failures: engine.as_ref().map(|s| s.consecutive_failures).unwrap_or(0),
            next_retry_at: engine.as_ref().and_then(|s| s.next_retry_at.clone()),
            last_cycle_status: engine.as_ref().and_then(|s| s.last_cycle_status.clone()),
            last_cycle_duration_ms: engine.and_then(|s| s.last_cycle_duration_ms),
        })
    }

    pub fn needs_bootstrap(&self, device_id: &str) -> Result<bool> {
        let mut conn = get_connection(&self.pool)?;
        let config = sync_device_config::table
            .find(device_id)
            .first::<SyncDeviceConfigDB>(&mut conn)
            .optional()
            .map_err(StorageError::from)?;

        Ok(match config {
            None => true,
            Some(row) => row.last_bootstrap_at.is_none(),
        })
    }

    pub async fn upsert_device_config(
        &self,
        device_id_value: String,
        key_version_value: Option<i32>,
        trust_state_value: String,
    ) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let row = SyncDeviceConfigDB {
                    device_id: device_id_value.clone(),
                    key_version: key_version_value,
                    trust_state: trust_state_value.clone(),
                    last_bootstrap_at: None,
                };

                diesel::insert_into(sync_device_config::table)
                    .values(&row)
                    .on_conflict(sync_device_config::device_id)
                    .do_update()
                    .set((
                        sync_device_config::key_version.eq(key_version_value),
                        sync_device_config::trust_state.eq(trust_state_value),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;

                Ok(())
            })
            .await
    }

    pub async fn mark_bootstrap_complete(
        &self,
        device_id_value: String,
        key_version_value: Option<i32>,
    ) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let now = Utc::now().to_rfc3339();

                diesel::insert_into(sync_device_config::table)
                    .values(SyncDeviceConfigDB {
                        device_id: device_id_value.clone(),
                        key_version: key_version_value,
                        trust_state: "trusted".to_string(),
                        last_bootstrap_at: Some(now.clone()),
                    })
                    .on_conflict(sync_device_config::device_id)
                    .do_update()
                    .set((
                        sync_device_config::key_version.eq(key_version_value),
                        sync_device_config::trust_state.eq("trusted"),
                        sync_device_config::last_bootstrap_at.eq(Some(now.clone())),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;

                Ok(())
            })
            .await
    }

    pub fn list_pending_outbox(&self, limit_value: i64) -> Result<Vec<SyncOutboxEvent>> {
        let mut conn = get_connection(&self.pool)?;
        let now = Utc::now().to_rfc3339();

        let rows = sync_outbox::table
            .filter(
                sync_outbox::status
                    .eq(enum_to_db(&SyncOutboxStatus::Pending)?)
                    .and(sync_outbox::sent.eq(0)),
            )
            .filter(
                sync_outbox::next_retry_at
                    .is_null()
                    .or(sync_outbox::next_retry_at.le(now)),
            )
            .order(sync_outbox::created_at.asc())
            .limit(limit_value)
            .load::<SyncOutboxEventDB>(&mut conn)
            .map_err(StorageError::from)?;

        rows.into_iter().map(to_outbox_event).collect()
    }

    pub async fn mark_outbox_sent(&self, event_ids: Vec<String>) -> Result<()> {
        if event_ids.is_empty() {
            return Ok(());
        }

        self.writer
            .exec(move |conn| {
                diesel::update(sync_outbox::table.filter(sync_outbox::event_id.eq_any(event_ids)))
                    .set((
                        sync_outbox::sent.eq(1),
                        sync_outbox::status.eq(enum_to_db(&SyncOutboxStatus::Sent)?),
                        sync_outbox::next_retry_at.eq::<Option<String>>(None),
                        sync_outbox::last_error.eq::<Option<String>>(None),
                        sync_outbox::last_error_code.eq::<Option<String>>(None),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn schedule_outbox_retry(
        &self,
        event_ids: Vec<String>,
        backoff_seconds: i64,
        last_error: Option<String>,
        last_error_code: Option<String>,
    ) -> Result<()> {
        if event_ids.is_empty() {
            return Ok(());
        }

        self.writer
            .exec(move |conn| {
                let retry_at = (Utc::now() + Duration::seconds(backoff_seconds)).to_rfc3339();
                let rows = sync_outbox::table
                    .filter(sync_outbox::event_id.eq_any(&event_ids))
                    .load::<SyncOutboxEventDB>(conn)
                    .map_err(StorageError::from)?;

                for row in rows {
                    diesel::update(sync_outbox::table.find(row.event_id))
                        .set((
                            sync_outbox::retry_count.eq(row.retry_count + 1),
                            sync_outbox::next_retry_at.eq(Some(retry_at.clone())),
                            sync_outbox::status.eq(enum_to_db(&SyncOutboxStatus::Pending)?),
                            sync_outbox::last_error.eq(last_error.clone()),
                            sync_outbox::last_error_code.eq(last_error_code.clone()),
                        ))
                        .execute(conn)
                        .map_err(StorageError::from)?;
                }
                Ok(())
            })
            .await
    }

    pub async fn upsert_entity_metadata(&self, metadata: SyncEntityMetadata) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let row = SyncEntityMetadataDB {
                    entity: enum_to_db(&metadata.entity)?,
                    entity_id: metadata.entity_id.clone(),
                    last_event_id: metadata.last_event_id.clone(),
                    last_client_timestamp: metadata.last_client_timestamp.clone(),
                    last_seq: metadata.last_seq,
                };

                diesel::insert_into(sync_entity_metadata::table)
                    .values(&row)
                    .on_conflict((
                        sync_entity_metadata::entity,
                        sync_entity_metadata::entity_id,
                    ))
                    .do_update()
                    .set((
                        sync_entity_metadata::last_event_id.eq(row.last_event_id.clone()),
                        sync_entity_metadata::last_client_timestamp
                            .eq(row.last_client_timestamp.clone()),
                        sync_entity_metadata::last_seq.eq(row.last_seq),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub fn get_entity_metadata(
        &self,
        entity: SyncEntity,
        entity_id_value: &str,
    ) -> Result<Option<SyncEntityMetadata>> {
        let mut conn = get_connection(&self.pool)?;
        let entity_value = enum_to_db(&entity)?;
        let row = sync_entity_metadata::table
            .find((entity_value, entity_id_value))
            .first::<SyncEntityMetadataDB>(&mut conn)
            .optional()
            .map_err(StorageError::from)?;

        row.map(to_entity_metadata).transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn apply_remote_event_lww(
        &self,
        entity: SyncEntity,
        entity_id_value: String,
        op: SyncOperation,
        event_id_value: String,
        client_timestamp_value: String,
        seq_value: i64,
        payload_json: serde_json::Value,
    ) -> Result<bool> {
        self.writer
            .exec(move |conn| {
                apply_remote_event_lww_tx(
                    conn,
                    entity,
                    entity_id_value,
                    op,
                    event_id_value,
                    client_timestamp_value,
                    seq_value,
                    payload_json,
                )
            })
            .await
    }

    pub async fn apply_remote_events_lww_batch(
        &self,
        events: Vec<(
            SyncEntity,
            String,
            SyncOperation,
            String,
            String,
            i64,
            serde_json::Value,
        )>,
    ) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }

        self.writer
            .exec(move |conn| {
                // Defer FK checks during batch replay — events may arrive
                // out of dependency order (e.g. activity before its account).
                // Note: writer actor wraps jobs in a transaction, and SQLite
                // ignores PRAGMA foreign_keys toggles inside active transactions.
                // defer_foreign_keys applies to the current transaction and lets
                // constraints validate at commit time.
                diesel::sql_query("PRAGMA defer_foreign_keys = ON")
                    .execute(conn)
                    .map_err(StorageError::from)?;

                let result = (|| -> Result<usize> {
                    let mut applied = 0usize;
                    for (entity, entity_id, op, event_id, client_timestamp, seq, payload) in events
                    {
                        if apply_remote_event_lww_tx(
                            conn,
                            entity,
                            entity_id.clone(),
                            op,
                            event_id.clone(),
                            client_timestamp.clone(),
                            seq,
                            payload,
                        )
                        .map_err(|err| {
                            Error::Database(DatabaseError::Internal(format!(
                                "Replay apply failed for entity={:?} entity_id={} op={:?} event_id={} seq={}: {}",
                                entity, entity_id, op, event_id, seq, err
                            )))
                        })?
                        {
                            applied += 1;
                        }
                    }
                    Ok(applied)
                })();

                let _ = diesel::sql_query("PRAGMA defer_foreign_keys = OFF").execute(conn);
                result
            })
            .await
    }

    pub async fn acquire_cycle_lock(&self) -> Result<i64> {
        self.writer
            .exec(move |conn| {
                let state = sync_engine_state::table
                    .find(1)
                    .first::<SyncEngineStateDB>(conn)
                    .optional()
                    .map_err(StorageError::from)?;

                let next_lock_version = state.map(|s| s.lock_version + 1).unwrap_or(1);
                diesel::insert_into(sync_engine_state::table)
                    .values(SyncEngineStateDB {
                        id: 1,
                        lock_version: next_lock_version,
                        last_push_at: None,
                        last_pull_at: None,
                        last_error: None,
                        consecutive_failures: 0,
                        next_retry_at: None,
                        last_cycle_status: None,
                        last_cycle_duration_ms: None,
                    })
                    .on_conflict(sync_engine_state::id)
                    .do_update()
                    .set(sync_engine_state::lock_version.eq(next_lock_version))
                    .execute(conn)
                    .map_err(StorageError::from)?;

                Ok(next_lock_version)
            })
            .await
    }

    pub fn verify_cycle_lock(&self, expected_version: i64) -> Result<bool> {
        let mut conn = get_connection(&self.pool)?;
        let state = sync_engine_state::table
            .find(1)
            .first::<SyncEngineStateDB>(&mut conn)
            .optional()
            .map_err(StorageError::from)?;
        Ok(state
            .map(|s| s.lock_version == expected_version)
            .unwrap_or(false))
    }

    pub async fn mark_push_completed(&self) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let now = Utc::now().to_rfc3339();
                diesel::insert_into(sync_engine_state::table)
                    .values(SyncEngineStateDB {
                        id: 1,
                        lock_version: 0,
                        last_push_at: Some(now.clone()),
                        last_pull_at: None,
                        last_error: None,
                        consecutive_failures: 0,
                        next_retry_at: None,
                        last_cycle_status: Some("ok".to_string()),
                        last_cycle_duration_ms: None,
                    })
                    .on_conflict(sync_engine_state::id)
                    .do_update()
                    .set((
                        sync_engine_state::last_push_at.eq(Some(now)),
                        sync_engine_state::last_error.eq::<Option<String>>(None),
                        sync_engine_state::consecutive_failures.eq(0),
                        sync_engine_state::next_retry_at.eq::<Option<String>>(None),
                        sync_engine_state::last_cycle_status.eq(Some("ok")),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn mark_pull_completed(&self) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let now = Utc::now().to_rfc3339();
                diesel::insert_into(sync_engine_state::table)
                    .values(SyncEngineStateDB {
                        id: 1,
                        lock_version: 0,
                        last_push_at: None,
                        last_pull_at: Some(now.clone()),
                        last_error: None,
                        consecutive_failures: 0,
                        next_retry_at: None,
                        last_cycle_status: Some("ok".to_string()),
                        last_cycle_duration_ms: None,
                    })
                    .on_conflict(sync_engine_state::id)
                    .do_update()
                    .set((
                        sync_engine_state::last_pull_at.eq(Some(now)),
                        sync_engine_state::last_error.eq::<Option<String>>(None),
                        sync_engine_state::consecutive_failures.eq(0),
                        sync_engine_state::next_retry_at.eq::<Option<String>>(None),
                        sync_engine_state::last_cycle_status.eq(Some("ok")),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn mark_engine_error(&self, error_message: String) -> Result<()> {
        self.writer
            .exec(move |conn| {
                diesel::insert_into(sync_engine_state::table)
                    .values(SyncEngineStateDB {
                        id: 1,
                        lock_version: 0,
                        last_push_at: None,
                        last_pull_at: None,
                        last_error: Some(error_message.clone()),
                        consecutive_failures: 1,
                        next_retry_at: None,
                        last_cycle_status: Some("error".to_string()),
                        last_cycle_duration_ms: None,
                    })
                    .on_conflict(sync_engine_state::id)
                    .do_update()
                    .set((
                        sync_engine_state::last_error.eq(Some(error_message)),
                        sync_engine_state::consecutive_failures
                            .eq(sync_engine_state::consecutive_failures + 1),
                        sync_engine_state::last_cycle_status.eq(Some("error")),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub fn has_applied_event(&self, event_id_value: &str) -> Result<bool> {
        let mut conn = get_connection(&self.pool)?;
        let existing = sync_applied_events::table
            .find(event_id_value)
            .first::<SyncAppliedEventDB>(&mut conn)
            .optional()
            .map_err(StorageError::from)?;
        Ok(existing.is_some())
    }

    pub async fn mark_applied_event(
        &self,
        event_id_value: String,
        seq_value: i64,
        entity_value: SyncEntity,
        entity_id_value: String,
    ) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let row = SyncAppliedEventDB {
                    event_id: event_id_value.clone(),
                    seq: seq_value,
                    entity: enum_to_db(&entity_value)?,
                    entity_id: entity_id_value,
                    applied_at: Utc::now().to_rfc3339(),
                };

                diesel::insert_into(sync_applied_events::table)
                    .values(&row)
                    .on_conflict(sync_applied_events::event_id)
                    .do_nothing()
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn prune_applied_events_up_to_seq(&self, seq_cutoff: i64) -> Result<usize> {
        self.writer
            .exec(move |conn| {
                let deleted = diesel::delete(
                    sync_applied_events::table.filter(sync_applied_events::seq.le(seq_cutoff)),
                )
                .execute(conn)
                .map_err(StorageError::from)?;
                Ok(deleted)
            })
            .await
    }

    pub async fn mark_table_incremental_applied(&self, table_name_value: String) -> Result<()> {
        validate_sync_table(&table_name_value)?;
        self.writer
            .exec(move |conn| {
                let now = Utc::now().to_rfc3339();
                diesel::insert_into(sync_table_state::table)
                    .values(SyncTableStateDB {
                        table_name: table_name_value.clone(),
                        enabled: 1,
                        last_snapshot_restore_at: None,
                        last_incremental_apply_at: Some(now.clone()),
                    })
                    .on_conflict(sync_table_state::table_name)
                    .do_update()
                    .set((
                        sync_table_state::enabled.eq(1),
                        sync_table_state::last_incremental_apply_at.eq(Some(now)),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn mark_outbox_dead(
        &self,
        event_ids: Vec<String>,
        error_message: Option<String>,
        error_code: Option<String>,
    ) -> Result<()> {
        if event_ids.is_empty() {
            return Ok(());
        }

        self.writer
            .exec(move |conn| {
                diesel::update(sync_outbox::table.filter(sync_outbox::event_id.eq_any(event_ids)))
                    .set((
                        sync_outbox::status.eq(enum_to_db(&SyncOutboxStatus::Dead)?),
                        sync_outbox::last_error.eq(error_message),
                        sync_outbox::last_error_code.eq(error_code),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn mark_cycle_outcome(
        &self,
        status_value: String,
        duration_ms_value: i64,
        next_retry_at_value: Option<String>,
    ) -> Result<()> {
        self.writer
            .exec(move |conn| {
                diesel::insert_into(sync_engine_state::table)
                    .values(SyncEngineStateDB {
                        id: 1,
                        lock_version: 0,
                        last_push_at: None,
                        last_pull_at: None,
                        last_error: None,
                        consecutive_failures: 0,
                        next_retry_at: next_retry_at_value.clone(),
                        last_cycle_status: Some(status_value.clone()),
                        last_cycle_duration_ms: Some(duration_ms_value),
                    })
                    .on_conflict(sync_engine_state::id)
                    .do_update()
                    .set((
                        sync_engine_state::last_cycle_status.eq(Some(status_value)),
                        sync_engine_state::last_cycle_duration_ms.eq(Some(duration_ms_value)),
                        sync_engine_state::next_retry_at.eq(next_retry_at_value),
                    ))
                    .execute(conn)
                    .map_err(StorageError::from)?;
                Ok(())
            })
            .await
    }

    pub async fn export_snapshot_sqlite_image(&self, tables: Vec<String>) -> Result<Vec<u8>> {
        /// Per-table WHERE filters applied during snapshot export.
        /// Tables not listed here are exported unfiltered.
        const SYNC_TABLE_EXPORT_FILTERS: &[(&str, &str)] = &[(
            "holdings_snapshots",
            "source NOT IN ('CALCULATED', 'SYNTHETIC')",
        )];

        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let mut conn = get_connection(&pool)?;
            let table_set = if tables.is_empty() {
                APP_SYNC_TABLES
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
            } else {
                tables
            };
            for table in &table_set {
                validate_sync_table(table)?;
            }

            let snapshot_path =
                std::env::temp_dir().join(format!("wf_snapshot_export_{}.db", Uuid::now_v7()));
            let escaped_path = escape_sqlite_str(&snapshot_path.to_string_lossy());
            let snapshot_alias = format!("snapshot_export_{}", Uuid::now_v7().simple());
            let attach_sql = format!("ATTACH DATABASE '{}' AS {}", escaped_path, snapshot_alias);
            let tx_result = conn.immediate_transaction::<_, StorageError, _>(|tx| {
                diesel::sql_query(attach_sql.clone())
                    .execute(tx)
                    .map_err(StorageError::from)?;

                let run_export = (|| -> Result<()> {
                    for table in &table_set {
                        let table_ident = quote_identifier(table);
                        let filter = SYNC_TABLE_EXPORT_FILTERS
                            .iter()
                            .find(|(t, _)| *t == table.as_str())
                            .map(|(_, f)| *f);
                        let copy_sql = match filter {
                            Some(where_clause) => format!(
                                "CREATE TABLE {snapshot_alias}.{table_ident} AS SELECT * FROM main.{table_ident} WHERE {where_clause}"
                            ),
                            None => format!(
                                "CREATE TABLE {snapshot_alias}.{table_ident} AS SELECT * FROM main.{table_ident}"
                            ),
                        };
                        diesel::sql_query(copy_sql)
                            .execute(tx)
                            .map_err(StorageError::from)?;
                    }
                    Ok(())
                })();

                let detach_sql = format!("DETACH DATABASE {}", snapshot_alias);
                let _ = diesel::sql_query(detach_sql).execute(tx);
                run_export.map_err(StorageError::from)
            });
            if let Err(err) = tx_result {
                let _ = std::fs::remove_file(&snapshot_path);
                return Err(Error::from(err));
            }

            let payload = std::fs::read(&snapshot_path).map_err(|e| {
                Error::Database(DatabaseError::Internal(format!(
                    "Failed reading exported snapshot: {}",
                    e
                )))
            })?;
            let _ = std::fs::remove_file(snapshot_path);
            Ok(payload)
        })
        .await
        .map_err(|e| {
            Error::Database(DatabaseError::Internal(format!(
                "Snapshot export worker failed: {}",
                e
            )))
        })?
    }

    pub async fn restore_snapshot_tables_from_file(
        &self,
        snapshot_db_path: String,
        tables: Vec<String>,
        cursor_value: i64,
        device_id_value: String,
        key_version_value: Option<i32>,
    ) -> Result<()> {
        self.writer
            .exec(move |conn| {
                let table_set = if tables.is_empty() {
                    APP_SYNC_TABLES
                        .iter()
                        .map(|t| t.to_string())
                        .collect::<Vec<_>>()
                } else {
                    tables
                };
                for table in &table_set {
                    validate_sync_table(table)?;
                }

                let now = Utc::now().to_rfc3339();
                let escaped_path = escape_sqlite_str(&snapshot_db_path);
                let snapshot_alias = format!("snapshot_{}", Uuid::new_v4().simple());
                let attach_sql =
                    format!("ATTACH DATABASE '{}' AS {}", escaped_path, snapshot_alias);

                diesel::sql_query("PRAGMA foreign_keys = OFF")
                    .execute(conn)
                    .map_err(StorageError::from)?;
                diesel::sql_query(attach_sql)
                    .execute(conn)
                    .map_err(StorageError::from)?;

                let restore_result = (|| -> Result<()> {
                    // Bootstrap reset: clear control-plane sync state so stale events/metadata
                    // never leak into the newly restored snapshot baseline.
                    diesel::delete(sync_outbox::table)
                        .execute(conn)
                        .map_err(StorageError::from)?;
                    diesel::delete(sync_entity_metadata::table)
                        .execute(conn)
                        .map_err(StorageError::from)?;
                    diesel::delete(sync_applied_events::table)
                        .execute(conn)
                        .map_err(StorageError::from)?;
                    diesel::delete(sync_table_state::table)
                        .execute(conn)
                        .map_err(StorageError::from)?;

                    for table in &table_set {
                        let target_columns = load_table_columns(conn, "main", table)?;
                        let source_columns = load_table_columns(conn, &snapshot_alias, table)?;
                        let source_column_set =
                            source_columns.into_iter().collect::<HashSet<String>>();
                        let common_columns = target_columns
                            .into_iter()
                            .filter(|column| source_column_set.contains(column))
                            .collect::<Vec<_>>();
                        if common_columns.is_empty() {
                            return Err(Error::Database(DatabaseError::Internal(format!(
                                "Snapshot table '{}' has no compatible columns to restore",
                                table
                            ))));
                        }

                        let table_ident = quote_identifier(table);
                        let alias_ident = quote_identifier(&snapshot_alias);
                        let columns_sql = common_columns
                            .iter()
                            .map(|column| quote_identifier(column))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let copy_sql = format!(
                            "INSERT INTO {table_ident} ({columns_sql}) SELECT {columns_sql} FROM {alias_ident}.{table_ident}"
                        );
                        let clear_sql = format!("DELETE FROM {table_ident}");
                        diesel::sql_query(clear_sql)
                            .execute(conn)
                            .map_err(StorageError::from)?;
                        diesel::sql_query(copy_sql)
                            .execute(conn)
                            .map_err(StorageError::from)?;

                        let state_row = SyncTableStateDB {
                            table_name: table.clone(),
                            enabled: 1,
                            last_snapshot_restore_at: Some(now.clone()),
                            last_incremental_apply_at: None,
                        };
                        diesel::insert_into(sync_table_state::table)
                            .values(&state_row)
                            .on_conflict(sync_table_state::table_name)
                            .do_update()
                            .set((
                                sync_table_state::enabled.eq(1),
                                sync_table_state::last_snapshot_restore_at.eq(Some(now.clone())),
                            ))
                            .execute(conn)
                            .map_err(StorageError::from)?;
                    }

                    diesel::insert_into(sync_cursor::table)
                        .values(SyncCursorDB {
                            id: 1,
                            cursor: cursor_value,
                            updated_at: now.clone(),
                        })
                        .on_conflict(sync_cursor::id)
                        .do_update()
                        .set((
                            sync_cursor::cursor.eq(cursor_value),
                            sync_cursor::updated_at.eq(now.clone()),
                        ))
                        .execute(conn)
                        .map_err(StorageError::from)?;

                    diesel::insert_into(sync_device_config::table)
                        .values(SyncDeviceConfigDB {
                            device_id: device_id_value.clone(),
                            key_version: key_version_value,
                            trust_state: "trusted".to_string(),
                            last_bootstrap_at: Some(now.clone()),
                        })
                        .on_conflict(sync_device_config::device_id)
                        .do_update()
                        .set((
                            sync_device_config::key_version.eq(key_version_value),
                            sync_device_config::trust_state.eq("trusted"),
                            sync_device_config::last_bootstrap_at.eq(Some(now.clone())),
                        ))
                        .execute(conn)
                        .map_err(StorageError::from)?;

                    diesel::insert_into(sync_engine_state::table)
                        .values(SyncEngineStateDB {
                            id: 1,
                            lock_version: 0,
                            last_push_at: None,
                            last_pull_at: Some(now.clone()),
                            last_error: None,
                            consecutive_failures: 0,
                            next_retry_at: None,
                            last_cycle_status: Some("ok".to_string()),
                            last_cycle_duration_ms: None,
                        })
                        .on_conflict(sync_engine_state::id)
                        .do_update()
                        .set((
                            sync_engine_state::last_pull_at.eq(Some(now.clone())),
                            sync_engine_state::last_error.eq::<Option<String>>(None),
                            sync_engine_state::consecutive_failures.eq(0),
                            sync_engine_state::next_retry_at.eq::<Option<String>>(None),
                            sync_engine_state::last_cycle_status.eq(Some("ok")),
                        ))
                        .execute(conn)
                        .map_err(StorageError::from)?;

                    Ok(())
                })();

                let detach_sql = format!("DETACH DATABASE {}", snapshot_alias);
                let _ = diesel::sql_query(detach_sql).execute(conn);
                let _ = diesel::sql_query("PRAGMA foreign_keys = ON").execute(conn);
                restore_result
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::dsl::count_star;
    use tempfile::tempdir;

    use crate::db::{create_pool, get_connection, init, run_migrations, write_actor::spawn_writer};
    use crate::schema::{
        accounts, assets, platforms, sync_applied_events, sync_entity_metadata, sync_outbox,
    };

    fn setup_db() -> (
        Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
        WriteHandle,
    ) {
        // Ensure connect is "configured" so outbox writes work in tests
        std::env::set_var("CONNECT_API_URL", "http://test.local");

        let app_data = tempdir()
            .expect("tempdir")
            .keep()
            .to_string_lossy()
            .to_string();
        let db_path = init(&app_data).expect("init db");
        run_migrations(&db_path).expect("migrate db");
        let pool = create_pool(&db_path).expect("create pool");
        let writer = spawn_writer(pool.as_ref().clone());
        (pool, writer)
    }

    fn insert_account_for_test(conn: &mut SqliteConnection, account_id: &str) -> Result<()> {
        let sql = format!(
            "INSERT INTO accounts (id, name, account_type, `group`, currency, is_default, is_active, created_at, updated_at, platform_id, account_number, meta, provider, provider_account_id, is_archived, tracking_mode) VALUES ('{}', 'Sync Test', 'cash', NULL, 'USD', 1, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, NULL, NULL, NULL, NULL, 0, 'portfolio')",
            escape_sqlite_str(account_id)
        );
        diesel::sql_query(sql)
            .execute(conn)
            .map_err(StorageError::from)?;
        Ok(())
    }

    fn create_snapshot_db_with_account(account_id: &str) -> String {
        let app_data = tempdir()
            .expect("tempdir")
            .keep()
            .to_string_lossy()
            .to_string();
        let db_path = init(&app_data).expect("init db");
        run_migrations(&db_path).expect("migrate db");
        let pool = create_pool(&db_path).expect("create pool");
        let mut conn = get_connection(&pool).expect("conn");
        insert_account_for_test(&mut conn, account_id).expect("insert account");
        db_path
    }

    fn create_snapshot_db_with_assets_extra_column(asset_id: &str) -> String {
        let app_data = tempdir()
            .expect("tempdir")
            .keep()
            .to_string_lossy()
            .to_string();
        let db_path = init(&app_data).expect("init db");
        run_migrations(&db_path).expect("migrate db");
        let pool = create_pool(&db_path).expect("create pool");
        let mut conn = get_connection(&pool).expect("conn");
        diesel::sql_query("ALTER TABLE assets ADD COLUMN legacy_extra TEXT")
            .execute(&mut conn)
            .expect("add extra column");
        let insert_sql = format!(
            "INSERT INTO assets (id, kind, name, display_code, notes, metadata, is_active, quote_mode, quote_ccy, instrument_type, instrument_symbol, instrument_exchange_mic, provider_config, created_at, updated_at, legacy_extra) VALUES ('{}', 'INVESTMENT', 'Snapshot Asset', 'SNAP', NULL, NULL, 1, 'MANUAL', 'USD', NULL, NULL, NULL, NULL, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 'legacy')",
            escape_sqlite_str(asset_id)
        );
        diesel::sql_query(insert_sql)
            .execute(&mut conn)
            .expect("insert asset");
        db_path
    }

    fn count_account_rows(
        pool: &Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
        account_id: &str,
    ) -> i64 {
        let mut conn = get_connection(pool).expect("conn");
        accounts::table
            .filter(accounts::id.eq(account_id))
            .select(count_star())
            .first(&mut conn)
            .expect("count")
    }

    fn count_asset_rows(
        pool: &Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
        asset_id: &str,
    ) -> i64 {
        let mut conn = get_connection(pool).expect("conn");
        assets::table
            .filter(assets::id.eq(asset_id))
            .select(count_star())
            .first(&mut conn)
            .expect("count")
    }

    fn count_platform_rows(
        pool: &Arc<Pool<r2d2::ConnectionManager<SqliteConnection>>>,
        platform_id: &str,
    ) -> i64 {
        let mut conn = get_connection(pool).expect("conn");
        platforms::table
            .filter(platforms::id.eq(platform_id))
            .select(count_star())
            .first(&mut conn)
            .expect("count")
    }

    #[tokio::test]
    async fn creates_sync_foundation_tables() {
        let (pool, _writer) = setup_db();
        let mut conn = get_connection(&pool).expect("conn");
        for table in [
            "sync_applied_events",
            "sync_cursor",
            "sync_outbox",
            "sync_entity_metadata",
            "sync_device_config",
            "sync_engine_state",
            "sync_table_state",
        ] {
            let sql = format!(
                "SELECT COUNT(*) as c FROM sqlite_master WHERE type='table' AND name='{}'",
                table
            );
            #[derive(diesel::QueryableByName)]
            struct CountRow {
                #[diesel(sql_type = diesel::sql_types::BigInt)]
                c: i64,
            }
            let row = diesel::sql_query(sql)
                .get_result::<CountRow>(&mut conn)
                .expect("table exists");
            assert_eq!(row.c, 1, "missing table {table}");
        }
    }

    #[tokio::test]
    async fn outbox_write_rollback_keeps_mutation_atomic() {
        let (pool, writer) = setup_db();

        let tx_result = writer
            .exec(|conn| {
                insert_account_for_test(conn, "acc-sync-rollback")?;

                let mut req = OutboxWriteRequest::new(
                    SyncEntity::Account,
                    "acc-sync-rollback",
                    SyncOperation::Create,
                    serde_json::json!({ "id": "acc-sync-rollback" }),
                );
                req.event_id = Some("fixed-event-id".to_string());
                write_outbox_event(conn, req.clone())?;
                let _ = write_outbox_event(conn, req)?;
                Ok(())
            })
            .await;

        assert!(
            tx_result.is_err(),
            "expected duplicate outbox event_id failure"
        );

        let mut conn = get_connection(&pool).expect("conn");
        let account_count: i64 = accounts::table
            .filter(accounts::id.eq("acc-sync-rollback"))
            .select(count_star())
            .first(&mut conn)
            .expect("count");
        assert_eq!(account_count, 0, "account insert should be rolled back");
    }

    #[tokio::test]
    async fn snapshot_restore_sets_cursor_and_is_idempotent() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool.clone(), writer);
        let snapshot_path = create_snapshot_db_with_account("acc-from-snapshot");

        repo.restore_snapshot_tables_from_file(
            snapshot_path.clone(),
            vec!["accounts".to_string()],
            88,
            "device-1".to_string(),
            Some(1),
        )
        .await
        .expect("restore snapshot");

        assert_eq!(repo.get_cursor().expect("cursor"), 88);
        assert_eq!(count_account_rows(&pool, "acc-from-snapshot"), 1);

        repo.restore_snapshot_tables_from_file(
            snapshot_path,
            vec!["accounts".to_string()],
            88,
            "device-1".to_string(),
            Some(1),
        )
        .await
        .expect("second restore");

        assert_eq!(repo.get_cursor().expect("cursor"), 88);
        assert_eq!(count_account_rows(&pool, "acc-from-snapshot"), 1);
    }

    #[tokio::test]
    async fn snapshot_restore_error_keeps_existing_cursor() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool, writer);
        repo.set_cursor(15).await.expect("set cursor");

        let broken_snapshot_path = tempdir()
            .expect("tempdir")
            .keep()
            .join("broken_snapshot.db");
        std::fs::write(&broken_snapshot_path, b"not-a-sqlite-db").expect("write broken file");

        let result = repo
            .restore_snapshot_tables_from_file(
                broken_snapshot_path.to_string_lossy().to_string(),
                vec!["accounts".to_string()],
                22,
                "device-2".to_string(),
                Some(1),
            )
            .await;
        assert!(result.is_err(), "restore should fail for invalid snapshot");
        assert_eq!(repo.get_cursor().expect("cursor"), 15);
    }

    #[tokio::test]
    async fn snapshot_restore_handles_source_with_extra_columns() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool.clone(), writer);
        let snapshot_path = create_snapshot_db_with_assets_extra_column("asset-extra-column");

        repo.restore_snapshot_tables_from_file(
            snapshot_path,
            vec!["assets".to_string()],
            19,
            "device-1".to_string(),
            Some(1),
        )
        .await
        .expect("restore snapshot with extra source columns");

        assert_eq!(count_asset_rows(&pool, "asset-extra-column"), 1);
        assert_eq!(repo.get_cursor().expect("cursor"), 19);
    }

    #[tokio::test]
    async fn snapshot_restore_resets_sync_control_state() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool.clone(), writer);
        let snapshot_path = create_snapshot_db_with_account("acc-reset-state");

        {
            let mut conn = get_connection(&pool).expect("conn");
            write_outbox_event(
                &mut conn,
                OutboxWriteRequest::new(
                    SyncEntity::Account,
                    "acc-local-dirty",
                    SyncOperation::Update,
                    serde_json::json!({ "id": "acc-local-dirty", "name": "dirty" }),
                ),
            )
            .expect("write outbox");
        }
        repo.upsert_entity_metadata(SyncEntityMetadata {
            entity: SyncEntity::Account,
            entity_id: "acc-local-dirty".to_string(),
            last_event_id: "evt-local".to_string(),
            last_client_timestamp: chrono::Utc::now().to_rfc3339(),
            last_seq: 123,
        })
        .await
        .expect("upsert metadata");
        repo.mark_applied_event(
            "evt-applied-local".to_string(),
            124,
            SyncEntity::Account,
            "acc-local-dirty".to_string(),
        )
        .await
        .expect("mark applied");

        repo.restore_snapshot_tables_from_file(
            snapshot_path,
            vec!["accounts".to_string()],
            200,
            "device-1".to_string(),
            Some(1),
        )
        .await
        .expect("restore snapshot");

        let mut conn = get_connection(&pool).expect("conn");
        let outbox_count: i64 = sync_outbox::table
            .select(count_star())
            .first(&mut conn)
            .expect("count outbox");
        let metadata_count: i64 = sync_entity_metadata::table
            .select(count_star())
            .first(&mut conn)
            .expect("count metadata");
        let applied_count: i64 = sync_applied_events::table
            .select(count_star())
            .first(&mut conn)
            .expect("count applied");

        assert_eq!(outbox_count, 0);
        assert_eq!(metadata_count, 0);
        assert_eq!(applied_count, 0);
    }

    #[tokio::test]
    async fn outbox_uses_trusted_device_key_version_by_default() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool, writer.clone());

        repo.upsert_device_config("device-1".to_string(), Some(3), "trusted".to_string())
            .await
            .expect("upsert device config");

        writer
            .exec(|conn| {
                write_outbox_event(
                    conn,
                    OutboxWriteRequest::new(
                        SyncEntity::Account,
                        "acc-key-version",
                        SyncOperation::Create,
                        serde_json::json!({ "id": "acc-key-version" }),
                    ),
                )?;
                Ok(())
            })
            .await
            .expect("write outbox");

        let pending = repo.list_pending_outbox(10).expect("list pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload_key_version, 3);
    }

    #[tokio::test]
    async fn replay_rejects_payload_with_mismatched_pk() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool, writer);

        let result = repo
            .apply_remote_event_lww(
                SyncEntity::Account,
                "account-entity-id".to_string(),
                SyncOperation::Update,
                "evt-1".to_string(),
                "2026-02-12T00:00:00Z".to_string(),
                1,
                serde_json::json!({
                    "id": "different-account-id"
                }),
            )
            .await;

        assert!(result.is_err(), "expected PK mismatch to be rejected");
    }

    #[tokio::test]
    async fn replay_applies_platform_create_then_update() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool.clone(), writer);
        let platform_id = "platform-sync-1".to_string();

        let created = repo
            .apply_remote_event_lww(
                SyncEntity::Platform,
                platform_id.clone(),
                SyncOperation::Create,
                "evt-platform-create".to_string(),
                "2026-02-16T00:00:00Z".to_string(),
                1,
                serde_json::json!({
                    "id": platform_id,
                    "name": "Initial Platform",
                    "url": "https://broker.example/initial",
                    "external_id": "ext-platform-1",
                    "kind": "BROKERAGE",
                    "website_url": "https://broker.example",
                    "logo_url": "https://broker.example/logo.png"
                }),
            )
            .await
            .expect("apply platform create");
        assert!(created, "expected platform create to apply");

        let updated = repo
            .apply_remote_event_lww(
                SyncEntity::Platform,
                "platform-sync-1".to_string(),
                SyncOperation::Update,
                "evt-platform-update".to_string(),
                "2026-02-16T00:00:01Z".to_string(),
                2,
                serde_json::json!({
                    "id": "platform-sync-1",
                    "name": "Renamed Platform",
                    "url": "https://broker.example/updated",
                    "external_id": "ext-platform-1",
                    "kind": "BROKERAGE",
                    "website_url": "https://broker.example/updated",
                    "logo_url": "https://broker.example/logo-v2.png"
                }),
            )
            .await
            .expect("apply platform update");
        assert!(updated, "expected platform update to apply");
        assert_eq!(count_platform_rows(&pool, "platform-sync-1"), 1);

        let mut conn = get_connection(&pool).expect("conn");
        let (name_value, url_value): (Option<String>, String) = platforms::table
            .filter(platforms::id.eq("platform-sync-1"))
            .select((platforms::name, platforms::url))
            .first(&mut conn)
            .expect("platform row");
        assert_eq!(name_value.as_deref(), Some("Renamed Platform"));
        assert_eq!(url_value, "https://broker.example/updated");
    }

    #[tokio::test]
    async fn replay_batch_applies_out_of_order_account_and_platform_events() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool.clone(), writer);

        let applied = repo
            .apply_remote_events_lww_batch(vec![
                (
                    SyncEntity::Account,
                    "acc-batch-platform".to_string(),
                    SyncOperation::Create,
                    "evt-account-create".to_string(),
                    "2026-02-17T00:00:00Z".to_string(),
                    10,
                    serde_json::json!({
                        "id": "acc-batch-platform",
                        "name": "Batch Account",
                        "account_type": "cash",
                        "group": serde_json::Value::Null,
                        "currency": "USD",
                        "is_default": false,
                        "is_active": true,
                        "platform_id": "platform-batch",
                        "account_number": serde_json::Value::Null,
                        "meta": serde_json::Value::Null,
                        "provider": serde_json::Value::Null,
                        "provider_account_id": serde_json::Value::Null,
                        "is_archived": false,
                        "tracking_mode": "portfolio"
                    }),
                ),
                (
                    SyncEntity::Platform,
                    "platform-batch".to_string(),
                    SyncOperation::Create,
                    "evt-platform-create".to_string(),
                    "2026-02-17T00:00:01Z".to_string(),
                    11,
                    serde_json::json!({
                        "id": "platform-batch",
                        "name": "Batch Platform",
                        "url": "https://batch.example",
                        "external_id": serde_json::Value::Null,
                        "kind": "BROKERAGE",
                        "website_url": serde_json::Value::Null,
                        "logo_url": serde_json::Value::Null
                    }),
                ),
            ])
            .await
            .expect("apply replay batch");

        assert_eq!(applied, 2, "both events should apply in one batch");
        assert_eq!(count_account_rows(&pool, "acc-batch-platform"), 1);
        assert_eq!(count_platform_rows(&pool, "platform-batch"), 1);

        let mut conn = get_connection(&pool).expect("conn");
        let account_platform_id: Option<String> = accounts::table
            .filter(accounts::id.eq("acc-batch-platform"))
            .select(accounts::platform_id)
            .first(&mut conn)
            .expect("account row");
        assert_eq!(account_platform_id.as_deref(), Some("platform-batch"));
    }

    #[tokio::test]
    async fn snapshot_export_returns_sqlite_image() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool.clone(), writer);
        let mut conn = get_connection(&pool).expect("conn");
        insert_account_for_test(&mut conn, "acc-export").expect("insert account");

        let payload = repo
            .export_snapshot_sqlite_image(vec!["accounts".to_string()])
            .await
            .expect("export snapshot");
        assert!(
            payload.starts_with(b"SQLite format 3\0"),
            "expected exported payload to be sqlite image"
        );
    }

    #[test]
    fn quote_identifier_escapes_backticks() {
        assert_eq!(quote_identifier("col`name"), "`col``name`");
    }

    #[test]
    fn escape_sqlite_str_escapes_single_quotes() {
        assert_eq!(escape_sqlite_str("O'Brien"), "O''Brien");
    }

    #[test]
    fn json_value_to_sql_literal_handles_injection_attempt() {
        let malicious = serde_json::Value::String("'; DROP TABLE accounts; --".to_string());
        let sql = json_value_to_sql_literal(&malicious);
        assert_eq!(sql, "'''; DROP TABLE accounts; --'");
    }

    #[tokio::test]
    async fn replay_rejects_unknown_columns() {
        let (pool, writer) = setup_db();
        let repo = AppSyncRepository::new(pool, writer);

        let result = repo
            .apply_remote_event_lww(
                SyncEntity::Account,
                "acc-unknown-col".to_string(),
                SyncOperation::Create,
                "evt-unk-col".to_string(),
                "2026-02-15T00:00:00Z".to_string(),
                1,
                serde_json::json!({
                    "id": "acc-unknown-col",
                    "nonexistent_column": "value"
                }),
            )
            .await;

        assert!(result.is_err(), "expected unknown column to be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("nonexistent_column"),
            "error should mention the bad column: {}",
            err_msg
        );
    }
}
