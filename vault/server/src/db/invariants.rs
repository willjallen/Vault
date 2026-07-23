use std::collections::{HashMap, HashSet, VecDeque};

use sqlx::{FromRow, SqliteConnection};

use crate::root_folders::RootFolderDefinition;

// This engine contains only invariants shared by every supported persisted
// format. Version-specific values and exceptions are supplied by each
// migration module. A future rule that is not valid for an older source must
// be represented in the version contract and introduced by a new migration,
// never added here unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RootInvariantDefinition {
    pub key: &'static str,
    pub stored_name: &'static str,
    pub public_path_prefix: &'static str,
    pub allows_folder_descendants: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RootNameAlias {
    pub root_key: &'static str,
    pub stored_name: &'static str,
}

#[derive(Debug, Clone, FromRow)]
struct FolderInvariantRow {
    id: i64,
    root_key: String,
    parent_id: Option<i64>,
    name: String,
    is_root: i64,
}

pub(super) async fn validate(
    connection: &mut SqliteConnection,
    root_definitions: &[RootInvariantDefinition],
    root_name_aliases: &[RootNameAlias],
) -> anyhow::Result<()> {
    let folders = sqlx::query_as::<_, FolderInvariantRow>(
        r"
        SELECT id, root_key, parent_id, name, is_root
        FROM folders
        ORDER BY id
        ",
    )
    .fetch_all(&mut *connection)
    .await?;
    validate_rows(&folders, root_definitions, root_name_aliases)
}

pub(super) async fn validate_current(
    connection: &mut SqliteConnection,
    root_definitions: &[RootFolderDefinition],
) -> anyhow::Result<()> {
    let definitions = root_definitions
        .iter()
        .map(|definition| RootInvariantDefinition {
            key: definition.key,
            stored_name: definition.stored_name,
            public_path_prefix: definition.public_path_prefix,
            allows_folder_descendants: definition.allows_folder_descendants,
        })
        .collect::<Vec<_>>();
    validate(connection, &definitions, &[]).await
}

fn validate_rows(
    folders: &[FolderInvariantRow],
    root_definitions: &[RootInvariantDefinition],
    root_name_aliases: &[RootNameAlias],
) -> anyhow::Result<()> {
    if folders.is_empty() {
        anyhow::bail!("folder_invariant_failed reason=no_folders");
    }

    validate_root_flags(folders)?;
    let by_id = folders
        .iter()
        .map(|folder| (folder.id, folder))
        .collect::<HashMap<_, _>>();
    let roots = folders
        .iter()
        .filter(|folder| folder.is_root == 1)
        .collect::<Vec<_>>();
    validate_roots(&roots, root_definitions, root_name_aliases)?;
    let children = validate_parent_edges(folders, &by_id, root_definitions)?;
    validate_reachability(folders, &roots, &children)
}

fn validate_root_flags(folders: &[FolderInvariantRow]) -> anyhow::Result<()> {
    for folder in folders {
        if folder.is_root != 0 && folder.is_root != 1 {
            anyhow::bail!(
                "folder_invariant_failed reason=invalid_root_flag folder_id={}",
                folder.id
            );
        }
    }
    Ok(())
}

fn validate_roots(
    roots: &[&FolderInvariantRow],
    root_definitions: &[RootInvariantDefinition],
    root_name_aliases: &[RootNameAlias],
) -> anyhow::Result<()> {
    if roots.len() != root_definitions.len() {
        anyhow::bail!(
            "folder_invariant_failed reason=wrong_root_count expected={} actual={}",
            root_definitions.len(),
            roots.len()
        );
    }

    for definition in root_definitions {
        let matching = roots
            .iter()
            .filter(|root| root.root_key == definition.key)
            .copied()
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            anyhow::bail!(
                "folder_invariant_failed reason=wrong_root_key_count root_key={} actual={}",
                definition.key,
                matching.len()
            );
        }
        let root = matching[0];
        if root.parent_id.is_some() {
            anyhow::bail!(
                "folder_invariant_failed reason=root_has_parent folder_id={} root_key={}",
                root.id,
                root.root_key
            );
        }
        let name_is_valid = root.name == definition.stored_name
            || root_name_aliases
                .iter()
                .any(|alias| alias.root_key == definition.key && alias.stored_name == root.name);
        if !name_is_valid {
            anyhow::bail!(
                "folder_invariant_failed reason=unsupported_root_name folder_id={} root_key={}",
                root.id,
                root.root_key
            );
        }
    }

    for root in roots {
        if root_definition(root_definitions, &root.root_key).is_none() {
            anyhow::bail!(
                "folder_invariant_failed reason=unknown_root_key folder_id={} root_key={}",
                root.id,
                root.root_key
            );
        }
    }
    Ok(())
}

