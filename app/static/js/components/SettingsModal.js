import { classNames } from "../lib/utils.js";

const h = React.createElement;
const { useCallback, useEffect, useRef, useState } = React;

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

function SectionPanel({ activeSection, currentUser }) {
  if (activeSection === "admin") {
    return h("div", { className: "settings-panel" }, [
      h("div", { className: "settings-panel-head", key: "head" }, [
        h("p", { className: "eyebrow tiny", key: "eyebrow" }, "Admin"),
        h("h2", { key: "title" }, "Permissions"),
        h(
          "p",
          { className: "muted tiny", key: "copy" },
          "Role assignment and access controls for the vault."
        ),
      ]),
      h("div", { className: "settings-card permissions-card", key: "card" }, [
        SettingsRow({
          title: "Default role",
          copy: "New users start with view-only vault access.",
          control: SegmentedMock({ options: ["View", "Edit", "Admin"], active: "View" }),
        }),
        SettingsRow({
          title: "Require admin approval",
          copy: "Permission changes wait for an administrator before taking effect.",
          control: ToggleMock({ active: true }),
        }),
        SettingsRow({
          title: "Current session",
          copy: currentUser?.name || currentUser?.id || "Local Admin",
          control: h("span", { className: "settings-pill" }, "Admin"),
        }),
      ]),
    ]);
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

export function SettingsModal({ currentUser, onClose }) {
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
          h(SectionPanel, { activeSection, currentUser, key: "panel" }),
        ]),
      ]
    ),
  ]);
}
