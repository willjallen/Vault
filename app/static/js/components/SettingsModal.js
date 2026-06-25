/* eslint-disable max-lines */
import { classNames } from "../lib/utils.js";
import { normalizeSiteSettings } from "../lib/siteSettings.js";
import { Icon } from "./common/Icon.js";

const h = React.createElement;
const { useCallback, useEffect, useMemo, useRef, useState } = React;

const personalSections = [
  { id: "personalization", label: "Personalization", detail: "Appearance, density, and focus" },
  { id: "files", label: "Files", detail: "Defaults for browsing and edits" },
];

const adminSection = { id: "admin", label: "Admin", detail: "Users, roles, and permissions" };
const debugSection = { id: "debug", label: "Debug", detail: "Development tools and test faults" };

function SectionButton({ active, section, onSelect }) {
  return h(
    "button",
    {
      className: classNames("settings-nav-item", active ? "active" : ""),
      type: "button",
      onClick: () => onSelect(section.id),
    },
    [
      h("span", { className: "settings-nav-dot", key: "dot" }),
      h("span", { className: "settings-nav-copy", key: "copy" }, [
        h("span", { className: "settings-nav-label", key: "label" }, section.label),
        h("span", { className: "settings-nav-detail", key: "detail" }, section.detail),
      ]),
    ]
  );
}

function ThemeSegmented({ value, onChange }) {
  const options = [
    { id: "system", label: "System" },
    { id: "light", label: "Light" },
    { id: "dark", label: "Dark" },
  ];
  return h(
    "div",
    { className: "settings-segmented theme-choice", role: "group", "aria-label": "Theme" },
    options.map((option) =>
      h(
        "button",
        {
          className: classNames(value === option.id ? "active" : ""),
          key: option.id,
          onClick: () => onChange(option.id),
          type: "button",
        },
        option.label
      )
    )
  );
}

function PaletteSegmented({ value, onChange }) {
  const options = [
    { id: "cozy", label: "Cozy" },
    { id: "winui", label: "WinUI" },
  ];
  return h(
    "div",
    { className: "settings-segmented palette-choice", role: "group", "aria-label": "Palette" },
    options.map((option) =>
      h(
        "button",
        {
          className: classNames(value === option.id ? "active" : ""),
          key: option.id,
          onClick: () => onChange(option.id),
          type: "button",
        },
        option.label
      )
    )
  );
}

function SettingsToggle({ checked, label, onChange }) {
  return h(
    "button",
    {
      "aria-label": label,
      "aria-pressed": checked,
      className: classNames("settings-toggle", checked ? "active" : ""),
      onClick: () => onChange(!checked),
      type: "button",
    },
    h("span", null)
  );
}

function SettingsRow({ title, copy, control }) {
  return h("div", { className: "settings-row" }, [
    h("div", { className: "settings-row-copy", key: "copy" }, [
      h("div", { className: "settings-row-title", key: "title" }, title),
      h("div", { className: "settings-row-note", key: "note" }, copy),
    ]),
    h("div", { className: "settings-row-control", key: "control" }, control),
  ]);
}

function DebugActionButton({ disabled, icon, label, onClick, tone = "" }) {
  return h(
    "button",
    {
      className: classNames("debug-action-button", tone),
      disabled,
      onClick,
      type: "button",
    },
    [h(Icon, { icon, key: "icon", size: 15 }), h("span", { key: "label" }, label)]
  );
}

async function responseError(res) {
  try {
    const body = await res.json();
    return body.detail || `Request failed (${res.status})`;
  } catch (_err) {
    return `Request failed (${res.status})`;
  }
}

function userDisplayName(user) {
  return user?.name || user?.email || user?.subject || `User ${user?.id || ""}`.trim();
}

function groupMembersLabel(group) {
  const count = group.members?.length || 0;
  if (count === 1) {
    return "1 member";
  }
  return `${count} members`;
}

function availableGroupsForUser(user, groups) {
  const assigned = new Set((user.groups || []).map((group) => group.id));
  return groups.filter((group) => !assigned.has(group.id));
}

