import { classNames } from "../lib/utils.js";

const h = React.createElement;
const { useCallback, useEffect, useRef, useState } = React;

export function ConfirmToast({ request, onResolve }) {
  const [phase, setPhase] = useState("entering");
  const confirmButton = useRef(null);
  const resolving = useRef(false);
  const resolveTimer = useRef(null);

  const finish = useCallback(
    (confirmed) => {
      if (resolving.current) {
        return;
      }
      resolving.current = true;
      setPhase("leaving");
      window.clearTimeout(resolveTimer.current);
      resolveTimer.current = window.setTimeout(() => onResolve(confirmed), 150);
    },
    [onResolve]
  );

  useEffect(() => {
    if (!request) {
      return undefined;
    }
    resolving.current = false;
    setPhase("entering");
    let firstFrame = null;
    let secondFrame = null;

    function handleKeyDown(evt) {
      if (evt.key === "Escape") {
        finish(false);
      }
      if ((evt.metaKey || evt.ctrlKey) && evt.key === "Enter") {
        finish(true);
      }
    }

    firstFrame = window.requestAnimationFrame(() => {
      secondFrame = window.requestAnimationFrame(() => setPhase("visible"));
    });
    const timer = window.setTimeout(() => {
      if (confirmButton.current) {
        confirmButton.current.focus();
      }
    }, 160);
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.clearTimeout(resolveTimer.current);
      window.clearTimeout(timer);
      if (firstFrame) {
        window.cancelAnimationFrame(firstFrame);
      }
      if (secondFrame) {
        window.cancelAnimationFrame(secondFrame);
      }
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, [finish, request]);

  if (!request) {
    return null;
  }

  const tone = request.tone || "default";
  return h(
    "div",
    {
      className: classNames(
        "confirm-toast-layer",
        `phase-${phase}`,
        tone === "danger" ? "danger" : ""
      ),
    },
    [
      h("div", { className: "confirm-toast-backdrop", "aria-hidden": true, key: "backdrop" }),
      h(
        "div",
        {
          className: classNames("confirm-toast", tone === "danger" ? "danger" : ""),
          role: "alertdialog",
          "aria-labelledby": "confirm-toast-title",
          "aria-describedby": request.message ? "confirm-toast-message" : undefined,
          key: "toast",
        },
        [
          h("div", { className: "confirm-toast-content", key: "content" }, [
            h("div", { className: "confirm-toast-copy", key: "copy" }, [
              h(
                "div",
                { className: "confirm-toast-title", id: "confirm-toast-title", key: "title" },
                request.title
              ),
              request.message
                ? h(
                    "div",
                    {
                      className: "confirm-toast-message",
                      id: "confirm-toast-message",
                      key: "message",
                    },
                    request.message
                  )
                : null,
            ]),
            h("div", { className: "confirm-toast-actions", key: "actions" }, [
              h(
                "button",
                {
                  className: "confirm-toast-button cancel",
                  type: "button",
                  onClick: () => finish(false),
                  key: "cancel",
                },
                request.cancelLabel || "Cancel"
              ),
              h(
                "button",
                {
                  className: classNames(
                    "confirm-toast-button",
                    tone === "danger" ? "danger" : "primary"
                  ),
                  type: "button",
                  ref: confirmButton,
                  onClick: () => finish(true),
                  key: "confirm",
                },
                request.confirmLabel || "Confirm"
              ),
            ]),
          ]),
        ]
      ),
    ]
  );
}
