use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Row, Sqlite, SqliteConnection, Transaction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SchemaMetadata {
    tables: BTreeMap<String, TableMetadata>,
    views: BTreeMap<String, ViewMetadata>,
    triggers: BTreeMap<String, TriggerMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableMetadata {
    create_sql: String,
    columns: BTreeMap<String, ColumnMetadata>,
    named_indexes: BTreeMap<String, IndexMetadata>,
    unique_constraints: BTreeSet<Vec<String>>,
    foreign_keys: BTreeSet<ForeignKeyMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnMetadata {
    declared_type: String,
    default_value: Option<String>,
    not_null: bool,
    primary_key_position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexMetadata {
    create_sql: String,
    columns: Vec<String>,
    unique: bool,
    where_clause: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewMetadata {
    create_sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TriggerMetadata {
    table_name: String,
    create_sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ForeignKeyMetadata {
    id: i64,
    sequence: i64,
    from_column: String,
    foreign_table: String,
    foreign_column: String,
    on_delete: String,
    on_update: String,
    match_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct UserSchemaObject {
    pub(super) object_type: String,
    pub(super) name: String,
}

/// A connection whose schema can be inspected without acquiring another pool
/// connection between the migration and its validation.
pub(super) trait SchemaConnection {
    fn as_sqlite_connection(&mut self) -> &mut SqliteConnection;
}

impl SchemaConnection for SqliteConnection {
    fn as_sqlite_connection(&mut self) -> &mut SqliteConnection {
        self
    }
}

impl SchemaConnection for Transaction<'_, Sqlite> {
    fn as_sqlite_connection(&mut self) -> &mut SqliteConnection {
        self
    }
}

pub(super) async fn schema_metadata<C>(source: &mut C) -> anyhow::Result<SchemaMetadata>
where
    C: SchemaConnection + ?Sized,
{
    let connection = source.as_sqlite_connection();
    let mut tables = BTreeMap::new();
    for table_name in user_table_names_on_connection(connection).await? {
        tables.insert(
            table_name.clone(),
            table_metadata(connection, &table_name).await?,
        );
    }

    let view_rows = sqlx::query(
        r"
        SELECT name, sql
        FROM sqlite_master
        WHERE type = 'view'
          AND name NOT LIKE 'sqlite_%'
        ORDER BY name
        ",
    )
    .fetch_all(&mut *connection)
    .await?;
    let views = view_rows
        .into_iter()
        .map(|row| {
            let name = row.try_get::<String, _>("name")?;
            let create_sql = normalized_create_sql(&row, "sql")?;
            Ok((name, ViewMetadata { create_sql }))
        })
        .collect::<Result<BTreeMap<_, _>, anyhow::Error>>()?;

    let trigger_rows = sqlx::query(
        r"
        SELECT name, tbl_name, sql
        FROM sqlite_master
        WHERE type = 'trigger'
          AND name NOT LIKE 'sqlite_%'
        ORDER BY name
        ",
    )
    .fetch_all(&mut *connection)
    .await?;
    let triggers = trigger_rows
        .into_iter()
        .map(|row| {
            let name = row.try_get::<String, _>("name")?;
            let table_name = row.try_get::<String, _>("tbl_name")?;
            let create_sql = normalized_create_sql(&row, "sql")?;
            Ok((
                name,
                TriggerMetadata {
                    table_name,
                    create_sql,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, anyhow::Error>>()?;

    Ok(SchemaMetadata {
        tables,
        views,
        triggers,
    })
}

pub(super) async fn user_schema_objects<C>(
    source: &mut C,
) -> anyhow::Result<BTreeSet<UserSchemaObject>>
where
    C: SchemaConnection + ?Sized,
{
    user_schema_objects_on_connection(source.as_sqlite_connection()).await
}

pub(super) fn validate_schema_metadata_exact(
    expected: &SchemaMetadata,
    live: &SchemaMetadata,
    mismatch_reason: impl std::fmt::Display,
) -> anyhow::Result<()> {
    if live != expected {
        return Err(schema_incompatible(format!(
            "{mismatch_reason}: {}",
            first_schema_difference(expected, live)
        )));
    }
    Ok(())
}

fn first_schema_difference(expected: &SchemaMetadata, live: &SchemaMetadata) -> String {
    for table_name in expected.tables.keys() {
        if !live.tables.contains_key(table_name) {
            return format!("missing table {table_name}");
        }
    }
    for table_name in live.tables.keys() {
        if !expected.tables.contains_key(table_name) {
            return format!("unexpected table {table_name}");
        }
    }
    for (table_name, expected_table) in &expected.tables {
        if live.tables.get(table_name) != Some(expected_table) {
            return format!("definition mismatch for table {table_name}");
        }
    }

    for view_name in expected.views.keys() {
        if !live.views.contains_key(view_name) {
            return format!("missing view {view_name}");
        }
    }
    for view_name in live.views.keys() {
        if !expected.views.contains_key(view_name) {
            return format!("unexpected view {view_name}");
        }
    }
    for (view_name, expected_view) in &expected.views {
        if live.views.get(view_name) != Some(expected_view) {
            return format!("definition mismatch for view {view_name}");
        }
    }

    for trigger_name in expected.triggers.keys() {
        if !live.triggers.contains_key(trigger_name) {
            return format!("missing trigger {trigger_name}");
        }
    }
    for trigger_name in live.triggers.keys() {
        if !expected.triggers.contains_key(trigger_name) {
            return format!("unexpected trigger {trigger_name}");
        }
    }
    for (trigger_name, expected_trigger) in &expected.triggers {
        if live.triggers.get(trigger_name) != Some(expected_trigger) {
            return format!("definition mismatch for trigger {trigger_name}");
        }
    }

    "unknown schema metadata mismatch".to_string()
}

pub(super) fn schema_incompatible(reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(
        "Database is incompatible with this app version. \
         Startup refused to alter or drop existing metadata automatically. {reason}"
    )
}

async fn user_schema_objects_on_connection(
    connection: &mut SqliteConnection,
) -> anyhow::Result<BTreeSet<UserSchemaObject>> {
    let rows = sqlx::query(
        r"
        SELECT type, name
        FROM sqlite_master
        WHERE type IN ('table', 'index', 'view', 'trigger')
          AND name NOT LIKE 'sqlite_%'
        ORDER BY type, name
        ",
    )
    .fetch_all(&mut *connection)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(UserSchemaObject {
                object_type: row.try_get::<String, _>("type")?,
                name: row.try_get::<String, _>("name")?,
            })
        })
        .collect::<Result<BTreeSet<_>, sqlx::Error>>()
        .map_err(Into::into)
}

async fn user_table_names_on_connection(
    connection: &mut SqliteConnection,
) -> anyhow::Result<BTreeSet<String>> {
    Ok(sqlx::query_scalar::<_, String>(
        r"
        SELECT name
        FROM sqlite_master
        WHERE type = 'table'
          AND name NOT LIKE 'sqlite_%'
        ORDER BY name
        ",
    )
    .fetch_all(&mut *connection)
    .await?
    .into_iter()
    .collect())
}

async fn table_metadata(
    connection: &mut SqliteConnection,
    table_name: &str,
) -> anyhow::Result<TableMetadata> {
    let create_sql = object_create_sql(connection, "table", table_name).await?;
    let columns = column_metadata(connection, table_name).await?;
    let (named_indexes, unique_constraints) = index_metadata(connection, table_name).await?;
    let foreign_keys = foreign_key_metadata(connection, table_name).await?;
    Ok(TableMetadata {
        create_sql,
        columns,
        named_indexes,
        unique_constraints,
        foreign_keys,
    })
}

async fn column_metadata(
    connection: &mut SqliteConnection,
    table_name: &str,
) -> anyhow::Result<BTreeMap<String, ColumnMetadata>> {
    let rows = sqlx::query(&format!("PRAGMA table_info({})", quote_ident(table_name)))
        .fetch_all(&mut *connection)
        .await?;
    let mut columns = BTreeMap::new();
    for row in rows {
        let name = row.try_get::<String, _>("name")?;
        let declared_type = row.try_get::<String, _>("type")?;
        columns.insert(
            name,
            ColumnMetadata {
                declared_type: normalize_sql(&declared_type),
                default_value: row
                    .try_get::<Option<String>, _>("dflt_value")?
                    .map(|value| normalize_sql(&value)),
                not_null: row.try_get::<i64, _>("notnull")? != 0,
                primary_key_position: row.try_get::<i64, _>("pk")?,
            },
        );
    }
    Ok(columns)
}

async fn index_metadata(
    connection: &mut SqliteConnection,
    table_name: &str,
) -> anyhow::Result<(BTreeMap<String, IndexMetadata>, BTreeSet<Vec<String>>)> {
    let rows = sqlx::query(&format!("PRAGMA index_list({})", quote_ident(table_name)))
        .fetch_all(&mut *connection)
        .await?;
    let mut named_indexes = BTreeMap::new();
    let mut unique_constraints = BTreeSet::new();
    for row in rows {
        let name = row.try_get::<String, _>("name")?;
        let unique = row.try_get::<i64, _>("unique")? != 0;
        let origin = row.try_get::<String, _>("origin")?;
        let columns = index_columns(connection, &name).await?;
        match origin.as_str() {
            "u" => {
                unique_constraints.insert(columns);
            }
            "c" => {
                let create_sql = object_create_sql(connection, "index", &name).await?;
                named_indexes.insert(
                    name.clone(),
                    IndexMetadata {
                        create_sql,
                        columns,
                        unique,
                        where_clause: index_where_clause(connection, &name).await?,
                    },
                );
            }
            _ => {}
        }
    }
    Ok((named_indexes, unique_constraints))
}

async fn index_columns(
    connection: &mut SqliteConnection,
    index_name: &str,
) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(&format!("PRAGMA index_info({})", quote_ident(index_name)))
        .fetch_all(&mut *connection)
        .await?;
    let mut columns = rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get::<i64, _>("seqno")?,
                row.try_get::<String, _>("name")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    columns.sort_by_key(|(sequence, _)| *sequence);
    Ok(columns.into_iter().map(|(_, name)| name).collect())
}

async fn index_where_clause(
    connection: &mut SqliteConnection,
    index_name: &str,
) -> anyhow::Result<String> {
    let sql: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?")
            .bind(index_name)
            .fetch_optional(&mut *connection)
            .await?
            .flatten();
    let normalized = sql.as_deref().map(normalize_sql).unwrap_or_default();
    Ok(normalized
        .split_once(" where ")
        .map_or(String::new(), |(_, where_clause)| where_clause.to_string()))
}

async fn foreign_key_metadata(
    connection: &mut SqliteConnection,
    table_name: &str,
) -> anyhow::Result<BTreeSet<ForeignKeyMetadata>> {
    let rows = sqlx::query(&format!(
        "PRAGMA foreign_key_list({})",
        quote_ident(table_name)
    ))
    .fetch_all(&mut *connection)
    .await?;
    let mut foreign_keys = BTreeSet::new();
    for row in rows {
        foreign_keys.insert(ForeignKeyMetadata {
            id: row.try_get::<i64, _>("id")?,
            sequence: row.try_get::<i64, _>("seq")?,
            from_column: row.try_get::<String, _>("from")?,
            foreign_table: row.try_get::<String, _>("table")?,
            foreign_column: row.try_get::<String, _>("to")?,
            on_delete: row.try_get::<String, _>("on_delete")?.to_ascii_uppercase(),
            on_update: row.try_get::<String, _>("on_update")?.to_ascii_uppercase(),
            match_type: row.try_get::<String, _>("match")?.to_ascii_uppercase(),
        });
    }
    Ok(foreign_keys)
}

async fn object_create_sql(
    connection: &mut SqliteConnection,
    object_type: &str,
    object_name: &str,
) -> anyhow::Result<String> {
    let create_sql: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = ? AND name = ?")
            .bind(object_type)
            .bind(object_name)
            .fetch_optional(&mut *connection)
            .await?
            .flatten();
    create_sql.map_or_else(
        || {
            Err(anyhow::anyhow!(
                "{object_type} {object_name:?} has no CREATE SQL in sqlite_master"
            ))
        },
        |sql| Ok(normalize_sql(&sql)),
    )
}

fn normalized_create_sql(row: &sqlx::sqlite::SqliteRow, column: &str) -> anyhow::Result<String> {
    let create_sql = row.try_get::<Option<String>, _>(column)?;
    create_sql.map_or_else(
        || {
            Err(anyhow::anyhow!(
                "schema object has no CREATE SQL in sqlite_master"
            ))
        },
        |sql| Ok(normalize_sql(&sql)),
    )
}

fn normalize_sql(sql: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Single,
        Double,
        Backtick,
        Bracket,
    }

    let mut normalized = String::with_capacity(sql.len());
    let mut characters = sql.chars().peekable();
    let mut quote = None;
    let mut pending_space = false;

    while let Some(character) = characters.next() {
        if let Some(active_quote) = quote {
            normalized.push(character);
            let closing = match active_quote {
                Quote::Single => '\'',
                Quote::Double => '"',
                Quote::Backtick => '`',
                Quote::Bracket => ']',
            };
            if character == closing {
                let doubled_quote = active_quote != Quote::Bracket
                    && characters.peek().is_some_and(|next| *next == closing);
                if doubled_quote {
                    normalized.push(characters.next().expect("peeked quote"));
                } else {
                    quote = None;
                }
            }
            continue;
        }

        if character.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !normalized.is_empty() {
            normalized.push(' ');
        }
        pending_space = false;

        quote = match character {
            '\'' => Some(Quote::Single),
            '"' => Some(Quote::Double),
            '`' => Some(Quote::Backtick),
            '[' => Some(Quote::Bracket),
            _ => None,
        };
        if quote.is_some() {
            normalized.push(character);
        } else {
            normalized.push(character.to_ascii_lowercase());
        }
    }

    normalized
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
