import { classNames } from "../lib/utils.js";

const { useCallback, useEffect, useMemo, useRef, useState } = React;
const h = React.createElement;

const colorOptions = [
  { id: "", label: "None" },
  { id: "blue", label: "Blue" },
  { id: "teal", label: "Teal" },
  { id: "green", label: "Green" },
  { id: "amber", label: "Amber" },
  { id: "rose", label: "Rose" },
  { id: "violet", label: "Violet" },
  { id: "slate", label: "Slate" },
];

const iconOptions = [
  { id: "", label: "Default", glyph: "📁" },
  { id: "folder", label: "Folder", glyph: "📁" },
  { id: "home", label: "Home", glyph: "🏠" },
  { id: "project", label: "Project", glyph: "📌" },
  { id: "photos", label: "Photos", glyph: "🖼️" },
  { id: "finance", label: "Finance", glyph: "💼" },
  { id: "locked", label: "Private", glyph: "🔒" },
  { id: "archive", label: "Archive", glyph: "🗄️" },
];

function formatTimestamp(timestamp) {
  if (!timestamp) {
    return "Unknown";
  }
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) {
    return timestamp;
  }
  return date.toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

async function responseError(res) {
  try {
    const body = await res.json();
    return body.detail || `Request failed (${res.status})`;
  } catch (_err) {
    return `Request failed (${res.status})`;
  }
}

function PermissionCheck({ checked, disabled, label, onChange }) {
  return h("label", { className: "folder-permission-check" }, [
    h("input", {
      checked,
      disabled,
      onChange: (evt) => onChange(evt.target.checked),
      type: "checkbox",
    }),
    h("span", null, label),
  ]);
}

function permissionKey(permission) {
  return String(permission.group_id);
}

function sortPermissions(items) {
  return [...items].sort((a, b) => a.group_name.localeCompare(b.group_name));
}