function GroupRow({
  group,
  disabled,
  nameValue,
  onDelete,
  onNameChange,
  onNameCommit,
  onNameReset,
}) {
  return h("article", { className: "admin-group-row" }, [
    h("div", { className: "admin-group-name-wrap", key: "name" }, [
      h("input", {
        "aria-label": `Group name: ${group.name}`,
        className: "admin-group-name-input",
        disabled,
        key: "input",
        onBlur: () => onNameCommit(group),
        onChange: (evt) => onNameChange(group.id, evt.target.value),
        onKeyDown: (evt) => {
          if (evt.key === "Enter") {
            evt.preventDefault();
            evt.currentTarget.blur();
          }
          if (evt.key === "Escape") {
            onNameReset(group);
            evt.currentTarget.blur();
          }
        },
        value: nameValue ?? group.name,
      }),
    ]),
    h("span", { className: "admin-group-count", key: "count" }, groupMembersLabel(group)),
    h(
      "button",
      {
        "aria-label": `Delete ${group.name}`,
        className: "admin-delete-button",
        disabled,
        key: "delete",
        onClick: () => onDelete(group),
        type: "button",
      },
      "Delete"
    ),
  ]);
}

function DraftGroupRow({ disabled, groupName, inputRef, onCancel, onChange, onSubmit }) {
  return h(
    "form",
    {
      className: "admin-group-row admin-group-row-draft",
      onSubmit,
    },
    [
      h("div", { className: "admin-group-name-wrap", key: "name" }, [
        h("input", {
          "aria-label": "New group name",
          className: "admin-group-name-input",
          disabled,
          key: "input",
          onChange: (evt) => onChange(evt.target.value),
          onKeyDown: (evt) => {
            if (evt.key === "Escape") {
              evt.preventDefault();
              onCancel();
            }
          },
          placeholder: "Group name",
          ref: inputRef,
          type: "text",
          value: groupName,
        }),
      ]),
      h("span", { className: "admin-group-count muted", key: "count" }, "New group"),
      h("div", { className: "admin-row-actions", key: "actions" }, [
        h(
          "button",
          {
            className: "admin-save-button",
            disabled: disabled || !groupName.trim(),
            key: "save",
            type: "submit",
          },
          "Save"
        ),
        h(
          "button",
          {
            className: "admin-text-button",
            disabled,
            key: "cancel",
            onClick: onCancel,
            type: "button",
          },
          "Cancel"
        ),
      ]),
    ]
  );
}

function UserGroupPicker({ availableGroups, disabled, onSelect }) {
  if (!availableGroups.length) {
    return h(
      "div",
      { className: "admin-user-group-menu" },
      h("div", { className: "admin-user-group-menu-empty" }, "No groups available")
    );
  }
  return h(
    "div",
    { className: "admin-user-group-menu" },
    availableGroups.map((group) =>
      h(
        "button",
        {
          disabled,
          key: group.id,
          onClick: () => onSelect(group.id),
          type: "button",
        },
        group.name
      )
    )
  );
}

function UserRow({
  disabled,
  groups,
  openPicker,
  onAddGroup,
  onRemoveGroup,
  onToggleAdmin,
  onTogglePicker,
  user,
}) {
  const availableGroups = availableGroupsForUser(user, groups);
  return h("article", { className: "admin-user-row" }, [
    h("div", { className: "admin-user-identity", key: "identity" }, [
      h("strong", { key: "name" }, userDisplayName(user)),
      h("span", { key: "email" }, user.email || user.subject),
    ]),
    h(
      "div",
      { className: "admin-user-groups", key: "groups" },
      user.groups?.length
        ? user.groups.map((group) =>
            h("span", { className: "admin-chip removable", key: group.id }, [
              h("span", { key: "name" }, group.name),
              h(
                "button",
                {
                  "aria-label": `Remove ${userDisplayName(user)} from ${group.name}`,
                  disabled,
                  key: "remove",
                  onClick: () => onRemoveGroup(user.id, group.id),
                  type: "button",
                },
                "x"
              ),
            ])
          )
        : h("span", { className: "admin-empty-inline" }, "No groups")
    ),
    h("div", { className: "admin-user-controls", key: "controls" }, [
      h(
        "span",
        { className: classNames("admin-role", user.is_admin ? "admin" : "") },
        user.is_admin ? "Admin" : "User"
      ),
      h(
        "button",
        {
          className: "admin-role-button",
          disabled,
          onClick: () => onToggleAdmin(user),
          type: "button",
        },
        user.is_admin ? "Demote" : "Promote"
      ),
      h("div", { className: "admin-user-add-wrap" }, [
        h(
          "button",
          {
            "aria-expanded": openPicker,
            "aria-label": `Add ${userDisplayName(user)} to a group`,
            className: "admin-plus-button",
            disabled,
            onClick: () => onTogglePicker(user.id),
            type: "button",
          },
          "+"
        ),
        openPicker
          ? h(UserGroupPicker, {
              availableGroups,
              disabled,
              onSelect: (groupId) => onAddGroup(user.id, groupId),
            })
          : null,
      ]),
    ]),
  ]);
}

