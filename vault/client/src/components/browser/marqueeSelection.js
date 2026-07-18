export function clientPointToContent({ clientX, clientY, listRect, scrollLeft, scrollTop }) {
  return {
    x: clientX - listRect.left + scrollLeft,
    y: clientY - listRect.top + scrollTop,
  };
}

export function clientRectToContent(rect, listRect, scrollLeft, scrollTop) {
  return {
    bottom: rect.bottom - listRect.top + scrollTop,
    left: rect.left - listRect.left + scrollLeft,
    right: rect.right - listRect.left + scrollLeft,
    top: rect.top - listRect.top + scrollTop,
  };
}

export function rectFromPoints(start, end) {
  return {
    bottom: Math.max(start.y, end.y),
    left: Math.min(start.x, end.x),
    right: Math.max(start.x, end.x),
    top: Math.min(start.y, end.y),
  };
}

export function contentRectToVisibleClientRect(rect, listRect, scrollLeft, scrollTop) {
  const visibleLeft = Math.max(rect.left - scrollLeft + listRect.left, listRect.left);
  const visibleRight = Math.min(rect.right - scrollLeft + listRect.left, listRect.right);
  const visibleTop = Math.max(rect.top - scrollTop + listRect.top, listRect.top);
  const visibleBottom = Math.min(rect.bottom - scrollTop + listRect.top, listRect.bottom);
  return {
    bottom: visibleBottom,
    height: Math.max(0, visibleBottom - visibleTop),
    left: visibleLeft,
    right: visibleRight,
    top: visibleTop,
    width: Math.max(0, visibleRight - visibleLeft),
  };
}

export function rectsIntersect(left, right) {
  return (
    left.left <= right.right &&
    left.right >= right.left &&
    left.top <= right.bottom &&
    left.bottom >= right.top
  );
}

export function combineMarqueeSelection({ baseSelection, hitKeys, mode, orderedKeys }) {
  if (mode === "replace") {
    return hitKeys;
  }
  const baseSet = new Set(baseSelection);
  const hitSet = new Set(hitKeys);
  return orderedKeys.filter((key) => {
    if (mode === "toggle") {
      return hitSet.has(key) ? !baseSet.has(key) : baseSet.has(key);
    }
    return baseSet.has(key) || hitSet.has(key);
  });
}
