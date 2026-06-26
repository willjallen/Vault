import { classNames } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { responseError } from "./http.js";

const h = React.createElement;
const { useCallback, useState } = React;

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

export function DebugPanel({ apiFetch, onDebugError }) {
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
      "Mock server timeout",
      "clock",
      () => runDebugRequest("Mock server timeout", "/api/admin/debug/timeout"),
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