fn validate_parent_edges(
    folders: &[FolderInvariantRow],
    by_id: &HashMap<i64, &FolderInvariantRow>,
    root_definitions: &[RootInvariantDefinition],
) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
    let mut children = HashMap::<i64, Vec<i64>>::new();
    for folder in folders {
        if folder.is_root == 1 {
            continue;
        }
        validate_canonical_folder_name(folder)?;

        let parent_id = folder.parent_id.ok_or_else(|| {
            anyhow::anyhow!(
                "folder_invariant_failed reason=non_root_without_parent folder_id={}",
                folder.id
            )
        })?;
        let parent = by_id.get(&parent_id).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "folder_invariant_failed reason=missing_parent folder_id={} parent_id={parent_id}",
                folder.id
            )
        })?;
        if parent.root_key != folder.root_key {
            anyhow::bail!(
                "folder_invariant_failed reason=cross_root_parent folder_id={} parent_id={parent_id}",
                folder.id
            );
        }
        if parent.is_root == 1 {
            let definition =
                root_definition(root_definitions, &parent.root_key).ok_or_else(|| {
                    anyhow::anyhow!(
                        "folder_invariant_failed reason=unknown_root_key folder_id={} root_key={}",
                        parent.id,
                        parent.root_key
                    )
                })?;
            if !definition.allows_folder_descendants {
                anyhow::bail!(
                    "folder_invariant_failed reason=root_disallows_folder_descendants \
                     folder_id={} root_key={}",
                    folder.id,
                    parent.root_key
                );
            }
            if child_name_conflicts_with_root_namespace(
                root_definitions,
                &parent.root_key,
                &folder.name,
            ) {
                anyhow::bail!(
                    "folder_invariant_failed reason=reserved_root_namespace folder_id={} \
                     root_key={}",
                    folder.id,
                    parent.root_key
                );
            }
        }
        children.entry(parent_id).or_default().push(folder.id);
    }
    Ok(children)
}

fn validate_reachability(
    folders: &[FolderInvariantRow],
    roots: &[&FolderInvariantRow],
    children: &HashMap<i64, Vec<i64>>,
) -> anyhow::Result<()> {
    let mut reachable = HashSet::with_capacity(folders.len());
    let mut pending = roots.iter().map(|root| root.id).collect::<VecDeque<_>>();
    while let Some(folder_id) = pending.pop_front() {
        if !reachable.insert(folder_id) {
            continue;
        }
        if let Some(child_ids) = children.get(&folder_id) {
            pending.extend(child_ids);
        }
    }

    if reachable.len() != folders.len() {
        let folder_id = folders
            .iter()
            .find(|folder| !reachable.contains(&folder.id))
            .map_or(0, |folder| folder.id);
        anyhow::bail!(
            "folder_invariant_failed reason=cyclic_or_detached_ancestry folder_id={folder_id}"
        );
    }
    Ok(())
}

fn root_definition<'a>(
    definitions: &'a [RootInvariantDefinition],
    root_key: &str,
) -> Option<&'a RootInvariantDefinition> {
    definitions
        .iter()
        .find(|definition| definition.key == root_key)
}

fn child_name_conflicts_with_root_namespace(
    definitions: &[RootInvariantDefinition],
    root_key: &str,
    name: &str,
) -> bool {
    definitions.iter().any(|definition| {
        definition.key != root_key
            && !definition.public_path_prefix.is_empty()
            && definition.public_path_prefix == name
    })
}

fn validate_canonical_folder_name(folder: &FolderInvariantRow) -> anyhow::Result<()> {
    let canonical = !folder.name.is_empty()
        && folder.name.trim() == folder.name
        && folder.name != "."
        && folder.name != ".."
        && !folder.name.contains('/')
        && !folder.name.contains('\\')
        && !folder
            .name
            .chars()
            .any(|character| character < ' ' || character == '\u{7f}');
    if !canonical {
        anyhow::bail!(
            "folder_invariant_failed reason=noncanonical_folder_name folder_id={}",
            folder.id
        );
    }
    Ok(())
}