export function FolderPropertiesModal({ apiFetch, folder, onClose, onUpdated }) {
  const [phase, setPhase] = useState("entering");
  const [detail, setDetail] = useState(null);
  const [color, setColor] = useState("");
  const [icon, setIcon] = useState("");
  const [permissions, setPermissions] = useState([]);
  const [selectedGroupId, setSelectedGroupId] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState("");
  const [error, setError] = useState("");
  const closeTimer = useRef(null);
  const closeButton = useRef(null);

  const closeModal = useCallback(() => {
    setPhase("leaving");
    window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(onClose, 140);
  }, [onClose]);

  const load = useCallback(async () => {
    if (!folder?.path && folder?.path !== "") {
      return;
    }
    setLoading(true);
    setError("");
    try {
      const params = new URLSearchParams({ path: folder.path || "" });
      const res = await apiFetch(`/api/folders/properties?${params.toString()}`);
      if (!res.ok) {
        throw new Error(await responseError(res));
      }
      const data = await res.json();
      setDetail(data);
      setColor(data.color || "");
      setIcon(data.icon || "");
      setPermissions(sortPermissions(data.permissions || []));
    } catch (err) {
      setError(err.message || "Could not load folder properties");
    } finally {
      setLoading(false);
    }
  }, [apiFetch, folder]);

  useEffect(() => {
    let frame = null;
    frame = window.requestAnimationFrame(() => setPhase("visible"));
    const focusTimer = window.setTimeout(() => closeButton.current?.focus(), 120);
    function handleKeyDown(evt) {
      if (evt.key === "Escape") {
        closeModal();
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    load();
    return () => {
      window.clearTimeout(closeTimer.current);
      window.clearTimeout(focusTimer);
      if (frame) {
        window.cancelAnimationFrame(frame);
      }
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [closeModal, load]);

  const availableGroups = useMemo(() => {
    const assigned = new Set(permissions.map((item) => item.group_id));
    return (detail?.available_groups || []).filter((group) => !assigned.has(group.id));
  }, [detail?.available_groups, permissions]);

  const updatePermission = useCallback((groupId, key, value) => {
    setPermissions((current) =>
      sortPermissions(
        current.map((permission) =>
          permission.group_id === groupId ? { ...permission, [key]: value } : permission
        )
      )
    );
  }, []);

  const removePermission = useCallback((groupId) => {
    setPermissions((current) => current.filter((permission) => permission.group_id !== groupId));
  }, []);

  const addPermission = useCallback(() => {
    const groupId = Number(selectedGroupId);
    const group = availableGroups.find((item) => item.id === groupId);
    if (!group) {
      return;
    }
    setPermissions((current) =>
      sortPermissions([
        ...current,
        {
          group_id: group.id,
          group_name: group.name,
          can_view: true,
          can_read: true,
          can_write: false,
        },
      ])
    );
    setSelectedGroupId("");
  }, [availableGroups, selectedGroupId]);

  const saveAppearance = useCallback(async () => {
    setSaving("appearance");
    setError("");
    try {
      const res = await apiFetch("/api/folders/properties", {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path: detail.path, color, icon }),
      });
      if (!res.ok) {
        throw new Error(await responseError(res));
      }
      const data = await res.json();
      setDetail(data);
      setColor(data.color || "");
      setIcon(data.icon || "");
      setPermissions(sortPermissions(data.permissions || []));
      onUpdated?.();
    } catch (err) {
      setError(err.message || "Could not save folder appearance");
    } finally {
      setSaving("");
    }
  }, [apiFetch, color, detail?.path, icon, onUpdated]);

  const savePermissions = useCallback(async () => {
    setSaving("permissions");
    setError("");
    try {
      const res = await apiFetch("/api/folders/permissions", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          path: detail.path,
          permissions: permissions.map((permission) => ({
            group_id: permission.group_id,
            can_view: permission.can_view,
            can_read: permission.can_read,
            can_write: permission.can_write,
          })),
        }),
      });
      if (!res.ok) {
        throw new Error(await responseError(res));
      }
      const data = await res.json();
      setDetail(data);
      setPermissions(sortPermissions(data.permissions || []));
      onUpdated?.();
    } catch (err) {
      setError(err.message || "Could not save folder permissions");
    } finally {
      setSaving("");
    }
  }, [apiFetch, detail?.path, onUpdated, permissions]);

  const title = detail?.name || folder?.name || "Folder";

  return h("div", { className: classNames("folder-properties-layer", `phase-${phase}`) }, [
    h("button", {
      "aria-label": "Close folder properties",
      className: "folder-properties-backdrop",
      key: "backdrop",
      onClick: closeModal,
      type: "button",
    }),
    h(
      "section",
      {
        "aria-labelledby": "folder-properties-title",
        "aria-modal": "true",
        className: "folder-properties-window",
        key: "window",
        role: "dialog",
      },
      [
        h("header", { className: "folder-properties-head", key: "head" }, [
          h("div", { className: "folder-properties-title", key: "title" }, [
            h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Folder Properties"),
            h("h2", { id: "folder-properties-title", key: "title" }, title),
            h(
              "p",
              { className: "muted tiny", key: "path" },
              detail?.path || folder?.path || "Vault"
            ),
          ]),
          h(
            "button",
            {
              "aria-label": "Close",
              className: "settings-close",
              key: "close",
              onClick: closeModal,
              ref: closeButton,
              type: "button",
            },
            "x"
          ),
        ]),
        loading
          ? h("div", { className: "folder-properties-loading", key: "loading" }, "Loading...")
          : h("div", { className: "folder-properties-body", key: "body" }, [
              error
                ? h("div", { className: "folder-properties-error", key: "error" }, error)
                : null,
              h("section", { className: "folder-properties-card", key: "details" }, [
                h("h3", null, "Details"),
                h("div", { className: "folder-detail-grid" }, [
                  h("div", null, [
                    h("span", null, "Created by"),
                    h("strong", null, detail.created_by_name),
                  ]),
                  h("div", null, [
                    h("span", null, "Created"),
                    h("strong", null, formatTimestamp(detail.created_at)),
                  ]),
                  h("div", null, [
                    h("span", null, "Contents"),
                    h(
                      "strong",
                      null,
                      `${detail.counts.documents} files, ${detail.counts.folders} folders`
                    ),
                  ]),
                  h("div", null, [
                    h("span", null, "Size"),
                    h("strong", null, detail.size_display || "0 B"),
                  ]),
                ]),
              ]),
              h("section", { className: "folder-properties-card", key: "appearance" }, [
                h("div", { className: "folder-card-headline" }, [
                  h("h3", null, "Appearance"),
                  h(
                    "button",
                    {
                      className: "confirm-toast-button primary",
                      disabled: Boolean(saving),
                      onClick: saveAppearance,
                      type: "button",
                    },
                    saving === "appearance" ? "Saving..." : "Save"
                  ),
                ]),
                h(
                  "div",
                  { className: "folder-color-grid" },
                  colorOptions.map((option) =>
                    h(
                      "button",
                      {
                        className: classNames(
                          "folder-color-choice",
                          option.id,
                          color === option.id ? "active" : ""
                        ),
                        key: option.id || "none",
                        onClick: () => setColor(option.id),
                        type: "button",
                      },
                      option.label
                    )
                  )
                ),
                h(
                  "div",
                  { className: "folder-icon-grid" },
                  iconOptions.map((option) =>
                    h(
                      "button",
                      {
                        className: classNames(
                          "folder-icon-choice",
                          icon === option.id ? "active" : ""
                        ),
                        key: option.id || "default",
                        onClick: () => setIcon(option.id),
                        type: "button",
                      },
                      [
                        h("span", { key: "glyph" }, option.glyph),
                        h("span", { key: "label" }, option.label),
                      ]
                    )
                  )
                ),
              ]),
              h("section", { className: "folder-properties-card", key: "permissions" }, [
                h("div", { className: "folder-card-headline" }, [
                  h("h3", null, "Permissions"),
                  h(
                    "button",
                    {
                      className: "confirm-toast-button primary",
                      disabled: Boolean(saving),
                      onClick: savePermissions,
                      type: "button",
                    },
                    saving === "permissions" ? "Saving..." : "Save"
                  ),
                ]),
                h("div", { className: "folder-permission-add" }, [
                  h(
                    "select",
                    {
                      onChange: (evt) => setSelectedGroupId(evt.target.value),
                      value: selectedGroupId,
                    },
                    [
                      h("option", { key: "placeholder", value: "" }, "Add group"),
                      ...availableGroups.map((group) =>
                        h("option", { key: group.id, value: group.id }, group.name)
                      ),
                    ]
                  ),
                  h(
                    "button",
                    {
                      className: "confirm-toast-button",
                      disabled: !selectedGroupId,
                      onClick: addPermission,
                      type: "button",
                    },
                    "Add"
                  ),
                ]),
                permissions.length
                  ? h(
                      "div",
                      { className: "folder-permission-list" },
                      permissions.map((permission) =>
                        h(
                          "article",
                          { className: "folder-permission-row", key: permissionKey(permission) },
                          [
                            h("strong", { key: "name" }, permission.group_name),
                            h(PermissionCheck, {
                              checked: permission.can_view,
                              disabled: Boolean(saving),
                              key: "view",
                              label: "View",
                              onChange: (value) =>
                                updatePermission(permission.group_id, "can_view", value),
                            }),
                            h(PermissionCheck, {
                              checked: permission.can_read,
                              disabled: Boolean(saving),
                              key: "read",
                              label: "Read",
                              onChange: (value) =>
                                updatePermission(permission.group_id, "can_read", value),
                            }),
                            h(PermissionCheck, {
                              checked: permission.can_write,
                              disabled: Boolean(saving),
                              key: "write",
                              label: "Write",
                              onChange: (value) =>
                                updatePermission(permission.group_id, "can_write", value),
                            }),
                            h(
                              "button",
                              {
                                className: "folder-permission-remove",
                                disabled: Boolean(saving),
                                key: "remove",
                                onClick: () => removePermission(permission.group_id),
                                type: "button",
                              },
                              "Remove"
                            ),
                          ]
                        )
                      )
                    )
                  : h("p", { className: "muted tiny" }, "No folder-specific group permissions."),
              ]),
              h("section", { className: "folder-properties-card", key: "history" }, [
                h("h3", null, "History"),
                detail.history?.length
                  ? h(
                      "ul",
                      { className: "folder-history-list" },
                      detail.history.map((historyEvent) =>
                        h("li", { key: historyEvent.id }, [
                          h("strong", null, historyEvent.message),
                          h(
                            "span",
                            null,
                            `${historyEvent.by} · ${formatTimestamp(historyEvent.timestamp)}`
                          ),
                        ])
                      )
                    )
                  : h("p", { className: "muted tiny" }, "No folder history yet."),
              ]),
            ]),
      ]
    ),
  ]);
}