function AdminPanel({ apiFetch, currentUser, onSiteSettingsChange, siteSettings }) {
  const [directory, setDirectory] = useState({ users: [], groups: [] });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [draftGroupOpen, setDraftGroupOpen] = useState(false);
  const [draftGroupName, setDraftGroupName] = useState("");
  const [editingGroupNames, setEditingGroupNames] = useState({});
  const [openGroupPickerUserId, setOpenGroupPickerUserId] = useState(null);
  const [pendingAction, setPendingAction] = useState("");
  const draftGroupInput = useRef(null);

  const users = useMemo(() => directory.users || [], [directory.users]);
  const groups = useMemo(() => directory.groups || [], [directory.groups]);
  const settings = normalizeSiteSettings(directory.settings || siteSettings);
  const archivePermanentDeleteAdminOnly = settings.archivePermanentDeleteAdminOnly;

  const applyDirectory = useCallback(
    (payload) => {
      setDirectory(payload);
      if (payload?.settings) {
        onSiteSettingsChange?.(normalizeSiteSettings(payload.settings));
      }
    },
    [onSiteSettingsChange]
  );

  const loadDirectory = useCallback(async () => {
    if (!apiFetch) {
      return;
    }
    setLoading(true);
    setError("");
    try {
      const res = await apiFetch("/api/admin/directory");
      if (!res.ok) {
        throw new Error(await responseError(res));
      }
      applyDirectory(await res.json());
    } catch (err) {
      setError(err.message || "Could not load admin settings");
    } finally {
      setLoading(false);
    }
  }, [apiFetch, applyDirectory]);

  const commitAdminChange = useCallback(
    async (label, url, options = {}) => {
      if (!apiFetch) {
        return;
      }
      setPendingAction(label);
      setError("");
      try {
        const res = await apiFetch(url, {
          ...options,
          headers: {
            "Content-Type": "application/json",
            ...(options.headers || {}),
          },
        });
        if (!res.ok) {
          throw new Error(await responseError(res));
        }
        const nextDirectory = await res.json();
        applyDirectory(nextDirectory);
        return nextDirectory;
      } catch (err) {
        setError(err.message || "Admin change failed");
        return null;
      } finally {
        setPendingAction("");
      }
    },
    [apiFetch, applyDirectory]
  );

  useEffect(() => {
    loadDirectory();
  }, [loadDirectory]);

  useEffect(() => {
    const nextNames = {};
    groups.forEach((group) => {
      nextNames[group.id] = group.name;
    });
    setEditingGroupNames(nextNames);
  }, [groups]);

  useEffect(() => {
    if (!draftGroupOpen) {
      return;
    }
    const focusTimer = window.setTimeout(() => draftGroupInput.current?.focus(), 40);
    return () => window.clearTimeout(focusTimer);
  }, [draftGroupOpen]);

  const handleCreateDraftGroup = useCallback(
    async (evt) => {
      evt.preventDefault();
      const groupName = draftGroupName.trim();
      if (!groupName) {
        return;
      }
      const nextDirectory = await commitAdminChange("create-group", "/api/admin/groups", {
        method: "POST",
        body: JSON.stringify({ name: groupName }),
      });
      if (nextDirectory) {
        setDraftGroupName("");
        setDraftGroupOpen(false);
      }
    },
    [commitAdminChange, draftGroupName]
  );

  const handleToggleAdmin = useCallback(
    (user) => {
      commitAdminChange(`user-${user.id}-admin`, `/api/admin/users/${user.id}`, {
        method: "PATCH",
        body: JSON.stringify({ is_admin: !user.is_admin }),
      });
    },
    [commitAdminChange]
  );

  const handleDeleteGroup = useCallback(
    (group) => {
      commitAdminChange(`group-${group.id}-delete`, `/api/admin/groups/${group.id}`, {
        method: "DELETE",
      });
    },
    [commitAdminChange]
  );

  const handleGroupNameChange = useCallback((groupId, value) => {
    setEditingGroupNames((prev) => ({ ...prev, [groupId]: value }));
  }, []);

  const handleGroupNameReset = useCallback((group) => {
    setEditingGroupNames((prev) => ({ ...prev, [group.id]: group.name }));
  }, []);

  const handleGroupNameCommit = useCallback(
    (group) => {
      const nextName = (editingGroupNames[group.id] || "").trim();
      if (!nextName) {
        handleGroupNameReset(group);
        return;
      }
      if (nextName === group.name) {
        return;
      }
      commitAdminChange(`group-${group.id}-rename`, `/api/admin/groups/${group.id}`, {
        method: "PATCH",
        body: JSON.stringify({ name: nextName, description: group.description || "" }),
      });
    },
    [commitAdminChange, editingGroupNames, handleGroupNameReset]
  );

  const handleAddMember = useCallback(
    (groupId, userId) => {
      if (!userId) {
        return;
      }
      commitAdminChange(`group-${groupId}-add-${userId}`, `/api/admin/groups/${groupId}/members`, {
        method: "POST",
        body: JSON.stringify({ user_id: userId }),
      });
    },
    [commitAdminChange]
  );

  const handleRemoveMember = useCallback(
    (groupId, userId) => {
      commitAdminChange(
        `group-${groupId}-remove-${userId}`,
        `/api/admin/groups/${groupId}/members/${userId}`,
        { method: "DELETE" }
      );
    },
    [commitAdminChange]
  );

  const handleAddGroupToUser = useCallback(
    (userId, groupId) => {
      setOpenGroupPickerUserId(null);
      handleAddMember(groupId, userId);
    },
    [handleAddMember]
  );

  const handleToggleGroupPicker = useCallback((userId) => {
    setOpenGroupPickerUserId((current) => (current === userId ? null : userId));
  }, []);

  const handleArchivePermanentDeleteAdminOnlyChange = useCallback(
    (checked) => {
      commitAdminChange("settings-archive-delete-policy", "/api/admin/settings", {
        method: "PATCH",
        body: JSON.stringify({
          settings: {
            archivePermanentDeleteAdminOnly: checked,
          },
        }),
      });
    },
    [commitAdminChange]
  );

  return h("div", { className: "settings-panel" }, [
    h("div", { className: "settings-panel-head", key: "head" }, [
      h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Admin"),
      h("h2", { key: "title" }, "Identity"),
      h(
        "p",
        { className: "muted tiny", key: "copy" },
        currentUser?.name ? `Signed in as ${currentUser.name}.` : "Users and groups for this vault."
      ),
    ]),
    error ? h("div", { className: "admin-error", key: "error" }, error) : null,
    h("div", { className: "admin-settings-grid", key: "grid" }, [
      h("section", { className: "admin-card", key: "policy" }, [
        h("div", { className: "admin-card-head compact", key: "head" }, [
          h("div", null, [
            h("h3", { key: "title" }, "Archive Policy"),
            h(
              "p",
              { className: "muted tiny", key: "note" },
              "Controls destructive actions in Archive."
            ),
          ]),
        ]),
        SettingsRow({
          title: "Admin-only permanent delete",
          copy: archivePermanentDeleteAdminOnly
            ? "Only admins can delete archived files and folders forever."
            : "Users with write access can delete archived files and folders forever.",
          control: SettingsToggle({
            checked: archivePermanentDeleteAdminOnly,
            label: "Toggle admin-only permanent delete",
            onChange: handleArchivePermanentDeleteAdminOnlyChange,
          }),
        }),
      ]),
      h("section", { className: "admin-card", key: "groups" }, [
        h("div", { className: "admin-card-head", key: "head" }, [
          h("div", { key: "copy" }, [
            h("h3", { key: "title" }, "Groups"),
            h("p", { className: "muted tiny", key: "note" }, `${groups.length} configured`),
          ]),
          h(
            "button",
            {
              "aria-label": "Add group",
              className: "admin-plus-button large",
              disabled: Boolean(pendingAction) || draftGroupOpen,
              key: "add",
              onClick: () => setDraftGroupOpen(true),
              type: "button",
            },
            "+"
          ),
        ]),
        loading
          ? h("div", { className: "admin-empty", key: "loading" }, "Loading groups...")
          : h("div", { className: "admin-list", key: "list" }, [
              draftGroupOpen
                ? h(DraftGroupRow, {
                    disabled: Boolean(pendingAction),
                    groupName: draftGroupName,
                    inputRef: draftGroupInput,
                    key: "draft",
                    onCancel: () => {
                      setDraftGroupName("");
                      setDraftGroupOpen(false);
                    },
                    onChange: setDraftGroupName,
                    onSubmit: handleCreateDraftGroup,
                  })
                : null,
              groups.length
                ? groups.map((group) =>
                    h(GroupRow, {
                      disabled: Boolean(pendingAction),
                      group,
                      key: group.id,
                      nameValue: editingGroupNames[group.id],
                      onDelete: handleDeleteGroup,
                      onNameChange: handleGroupNameChange,
                      onNameCommit: handleGroupNameCommit,
                      onNameReset: handleGroupNameReset,
                    })
                  )
                : h("div", { className: "admin-empty", key: "empty" }, "No groups yet"),
            ]),
      ]),
      h("section", { className: "admin-card", key: "users" }, [
        h("div", { className: "admin-card-head compact", key: "head" }, [
          h("div", null, [
            h("h3", { key: "title" }, "Users"),
            h("p", { className: "muted tiny", key: "note" }, `${users.length} known identities`),
          ]),
        ]),
        loading
          ? h("div", { className: "admin-empty", key: "loading" }, "Loading users...")
          : users.length
            ? h(
                "div",
                { className: "admin-list", key: "list" },
                users.map((user) =>
                  h(UserRow, {
                    disabled: Boolean(pendingAction),
                    groups,
                    key: user.id,
                    onAddGroup: handleAddGroupToUser,
                    onRemoveGroup: handleRemoveMember,
                    onToggleAdmin: handleToggleAdmin,
                    onTogglePicker: handleToggleGroupPicker,
                    openPicker: openGroupPickerUserId === user.id,
                    user,
                  })
                )
              )
            : h("div", { className: "admin-empty", key: "empty" }, "No users yet"),
      ]),
    ]),
  ]);
}

