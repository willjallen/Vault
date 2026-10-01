import { moveContentsColumn } from "../../lib/contentsColumns.js";

const { useEffect, useRef, useState } = React;
const DRAG_THRESHOLD = 6;

export function useColumnReorder({ columnOrder, folder, headerRef, locked, onColumnOrderChange }) {
  const dragRef = useRef(null);
  const suppressSortRef = useRef(false);
  const [dragState, setDragState] = useState(null);

  useEffect(() => {
    dragRef.current = null;
    setDragState(null);
  }, [folder, locked]);

  function start(key, evt) {
    suppressSortRef.current = false;
    if (locked || evt.button !== 0) {
      return;
    }
    evt.stopPropagation();
    dragRef.current = {
      active: false,
      folder,
      key,
      order: columnOrder,
      pointerId: evt.pointerId,
      startX: evt.clientX,
      startY: evt.clientY,
      target: null,
    };
    evt.currentTarget.setPointerCapture?.(evt.pointerId);
  }

  function move(evt) {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== evt.pointerId || drag.folder !== folder || locked) {
      return;
    }
    if (
      !drag.active &&
      Math.hypot(evt.clientX - drag.startX, evt.clientY - drag.startY) < DRAG_THRESHOLD
    ) {
      return;
    }
    drag.active = true;
    evt.preventDefault();
    evt.stopPropagation();
    const cell = document.elementFromPoint(evt.clientX, evt.clientY)?.closest("[data-column-key]");
    const key = cell?.dataset.columnKey;
    const validTarget = headerRef.current?.contains(cell) && columnOrder.includes(key);
    drag.target = validTarget
      ? {
          key,
          after:
            evt.clientX >=
            cell.getBoundingClientRect().left + cell.getBoundingClientRect().width / 2,
        }
      : null;
    setDragState({ key: drag.key, target: drag.target });
  }

  function finish(evt, canceled = false) {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== evt.pointerId) {
      return;
    }
    if (drag.active) {
      evt.preventDefault();
      evt.stopPropagation();
      suppressSortRef.current = true;
      if (!canceled && !locked && drag.folder === folder && drag.target) {
        const next = moveContentsColumn(drag.order, drag.key, drag.target.key, drag.target.after);
        if (next.some((key, index) => key !== drag.order.at(index))) {
          onColumnOrderChange?.(folder, next);
        }
      }
    }
    dragRef.current = null;
    setDragState(null);
    if (evt.currentTarget.hasPointerCapture?.(evt.pointerId)) {
      evt.currentTarget.releasePointerCapture(evt.pointerId);
    }
  }

  function keyDown(key, evt) {
    if (evt.key === "Escape" && dragRef.current) {
      evt.preventDefault();
      dragRef.current = null;
      setDragState(null);
      suppressSortRef.current = true;
      return;
    }
    if (locked || !evt.altKey || !["ArrowLeft", "ArrowRight"].includes(evt.key)) {
      return;
    }
    evt.preventDefault();
    evt.stopPropagation();
    const index = columnOrder.indexOf(key);
    const nextIndex = index + (evt.key === "ArrowRight" ? 1 : -1);
    if (nextIndex >= 0 && nextIndex < columnOrder.length) {
      onColumnOrderChange?.(
        folder,
        moveContentsColumn(columnOrder, key, columnOrder.at(nextIndex), nextIndex > index)
      );
    }
  }

  function suppressSort() {
    const suppressed = suppressSortRef.current;
    suppressSortRef.current = false;
    return suppressed;
  }

  return {
    dragState,
    reorderHandlers: {
      cancel: (evt) => finish(evt, true),
      end: finish,
      keyDown,
      move,
      start,
      suppressSort,
    },
  };
}
