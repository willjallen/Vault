#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootFolderDefinition {
    pub key: &'static str,
    pub stored_name: &'static str,
    pub public_label: &'static str,
    pub public_path_prefix: &'static str,
    pub allows_folder_descendants: bool,
}

pub const VAULT_ROOT_KEY: &str = "vault";
pub const ARCHIVE_ROOT_KEY: &str = "archive";
pub const VAULT_PUBLIC_NAME: &str = "Vault";
pub const ARCHIVE_PUBLIC_NAME: &str = "Archive";

pub const VAULT_ROOT: RootFolderDefinition = RootFolderDefinition {
    key: VAULT_ROOT_KEY,
    stored_name: "",
    public_label: VAULT_PUBLIC_NAME,
    public_path_prefix: "",
    allows_folder_descendants: true,
};

pub const ARCHIVE_ROOT: RootFolderDefinition = RootFolderDefinition {
    key: ARCHIVE_ROOT_KEY,
    stored_name: "Archive",
    public_label: ARCHIVE_PUBLIC_NAME,
    public_path_prefix: ARCHIVE_PUBLIC_NAME,
    allows_folder_descendants: false,
};

pub const ROOT_FOLDERS: [RootFolderDefinition; 2] = [VAULT_ROOT, ARCHIVE_ROOT];

#[must_use]
pub fn definition(root_key: &str) -> Option<RootFolderDefinition> {
    ROOT_FOLDERS
        .into_iter()
        .find(|definition| definition.key == root_key)
}

#[must_use]
pub fn child_name_conflicts_with_root_namespace(root_key: &str, name: &str) -> bool {
    ROOT_FOLDERS.iter().any(|definition| {
        definition.key != root_key
            && !definition.public_path_prefix.is_empty()
            && definition.public_path_prefix == name
    })
}