function DebugPanel({ apiFetch, onDebugError }) {
  const [pendingAction, setPendingAction] = useState("");
  const [result, setResult] = useState(null);

  const completeAction = useCallback((label, payload) => {
    setResult({
      label,
      payload,
      timestamp: new Date().toLocaleTimeString(),
    });
  }, []);

  const runDebugRequest = useCallback(
    async (label, url, options = {}) => {
      if (!apiFetch || pendingAction) {
        return;
      }
      setPendingAction(label);
      try {
        const res = await apiFetch(url, {
          method: "POST",
          ...options,
          headers: {
            "Content-Type": "application/json",
            ...(options.headers || {}),
          },
        });
        if (!res.ok) {
          throw new Error(await responseError(res));
        }
        const payload = await res.json();
        completeAction(label, payload);
        if (payload.reload) {
          window.setTimeout(() => window.location.reload(), 450);
        }
      } catch (err) {
        onDebugError?.(err.message || `${label} failed`);
        completeAction(label, { error: err.message || `${label} failed` });
      } finally {
        setPendingAction("");
      }
    },
    [apiFetch, completeAction, onDebugError, pendingAction]
  );

  const showClientError = useCallback(() => {
    const message = `Debug client error ${new Date().toLocaleTimeString()}`;
    onDebugError?.(message);
    completeAction("Client error", { message });
  }, [completeAction, onDebugError]);

  const resetDatabase = useCallback(() => {
    if (!window.confirm("Reset the development database? This cannot be undone.")) {
      return;
    }
    runDebugRequest("Reset database", "/api/admin/debug/reset-database");
  }, [runDebugRequest]);

  const disabled = Boolean(pendingAction);
  const errorButtons = [
    ["Client error", "warning", showClientError],
    [
      "HTTP 400",
      "bug",
      () =>
        runDebugRequest("HTTP 400", "/api/admin/debug/error", {
          body: JSON.stringify({ kind: "bad-request" }),
        }),
    ],
    [
      "HTTP 403",
      "shield",
      () =>
        runDebugRequest("HTTP 403", "/api/admin/debug/error", {
          body: JSON.stringify({ kind: "forbidden" }),
        }),
    ],
    [
      "HTTP 404",
      "search",
      () =>
        runDebugRequest("HTTP 404", "/api/admin/debug/error", {
          body: JSON.stringify({ kind: "not-found" }),
        }),
    ],
    [
      "HTTP 500",
      "server",
      () =>
        runDebugRequest("HTTP 500", "/api/admin/debug/error", {
          body: JSON.stringify({ kind: "server" }),
        }),
    ],
    [
      "HTTP 503",
      "server",
      () =>
        runDebugRequest("HTTP 503", "/api/admin/debug/error", {
          body: JSON.stringify({ kind: "unavailable" }),
        }),
    ],
  ];
  const utilityButtons = [
    [
      "Seed sample file",
      "seedling",
      () => runDebugRequest("Seed sample file", "/api/admin/debug/seed"),
    ],
    [
      "Emit refresh",
      "refresh",
      () =>
        runDebugRequest("Emit refresh", "/api/admin/debug/emit-state", {
          body: JSON.stringify({ resources: ["contents", "sidebar", "my_edits", "settings"] }),
        }),
    ],
    [
      "Run TTL sweep",
      "clock",
      () => runDebugRequest("Run TTL sweep", "/api/admin/debug/sweep-ttl"),
    ],
    [
      "Storage report",
      "database",
      () => runDebugRequest("Storage report", "/api/admin/debug/storage-report"),
    ],
  ];

  return h("div", { className: "settings-panel" }, [
    h("div", { className: "settings-panel-head", key: "head" }, [
      h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Debug"),
      h("h2", { key: "title" }, "Development"),
      h(
        "p",
        { className: "muted tiny", key: "copy" },
        pendingAction
          ? `${pendingAction} is running.`
          : "Development-only fault and maintenance tools."
      ),
    ]),
    h("div", { className: "debug-panel-grid", key: "grid" }, [
      h("section", { className: "admin-card debug-card", key: "errors" }, [
        h("div", { className: "admin-card-head compact", key: "head" }, [
          h("div", null, [
            h("h3", { key: "title" }, "Error Tests"),
            h("p", { className: "muted tiny", key: "note" }, "Exercise client and API failures."),
          ]),
        ]),
        h(
          "div",
          { className: "debug-action-grid", key: "actions" },
          errorButtons.map(([label, icon, onClick]) =>
            h(DebugActionButton, {
              disabled,
              icon,
              key: label,
              label,
              onClick,
            })
          )
        ),
      ]),
      h("section", { className: "admin-card debug-card", key: "tools" }, [
        h("div", { className: "admin-card-head compact", key: "head" }, [
          h("div", null, [
            h("h3", { key: "title" }, "Utilities"),
            h("p", { className: "muted tiny", key: "note" }, "Mutate or inspect this dev vault."),
          ]),
        ]),
        h(
          "div",
          { className: "debug-action-grid", key: "actions" },
          utilityButtons.map(([label, icon, onClick]) =>
            h(DebugActionButton, {
              disabled,
              icon,
              key: label,
              label,
              onClick,
            })
          )
        ),
      ]),
      h("section", { className: "admin-card debug-card danger", key: "danger" }, [
        h("div", { className: "admin-card-head compact", key: "head" }, [
          h("div", null, [
            h("h3", { key: "title" }, "Reset"),
            h("p", { className: "muted tiny", key: "note" }, "Destroy local database state."),
          ]),
        ]),
        h("div", { className: "debug-action-grid", key: "actions" }, [
          h(DebugActionButton, {
            disabled,
            icon: "database",
            key: "reset",
            label: "Reset database",
            onClick: resetDatabase,
            tone: "danger",
          }),
        ]),
      ]),
      h("section", { className: "admin-card debug-card output", key: "output" }, [
        h("div", { className: "admin-card-head compact", key: "head" }, [
          h("div", null, [
            h("h3", { key: "title" }, "Output"),
            h(
              "p",
              { className: "muted tiny", key: "note" },
              result ? `Last result at ${result.timestamp}.` : "Debug action responses appear here."
            ),
          ]),
        ]),
        h(
          "pre",
          { className: "debug-output", key: "result" },
          result ? JSON.stringify(result, null, 2) : "Run a debug action to inspect its response."
        ),
      ]),
    ]),
  ]);
}

