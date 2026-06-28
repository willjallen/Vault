import { classNames } from "../lib/utils.js";

const h = React.createElement;
const { useEffect, useRef, useState } = React;

function pluralize(count, singular, plural = `${singular}s`) {
  return `${count} ${count === 1 ? singular : plural}`;
}

export function DragPreview({ drag }) {
  const dragItems = drag?.items || null;
  const initialX = drag?.x || 0;
  const initialY = drag?.y || 0;
  const [position, setPosition] = useState(() => ({ x: initialX, y: initialY }));
  const latestPosition = useRef(position);
  const frameRef = useRef(0);

  useEffect(() => {
    if (!dragItems) {
      return;
    }
    const nextPosition = { x: initialX, y: initialY };
    latestPosition.current = nextPosition;
    setPosition(nextPosition);
  }, [dragItems, initialX, initialY]);

  useEffect(() => {
    if (!dragItems) {
      return undefined;
    }
    function updateDragPosition(evt) {
      latestPosition.current = { x: evt.clientX, y: evt.clientY };
      if (frameRef.current) {
        return;
      }
      frameRef.current = window.requestAnimationFrame(() => {
        frameRef.current = 0;
        setPosition(latestPosition.current);
      });
    }
    window.addEventListener("dragover", updateDragPosition, true);
    return () => {
      window.removeEventListener("dragover", updateDragPosition, true);
      if (frameRef.current) {
        window.cancelAnimationFrame(frameRef.current);
        frameRef.current = 0;
      }
    };
  }, [dragItems]);

  if (!drag || !drag.items || drag.items.length === 0) {
    return null;
  }
  const lead = drag.items[0];
  const files = drag.items.filter((item) => item.type === "document").length;
  const folders = drag.items.length - files;
  const details = [
    lead.name || "Selection",
    files ? pluralize(files, "file") : "",
    folders ? pluralize(folders, "folder") : "",
  ].filter(Boolean);

  return h(
    "div",
    {
      className: classNames(
        "drag-preview",
        drag.items.length === 1 ? "single" : "multiple",
        drag.phase || "visible"
      ),
      style: { left: position.x + 14, top: position.y + 14 },
    },
    [
      h("div", { className: "drag-preview-stack", key: "stack" }, [
        h("span", { className: "drag-preview-card one", key: "one" }),
        h("span", { className: "drag-preview-card two", key: "two" }),
        h("span", { className: "drag-preview-card three", key: "three" }),
      ]),
      h("div", { className: "drag-preview-copy", key: "copy" }, [
        h("strong", null, pluralize(drag.items.length, "item")),
        h("span", null, details.join(" · ")),
      ]),
    ]
  );
}
