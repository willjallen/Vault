use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde::Serialize;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use thiserror::Error;
use tokio::sync::broadcast;

const STATE_EVENT_BATCH_LIMIT: i64 = 100;
const STATE_EVENT_MAX_REPLAY_EVENTS: u32 = 10_000;
const STATE_EVENT_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(168);
const STATE_EVENT_COMPACTED_TYPE: &str = "state.compacted";
const FULL_STATE_EVENT_RESOURCES: &[&str] = &[
    "admin",
    "contents",
    "document_detail",
    "my_edits",
    "preferences",
    "settings",
    "sidebar",
];

static STATE_EVENT_NOTIFIER: OnceLock<broadcast::Sender<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateEventPayload {
    #[serde(rename = "type")]
    pub event_type: String,
    pub resources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEventRecord {
    pub id: i64,
    pub payload: StateEventPayload,
}

#[derive(Debug, Error)]
pub enum StateEventError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, FromRow)]
struct StateEventRow {
    id: i64,
    event_type: String,
    resources: String,
}

#[derive(Debug, Clone, Copy)]
pub struct StateEventRetentionPolicy {
    max_replay_events: u32,
    max_age: std::time::Duration,
}

impl StateEventRetentionPolicy {
    #[doc(hidden)]
    #[must_use]
    pub const fn new(max_replay_events: u32, max_age: std::time::Duration) -> Self {
        Self {
            max_replay_events,
            max_age,
        }
    }
}

const DEFAULT_STATE_EVENT_RETENTION: StateEventRetentionPolicy =
    StateEventRetentionPolicy::new(STATE_EVENT_MAX_REPLAY_EVENTS, STATE_EVENT_MAX_AGE);

pub async fn latest_state_event_id(pool: &SqlitePool) -> Result<i64, StateEventError> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(id), 0) FROM state_events")
            .fetch_one(pool)
            .await?,
    )
}

pub async fn state_event_resume_id(
    pool: &SqlitePool,
    requested_id: i64,
) -> Result<i64, StateEventError> {
    let (latest_id, marker_id) = sqlx::query_as::<_, (i64, i64)>(
        r"
        SELECT
            COALESCE(MAX(id), 0),
            COALESCE(MAX(CASE WHEN event_type = ? THEN id END), 0)
        FROM state_events
        ",
    )
    .bind(STATE_EVENT_COMPACTED_TYPE)
    .fetch_one(pool)
    .await?;
    if marker_id > 0 && (requested_id < marker_id || requested_id > latest_id) {
        return Ok(marker_id - 1);
    }
    if requested_id > latest_id {
        return Ok(0);
    }
    Ok(requested_id)
}

pub async fn compact_state_events(pool: &SqlitePool) -> Result<(), StateEventError> {
    compact_state_events_with_policy(pool, DEFAULT_STATE_EVENT_RETENTION).await
}