function SectionPanel({
  alternateRows,
  activeSection,
  apiFetch,
  currentUser,
  doubleClickDownload,
  onDebugError,
  onAlternateRowsChange,
  onDoubleClickDownloadChange,
  onOpenFoldersOnClickChange,
  onPalettePreferenceChange,
  onSiteSettingsChange,
  onThemePreferenceChange,
  openFoldersOnClick,
  palettePreference,
  siteSettings,
  themePreference,
}) {
  if (activeSection === "admin") {
    return h(AdminPanel, { apiFetch, currentUser, onSiteSettingsChange, siteSettings });
  }

  if (activeSection === "debug") {
    return h(DebugPanel, { apiFetch, onDebugError });
  }

  if (activeSection === "files") {
    return h("div", { className: "settings-panel" }, [
      h("div", { className: "settings-panel-head", key: "head" }, [
        h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Files"),
        h("h2", { key: "title" }, "Browsing"),
        h("p", { className: "muted tiny", key: "copy" }, "File behavior defaults for the vault."),
      ]),
      h("div", { className: "settings-card", key: "card" }, [
        SettingsRow({
          title: "Open folders on click",
          copy: openFoldersOnClick
            ? "Left click opens folders. Ctrl or Shift click selects them."
            : "Left click selects folders. Double click or press Enter to open.",
          control: SettingsToggle({
            checked: openFoldersOnClick,
            label: "Toggle open folders on click",
            onChange: onOpenFoldersOnClickChange,
          }),
        }),
        SettingsRow({
          title: "Double click downloads files",
          copy: doubleClickDownload
            ? "Double clicking a file starts a download."
            : "Double clicking a file is disabled. Use the row download action instead.",
          control: SettingsToggle({
            checked: doubleClickDownload,
            label: "Toggle double click file download",
            onChange: onDoubleClickDownloadChange,
          }),
        }),
      ]),
    ]);
  }

  return h("div", { className: "settings-panel" }, [
    h("div", { className: "settings-panel-head", key: "head" }, [
      h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Personalization"),
      h("h2", { key: "title" }, "Appearance"),
      h(
        "p",
        { className: "muted tiny", key: "copy" },
        "Personal vault preferences and layout comfort."
      ),
    ]),
    h("div", { className: "settings-preview", key: "preview" }, [
      h("span", { className: "settings-preview-sidebar", key: "sidebar" }),
      h("span", { className: "settings-preview-main", key: "main" }),
      h("span", { className: "settings-preview-detail", key: "detail" }),
    ]),
    h("div", { className: "settings-card", key: "card" }, [
      SettingsRow({
        title: "Theme",
        copy:
          themePreference === "system"
            ? "Follows your operating system appearance."
            : "Overrides system appearance on this device.",
        control: ThemeSegmented({
          onChange: onThemePreferenceChange,
          value: themePreference || "system",
        }),
      }),
      SettingsRow({
        title: "Palette",
        copy:
          palettePreference === "winui"
            ? "Uses a WinUI-inspired neutral ramp and Windows blue accent."
            : "Uses the existing softer color palette.",
        control: PaletteSegmented({
          onChange: onPalettePreferenceChange,
          value: palettePreference || "cozy",
        }),
      }),
      SettingsRow({
        title: "Alternate row colors",
        copy: alternateRows
          ? "Contents rows use a subtle alternating background."
          : "Contents rows use flat dividers only.",
        control: SettingsToggle({
          checked: alternateRows,
          label: "Toggle alternate row colors",
          onChange: onAlternateRowsChange,
        }),
      }),
    ]),
  ]);
}

