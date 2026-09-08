import { filePreview } from "../../lib/filePreview.js";
import { Icon } from "../common/Icon.js";

const { useEffect, useRef, useState } = React;
const h = React.createElement;

export function PreviewContent({ preview, fileName, onRetry }) {
  const [loadState, setLoadState] = useState("loading");
  const mediaRef = useRef(null);
  const isImage = preview.kind === "image";

  useEffect(() => {
    const media = mediaRef.current;
    return () => {
      if (media) {
        media.pause();
        media.removeAttribute("src");
        media.load();
      }
    };
  }, []);

  if (loadState === "error") {
    return h("div", { className: "file-preview-message", role: "alert" }, [
      h(Icon, { icon: `file-${preview.kind}`, size: 36, key: "icon" }),
      h("h3", { key: "title" }, isImage ? "Image preview unavailable" : "Playback unavailable"),
      h(
        "p",
        { className: "muted", key: "message" },
        "The file could not be loaded or its format is not supported by this browser. Try again or download it to open on your device."
      ),
      h(
        "button",
        { className: "btn", key: "retry", onClick: onRetry, type: "button" },
        "Try again"
      ),
    ]);
  }

  const shared = {
    className: `file-preview-${preview.kind}`,
    key: "content",
    onError: () => setLoadState("error"),
    src: preview.url,
  };
  return h(React.Fragment, null, [
    loadState === "loading"
      ? h(
          "p",
          { className: "file-preview-loading muted", key: "loading", role: "status" },
          "Loading preview…"
        )
      : null,
    preview.kind === "audio"
      ? h(
          "div",
          { className: "file-preview-audio-art", key: "art", "aria-hidden": true },
          h(Icon, { icon: "file-audio", size: 72 })
        )
      : null,
    isImage
      ? h("img", { ...shared, alt: fileName, onLoad: () => setLoadState("ready") })
      : h(preview.kind, {
          ...shared,
          "aria-label": fileName,
          controls: true,
          onLoadedMetadata: () => setLoadState("ready"),
          playsInline: preview.kind === "video" ? true : undefined,
          preload: "metadata",
          ref: mediaRef,
        }),
  ]);
}

export function FilePreviewModal({ doc, onClose, onDownload }) {
  const dialogRef = useRef(null);
  const closeButtonRef = useRef(null);
  const backdropPressRef = useRef(false);
  const [attempt, setAttempt] = useState(0);
  const preview = filePreview(doc);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) {
      return undefined;
    }
    const previousFocus = document.activeElement;
    const sourceRow = document.querySelector(
      `[data-selection-key="document:${CSS.escape(String(doc.id))}"]`
    );
    dialog.showModal();
    closeButtonRef.current?.focus();
    return () => {
      dialog.close();
      const focusTarget =
        previousFocus !== document.body && previousFocus?.isConnected ? previousFocus : sourceRow;
      if (focusTarget?.isConnected) {
        focusTarget.focus({ preventScroll: true });
      }
    };
  }, [doc.id]);

  if (!preview) {
    return null;
  }

  return h(
    "dialog",
    {
      "aria-labelledby": "file-preview-title",
      className: "file-preview-window",
      onCancel: (evt) => {
        evt.preventDefault();
        onClose();
      },
      onClick: (evt) => {
        if (evt.target === evt.currentTarget && backdropPressRef.current) {
          onClose();
        }
      },
      onKeyDown: (evt) => evt.stopPropagation(),
      onPointerDown: (evt) => {
        backdropPressRef.current = evt.target === evt.currentTarget;
      },
      ref: dialogRef,
    },
    [
      h("header", { className: "file-preview-head", key: "head" }, [
        h("div", { className: "file-preview-title", key: "title" }, [
          h("p", { className: "eyebrow tiny", key: "label" }, "Preview"),
          h("h2", { id: "file-preview-title", key: "name", title: doc.name }, doc.name),
          doc.size_display
            ? h("p", { className: "muted tiny", key: "size" }, doc.size_display)
            : null,
        ]),
        h("div", { className: "file-preview-actions", key: "actions" }, [
          h(
            "button",
            {
              className: "btn",
              key: "download",
              onClick: () => {
                onClose();
                onDownload(doc);
              },
              type: "button",
            },
            [h(Icon, { icon: "download", size: 15, key: "icon" }), "Download"]
          ),
          h(
            "button",
            {
              "aria-label": "Close preview",
              className: "settings-close",
              key: "close",
              onClick: onClose,
              ref: closeButtonRef,
              type: "button",
            },
            h(Icon, { icon: "close", size: 16 })
          ),
        ]),
      ]),
      h(
        "div",
        { className: `file-preview-body media-${preview.kind}`, key: "body" },
        h(PreviewContent, {
          key: `${preview.url}:${attempt}`,
          fileName: doc.name,
          onRetry: () => setAttempt((value) => value + 1),
          preview,
        })
      ),
    ]
  );
}
