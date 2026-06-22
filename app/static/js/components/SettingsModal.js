import { classNames } from "../lib/utils.js";

const h = React.createElement;
const { useCallback, useEffect, useMemo, useRef, useState } = React;

const personalSections = [
  { id: "personalization", label: "Personalization", detail: "Appearance, density, and focus" },
  { id: "files", label: "Files", detail: "Defaults for browsing and edits" },
  { id: "transfers", label: "Transfers", detail: "Uploads, downloads, and progress" },
  { id: "notifications", label: "Notifications", detail: "Activity and alerts" },
];

const adminSection = { id: "admin", label: "Admin", detail: "Users, roles, and permissions" };

function CloseIcon() {
  return h(
    "svg",
    {
      "aria-hidden": "true",
      fill: "none",
      height: 16,
      stroke: "currentColor",
      strokeLinecap: "round",
      strokeLinejoin: "round",
      strokeWidth: 2,
      viewBox: "0 0 24 24",
      width: 16,
    },
    [h("path", { d: "M18 6 6 18", key: "a" }), h("path", { d: "m6 6 12 12", key: "b" })]
  );
}

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

function ToggleMock({ active }) {
  return h(
    "span",
    { className: classNames("settings-toggle", active ? "active" : ""), "aria-hidden": true },
    h("span", null)
  );
}

function SegmentedMock({ options, active }) {
  return h(
    "div",
    { className: "settings-segmented", "aria-hidden": true },
    options.map((option) =>
      h("span", { className: classNames(option === active ? "active" : ""), key: option }, option)
    )
  );
}

function SliderMock({ value }) {
  return h(
    "div",
    { className: "settings-slider", "aria-hidden": true },
    h("span", { style: { width: `${value}%` } })
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
    return h("div", { className: "admin-user-group-menu empty" }, "No groups available");
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

function AdminPanel({ apiFetch, currentUser }) {
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
      setDirectory(await res.json());
    } catch (err) {
      setError(err.message || "Could not load admin settings");
    } finally {
      setLoading(false);
    }
  }, [apiFetch]);

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
        setDirectory(nextDirectory);
        return nextDirectory;
      } catch (err) {
        setError(err.message || "Admin change failed");
        return null;
      } finally {
        setPendingAction("");
      }
    },
    [apiFetch]
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

function SectionPanel({ activeSection, apiFetch, currentUser }) {
  if (activeSection === "admin") {
    return h(AdminPanel, { apiFetch, currentUser });
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
          copy: "Folder rows prioritize fast navigation from Browse.",
          control: ToggleMock({ active: true }),
        }),
        SettingsRow({
          title: "Details density",
          copy: "Controls how much metadata appears in each row.",
          control: SegmentedMock({ options: ["Quiet", "Standard", "Dense"], active: "Standard" }),
        }),
        SettingsRow({
          title: "Archive reminders",
          copy: "Surface extra confirmation before permanent deletion.",
          control: ToggleMock({ active: true }),
        }),
      ]),
    ]);
  }

  if (activeSection === "transfers") {
    return h("div", { className: "settings-panel" }, [
      h("div", { className: "settings-panel-head", key: "head" }, [
        h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Transfers"),
        h("h2", { key: "title" }, "Progress"),
        h("p", { className: "muted tiny", key: "copy" }, "Upload and download presentation."),
      ]),
      h("div", { className: "settings-card", key: "card" }, [
        SettingsRow({
          title: "Show transfer dock",
          copy: "Progress, speed, and ETA stay visible while files move.",
          control: ToggleMock({ active: true }),
        }),
        SettingsRow({
          title: "Completion hold",
          copy: "Keep finished transfers visible long enough to read.",
          control: SliderMock({ value: 64 }),
        }),
        SettingsRow({
          title: "Animation feel",
          copy: "Use quick spring motion for transfer status changes.",
          control: SegmentedMock({ options: ["Calm", "Spring", "Fast"], active: "Spring" }),
        }),
      ]),
    ]);
  }

  if (activeSection === "notifications") {
    return h("div", { className: "settings-panel" }, [
      h("div", { className: "settings-panel-head", key: "head" }, [
        h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Notifications"),
        h("h2", { key: "title" }, "Activity"),
        h(
          "p",
          { className: "muted tiny", key: "copy" },
          "How vault activity should announce itself."
        ),
      ]),
      h("div", { className: "settings-card", key: "card" }, [
        SettingsRow({
          title: "Lock changes",
          copy: "Show a small notice when a file is locked or released.",
          control: ToggleMock({ active: true }),
        }),
        SettingsRow({
          title: "Archive changes",
          copy: "Show movement into and out of Archive.",
          control: ToggleMock({ active: true }),
        }),
        SettingsRow({
          title: "Sound",
          copy: "Keep vault notifications silent.",
          control: ToggleMock({ active: false }),
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
        copy: "Follow the system appearance.",
        control: SegmentedMock({ options: ["Light", "Auto", "Dark"], active: "Auto" }),
      }),
      SettingsRow({
        title: "Sidebar labels",
        copy: "Keep folder names readable in the left pane.",
        control: ToggleMock({ active: true }),
      }),
      SettingsRow({
        title: "Motion",
        copy: "Use soft spring animations for overlays and progress.",
        control: SliderMock({ value: 72 }),
      }),
    ]),
  ]);
}

export function SettingsModal({ apiFetch, currentUser, onClose }) {
  const sections = currentUser?.is_admin ? [...personalSections, adminSection] : personalSections;
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
            h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Vault"),
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
            h(CloseIcon)
          ),
        ]),
        h("div", { className: "settings-body", key: "body" }, [
          h(
            "nav",
            { "aria-label": "Settings sections", className: "settings-nav", key: "nav" },
            sections.map((section) =>
              h(SectionButton, {
                active: activeSection === section.id,
                key: section.id,
                onSelect: setActiveSection,
                section,
              })
            )
          ),
          h(SectionPanel, { activeSection, apiFetch, currentUser, key: "panel" }),
        ]),
      ]
    ),
  ]);
}
