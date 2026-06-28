use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde::Serialize;
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use tokio::sync::broadcast;

const STATE_EVENT_BATCH_LIMIT: i64 = 100;

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

pub async fn latest_state_event_id(pool: &SqlitePool) -> Result<i64, StateEventError> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(id), 0) FROM state_events")
            .fetch_one(pool)
            .await?,
    )
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