export function SettingsModal({
  apiFetch,
  appVersion = "0.0.0-dev",
  alternateRows = false,
  currentUser,
  devMode = false,
  doubleClickDownload = false,
  onAlternateRowsChange,
  onClose,
  onDebugError,
  onDoubleClickDownloadChange,
  onOpenFoldersOnClickChange,
  onSiteSettingsChange,
  onPalettePreferenceChange,
  onThemePreferenceChange,
  openFoldersOnClick = true,
  palettePreference = "cozy",
  siteName = "Vault",
  siteSettings = {},
  themePreference = "system",
}) {
  const sections = [
    ...personalSections,
    ...(currentUser?.is_admin ? [adminSection] : []),
    ...(currentUser?.is_admin && devMode ? [debugSection] : []),
  ];
  const [activeSection, setActiveSection] = useState(sections[0].id);
  const [phase, setPhase] = useState("entering");
  const closeButton = useRef(null);
  const closeTimer = useRef(null);
  const closing = useRef(false);

  const finish = useCallback(() => {
    if (closing.current) {
      return;
    }
    closing.current = true;
    setPhase("leaving");
    window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(onClose, 150);
  }, [onClose]);

  useEffect(() => {
    let firstFrame = null;
    let secondFrame = null;
    firstFrame = window.requestAnimationFrame(() => {
      secondFrame = window.requestAnimationFrame(() => setPhase("visible"));
    });
    const focusTimer = window.setTimeout(() => closeButton.current?.focus(), 160);

    function handleKeyDown(evt) {
      if (evt.key === "Escape") {
        finish();
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.clearTimeout(closeTimer.current);
      window.clearTimeout(focusTimer);
      if (firstFrame) {
        window.cancelAnimationFrame(firstFrame);
      }
      if (secondFrame) {
        window.cancelAnimationFrame(secondFrame);
      }
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [finish]);

  return h("div", { className: classNames("settings-layer", `phase-${phase}`) }, [
    h("button", {
      "aria-label": "Close settings",
      className: "settings-backdrop",
      key: "backdrop",
      type: "button",
      onClick: finish,
    }),
    h(
      "section",
      {
        "aria-labelledby": "settings-title",
        "aria-modal": "true",
        className: "settings-window",
        key: "window",
        role: "dialog",
      },
      [
        h("header", { className: "settings-titlebar", key: "titlebar" }, [
          h("div", { className: "settings-title-copy", key: "copy" }, [
            h("p", { className: "eyebrow tiny", key: "eyebrow" }, siteName),
            h("h1", { id: "settings-title", key: "title" }, "Settings"),
          ]),
          h(
            "button",
            {
              "aria-label": "Close settings",
              className: "settings-close",
              key: "close",
              ref: closeButton,
              type: "button",
              onClick: finish,
            },
            h(Icon, { icon: "close", size: 16 })
          ),
        ]),
        h("div", { className: "settings-body", key: "body" }, [
          h("nav", { "aria-label": "Settings sections", className: "settings-nav", key: "nav" }, [
            h(
              "div",
              { className: "settings-nav-list", key: "sections" },
              sections.map((section) =>
                h(SectionButton, {
                  active: activeSection === section.id,
                  key: section.id,
                  onSelect: setActiveSection,
                  section,
                })
              )
            ),
            h("div", { className: "settings-version", key: "version" }, [
              h("span", { key: "label" }, "Version"),
              h("strong", { key: "value" }, appVersion),
            ]),
          ]),
          h(SectionPanel, {
            alternateRows,
            activeSection,
            apiFetch,
            currentUser,
            doubleClickDownload,
            key: "panel",
            onDebugError,
            onAlternateRowsChange,
            onDoubleClickDownloadChange,
            onOpenFoldersOnClickChange,
            onPalettePreferenceChange,
            onSiteSettingsChange,
            onThemePreferenceChange,
            openFoldersOnClick,
            palettePreference,
            siteSettings,
            themePreference,
          }),
        ]),
      ]
    ),
  ]);
}