#[doc(hidden)]
pub async fn compact_state_events_with_policy(
    pool: &SqlitePool,
    policy: StateEventRetentionPolicy,
) -> Result<(), StateEventError> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (minimum_id, marker_id) = sqlx::query_as::<_, (Option<i64>, Option<i64>)>(
        r"
        SELECT
            MIN(id),
            MAX(CASE WHEN event_type = ? THEN id END)
        FROM state_events
        ",
    )
    .bind(STATE_EVENT_COMPACTED_TYPE)
    .fetch_one(&mut *transaction)
    .await?;
    let Some(minimum_id) = minimum_id else {
        replace_state_events_with_compaction_marker_in_tx(&mut transaction).await?;
        transaction.commit().await?;
        return Ok(());
    };

    let count_boundary = sqlx::query_scalar::<_, i64>(
        r"
        SELECT id
        FROM state_events
        ORDER BY id DESC
        LIMIT 1 OFFSET ?
        ",
    )
    .bind(i64::from(policy.max_replay_events))
    .fetch_optional(&mut *transaction)
    .await?;
    let max_age_seconds = i64::try_from(policy.max_age.as_secs()).unwrap_or(i64::MAX);
    let age_modifier = format!("-{max_age_seconds} seconds");
    let age_boundary = sqlx::query_scalar::<_, Option<i64>>(
        r"
        SELECT MAX(id)
        FROM state_events
        WHERE event_type <> ?
          AND created_at < datetime('now', ?)
        ",
    )
    .bind(STATE_EVENT_COMPACTED_TYPE)
    .bind(age_modifier)
    .fetch_one(&mut *transaction)
    .await?;

    let mut boundary_id = marker_id.unwrap_or(minimum_id);
    if let Some(candidate) = count_boundary {
        boundary_id = boundary_id.max(candidate);
    }
    if let Some(candidate) = age_boundary {
        boundary_id = boundary_id.max(candidate);
    }

    let resources = full_state_event_resources_json();
    sqlx::query(
        r"
        UPDATE state_events
        SET event_type = ?, resources = ?
        WHERE id = ?
          AND (event_type <> ? OR resources <> ?)
        ",
    )
    .bind(STATE_EVENT_COMPACTED_TYPE)
    .bind(&resources)
    .bind(boundary_id)
    .bind(STATE_EVENT_COMPACTED_TYPE)
    .bind(&resources)
    .execute(&mut *transaction)
    .await?;
    if minimum_id < boundary_id {
        sqlx::query("DELETE FROM state_events WHERE id < ?")
            .bind(boundary_id)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

pub async fn state_events_after(
    pool: &SqlitePool,
    last_id: i64,
) -> Result<Vec<StateEventRecord>, StateEventError> {
    let rows = sqlx::query_as::<_, StateEventRow>(
        r"
        SELECT id, event_type, resources
        FROM state_events
        WHERE id > ?
        ORDER BY id
        LIMIT ?
        ",
    )
    .bind(last_id)
    .bind(STATE_EVENT_BATCH_LIMIT)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(state_event_record).collect()
}

pub async fn record_state_event(
    pool: &SqlitePool,
    event_type: &str,
    resources: &[&str],
) -> Result<(), StateEventError> {
    let resources_json = state_event_resources_json(resources);
    if resources_json == "[]" {
        return Ok(());
    }
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(event_type)
    .bind(resources_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_state_event_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event_type: &str,
    resources: &[&str],
) -> Result<(), sqlx::Error> {
    let resources_json = state_event_resources_json(resources);
    if resources_json == "[]" {
        return Ok(());
    }
    sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(event_type)
    .bind(resources_json)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn replace_state_events_with_compaction_marker_in_tx(
    transaction: &mut Transaction<'_, Sqlite>,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        r"
        INSERT INTO state_events (event_type, resources)
        VALUES (?, ?)
        ",
    )
    .bind(STATE_EVENT_COMPACTED_TYPE)
    .bind(full_state_event_resources_json())
    .execute(&mut **transaction)
    .await?;
    let marker_id = result.last_insert_rowid();
    sqlx::query("DELETE FROM state_events WHERE id < ?")
        .bind(marker_id)
        .execute(&mut **transaction)
        .await?;
    Ok(marker_id)
}

#[must_use]
pub fn subscribe_state_events() -> broadcast::Receiver<()> {
    notifier().subscribe()
}

pub fn notify_state_event_committed() {
    let _ = notifier().send(());
}

fn state_event_record(row: StateEventRow) -> Result<StateEventRecord, StateEventError> {
    Ok(StateEventRecord {
        id: row.id,
        payload: StateEventPayload {
            event_type: row.event_type,
            resources: normalized_resources(&row.resources)?,
        },
    })
}

#[must_use]
pub fn state_event_resources_json(resources: &[&str]) -> String {
    serde_json::to_string(&normalized_resource_names(resources))
        .expect("state event resources should serialize")
}

fn full_state_event_resources_json() -> String {
    state_event_resources_json(FULL_STATE_EVENT_RESOURCES)
}

fn normalized_resource_names(resources: &[&str]) -> Vec<String> {
    resources
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalized_resources(raw: &str) -> Result<Vec<String>, StateEventError> {
    let values = serde_json::from_str::<Vec<String>>(raw)?;
    let values = values.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(normalized_resource_names(&values))
}

fn notifier() -> &'static broadcast::Sender<()> {
    STATE_EVENT_NOTIFIER.get_or_init(|| {
        let (sender, _receiver) = broadcast::channel(1024);
        sender
    })
}
