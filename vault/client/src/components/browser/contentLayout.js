import { CONTENTS_VIEW_MODES } from "../../lib/contentsView.js";

export function shouldAnimateLayoutChange(current, next) {
  return (
    current.mode !== next.mode ||
    (current.mode === CONTENTS_VIEW_MODES.ICONS &&
      next.mode === CONTENTS_VIEW_MODES.ICONS &&
      current.iconSize !== next.iconSize)
  );
}

export function visibleItemElements(list) {
  if (!list) {
    return [];
  }
  const listRect = list.getBoundingClientRect();
  return Array.from(list.querySelectorAll("[data-selection-key]")).filter((element) => {
    const rect = element.getBoundingClientRect();
    return rect.bottom >= listRect.top && rect.top <= listRect.bottom;
  });
}

export function captureLayoutSnapshot(list, animate) {
  const elements = visibleItemElements(list);
  const anchor = elements[0];
  return {
    anchorKey: anchor?.dataset.selectionKey || "",
    anchorTop: anchor?.getBoundingClientRect().top || 0,
    animate,
    rects: new Map(
      elements.map((element) => [element.dataset.selectionKey, element.getBoundingClientRect()])
    ),
  };
}

function elementForSelectionKey(list, key) {
  return Array.from(list?.querySelectorAll("[data-selection-key]") || []).find(
    (element) => element.dataset.selectionKey === key
  );
}

export function applyLayoutSnapshot(list, snapshot) {
  if (!list || !snapshot) {
    return;
  }
  const anchor = elementForSelectionKey(list, snapshot.anchorKey);
  if (anchor) {
    list.scrollTop += anchor.getBoundingClientRect().top - snapshot.anchorTop;
  }
  const reducedMotion = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
  if (!snapshot.animate || reducedMotion) {
    return;
  }
  visibleItemElements(list).forEach((element) => {
    const previous = snapshot.rects.get(element.dataset.selectionKey);
    if (!previous || !element.animate) {
      return;
    }
    const current = element.getBoundingClientRect();
    const deltaX = previous.left - current.left;
    const deltaY = previous.top - current.top;
    if (Math.abs(deltaX) < 1 && Math.abs(deltaY) < 1) {
      return;
    }
    element.animate(
      [
        { transform: `translate3d(${deltaX}px, ${deltaY}px, 0)` },
        { transform: "translate3d(0, 0, 0)" },
      ],
      { duration: 180, easing: "cubic-bezier(0.2, 0.8, 0.2, 1)" }
    );
  });
}
