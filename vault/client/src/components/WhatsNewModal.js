import { releaseNoteVisual, releaseNotesSince } from "../lib/whatsNew.js";
import { classNames } from "../lib/utils.js";
import { Icon } from "./common/Icon.js";

const h = React.createElement;
const { useCallback, useEffect, useRef, useState } = React;

export function WhatsNew({ acknowledgedVersion, currentVersion, onAcknowledge, releaseNotes }) {
  const sections = releaseNotesSince(releaseNotes, currentVersion, acknowledgedVersion);
  return sections.length ? h(WhatsNewModal, { currentVersion, onAcknowledge, sections }) : null;
}

function WhatsNewModal({ currentVersion, onAcknowledge, sections }) {
  const [phase, setPhase] = useState("entering");
  const bodyRef = useRef(null);
  const okayButtonRef = useRef(null);
  const finishTimerRef = useRef(null);
  const finishingRef = useRef(false);

  const finish = useCallback(() => {
    if (finishingRef.current) {
      return;
    }
    finishingRef.current = true;
    setPhase("leaving");
    finishTimerRef.current = window.setTimeout(() => onAcknowledge(currentVersion), 140);
  }, [currentVersion, onAcknowledge]);

  useEffect(() => {
    const previousFocus = document.activeElement;
    let firstFrame = window.requestAnimationFrame(() => {
      firstFrame = window.requestAnimationFrame(() => setPhase("visible"));
    });
    const focusTimer = window.setTimeout(() => okayButtonRef.current?.focus(), 180);

    function handleKeyDown(evt) {
      if (evt.key === "Tab") {
        evt.preventDefault();
        const target = document.activeElement === okayButtonRef.current ? bodyRef : okayButtonRef;
        target.current?.focus();
      } else if (evt.key === "Escape") {
        evt.preventDefault();
        evt.stopPropagation();
      }
    }

    document.addEventListener("keydown", handleKeyDown, true);
    return () => {
      window.cancelAnimationFrame(firstFrame);
      window.clearTimeout(focusTimer);
      window.clearTimeout(finishTimerRef.current);
      document.removeEventListener("keydown", handleKeyDown, true);
      if (previousFocus instanceof HTMLElement) {
        previousFocus.focus();
      }
    };
  }, []);

  const showVersionHeadings = sections.length > 1;
  return h("div", { className: classNames("whats-new-layer", `phase-${phase}`) }, [
    h("div", { "aria-hidden": true, className: "whats-new-backdrop", key: "backdrop" }),
    h(
      "section",
      {
        "aria-labelledby": "whats-new-title",
        "aria-modal": "true",
        className: "whats-new-window",
        key: "window",
        role: "dialog",
      },
      [
        h("header", { className: "whats-new-head", key: "head" }, [
          h(
            "div",
            { "aria-hidden": true, className: "whats-new-mark", key: "mark" },
            h(Icon, { icon: "rocket", size: 25 })
          ),
          h("div", { className: "whats-new-heading", key: "heading" }, [
            h("span", { className: "whats-new-version", key: "version" }, `v${currentVersion}`),
            h("h1", { id: "whats-new-title", key: "title" }, "What's new"),
          ]),
        ]),
        h(
          "div",
          {
            "aria-label": "Release notes",
            className: "whats-new-body",
            key: "body",
            ref: bodyRef,
            role: "region",
            tabIndex: 0,
          },
          sections.map((section) =>
            h("section", { className: "whats-new-release", key: section.version }, [
              showVersionHeadings
                ? h(
                    "h2",
                    { className: "whats-new-release-version", key: "version" },
                    `Version ${section.version}`
                  )
                : null,
              h(
                "ul",
                { className: "whats-new-list", key: "entries" },
                section.entries.map((entry, index) => {
                  const visual = releaseNoteVisual(entry.kind);
                  return h(
                    "li",
                    {
                      className: classNames("whats-new-entry", `tone-${visual.tone}`),
                      key: `${entry.kind}:${index}`,
                    },
                    [
                      h(
                        "span",
                        { "aria-hidden": true, className: "whats-new-entry-icon", key: "icon" },
                        h(Icon, { icon: visual.icon, size: 14 })
                      ),
                      h("span", { className: "whats-new-entry-text", key: "text" }, entry.text),
                    ]
                  );
                })
              ),
            ])
          )
        ),
        h("footer", { className: "whats-new-actions", key: "actions" }, [
          h(
            "button",
            {
              className: "whats-new-okay",
              disabled: phase === "leaving",
              key: "okay",
              onClick: finish,
              ref: okayButtonRef,
              type: "button",
            },
            [h(Icon, { icon: "check", key: "icon", size: 14 }), h("span", { key: "label" }, "Okay")]
          ),
        ]),
      ]
    ),
  ]);
}
