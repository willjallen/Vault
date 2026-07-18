import {
  CONTENTS_VIEW_STORAGE_KEY,
  normalizeContentsView,
  sameContentsView,
} from "../../lib/contentsView.js";
import { readLocalPreference, writeLocalPreference } from "../../lib/localPreferences.js";
import {
  applyLayoutSnapshot,
  captureLayoutSnapshot,
  shouldAnimateLayoutChange,
} from "./contentLayout.js";

const { useCallback, useEffect, useLayoutEffect, useRef, useState } = React;

export function useContentsView({ interactionRef, listRef, locked }) {
  const layoutFrameRef = useRef(null);
  const layoutSnapshotRef = useRef(null);
  const pendingViewRef = useRef(null);
  const pendingViewRenderRef = useRef(false);
  const viewCommitTimerRef = useRef(null);
  const [contentsView, setContentsView] = useState(() =>
    normalizeContentsView(readLocalPreference(CONTENTS_VIEW_STORAGE_KEY))
  );
  const contentsViewRef = useRef(contentsView);
  const committedViewRef = useRef(contentsView);

  const commitLiveContentsView = useCallback(() => {
    if (viewCommitTimerRef.current) {
      window.clearTimeout(viewCommitTimerRef.current);
      viewCommitTimerRef.current = null;
    }
    const next = contentsViewRef.current;
    if (sameContentsView(committedViewRef.current, next)) {
      return;
    }
    committedViewRef.current = next;
    setContentsView(next);
  }, []);

  const requestContentsView = useCallback(
    (nextValue, options = {}) => {
      if (locked || interactionRef?.current) {
        return;
      }
      const next = normalizeContentsView(nextValue);
      const current = contentsViewRef.current;
      if (sameContentsView(current, next)) {
        if (options.commit) {
          commitLiveContentsView();
        }
        return;
      }
      const animateLayout = shouldAnimateLayoutChange(current, next);
      if (!layoutSnapshotRef.current) {
        layoutSnapshotRef.current = captureLayoutSnapshot(listRef.current, animateLayout);
      } else if (animateLayout) {
        layoutSnapshotRef.current.animate = true;
      }
      contentsViewRef.current = next;
      pendingViewRef.current = next;
      pendingViewRenderRef.current =
        pendingViewRenderRef.current ||
        !options.transient ||
        committedViewRef.current.mode !== next.mode;
      if (!layoutFrameRef.current) {
        layoutFrameRef.current = window.requestAnimationFrame(() => {
          layoutFrameRef.current = null;
          const pending = pendingViewRef.current;
          const shouldRender = pendingViewRenderRef.current;
          pendingViewRef.current = null;
          pendingViewRenderRef.current = false;
          if (!pending) {
            return;
          }
          if (shouldRender) {
            committedViewRef.current = pending;
            setContentsView(pending);
            return;
          }
          listRef.current?.style.setProperty("--contents-icon-size", `${pending.iconSize}px`);
          const snapshot = layoutSnapshotRef.current;
          layoutSnapshotRef.current = null;
          applyLayoutSnapshot(listRef.current, snapshot);
        });
      }
      if (viewCommitTimerRef.current) {
        window.clearTimeout(viewCommitTimerRef.current);
      }
      if (options.transient) {
        viewCommitTimerRef.current = window.setTimeout(() => {
          viewCommitTimerRef.current = null;
          commitLiveContentsView();
        }, 180);
      }
    },
    [commitLiveContentsView, interactionRef, listRef, locked]
  );

  useLayoutEffect(() => {
    contentsViewRef.current = contentsView;
    committedViewRef.current = contentsView;
    listRef.current?.style.setProperty("--contents-icon-size", `${contentsView.iconSize}px`);
    const snapshot = layoutSnapshotRef.current;
    layoutSnapshotRef.current = null;
    applyLayoutSnapshot(listRef.current, snapshot);
  }, [contentsView, listRef]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      writeLocalPreference(CONTENTS_VIEW_STORAGE_KEY, contentsView);
    }, 220);
    return () => window.clearTimeout(timer);
  }, [contentsView]);

  useEffect(
    () => () => {
      if (layoutFrameRef.current) {
        window.cancelAnimationFrame(layoutFrameRef.current);
      }
      if (viewCommitTimerRef.current) {
        window.clearTimeout(viewCommitTimerRef.current);
      }
    },
    []
  );

  return { contentsView, requestContentsView };
}
