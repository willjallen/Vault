import {
  contentsViewForFolder,
  normalizeContentsView,
  sameContentsView,
} from "../../lib/contentsView.js";
import {
  applyLayoutSnapshot,
  captureLayoutSnapshot,
  shouldAnimateLayoutChange,
} from "./contentLayout.js";

const { useCallback, useEffect, useLayoutEffect, useRef, useState } = React;

export function useContentsView({
  contentsViewByFolder,
  folder,
  interactionRef,
  listRef,
  locked,
  onContentsViewChange,
}) {
  const layoutFrameRef = useRef(null);
  const layoutSnapshotRef = useRef(null);
  const pendingViewRef = useRef(null);
  const pendingViewRenderRef = useRef(false);
  const pendingViewSaveRef = useRef(null);
  const viewSaveTimerRef = useRef(null);
  const viewCommitTimerRef = useRef(null);
  const folderRef = useRef(folder || "");
  const onContentsViewChangeRef = useRef(onContentsViewChange);
  const synchronizedFolderRef = useRef(folder || "");
  const savedContentsView = contentsViewForFolder(contentsViewByFolder, folder);
  const [contentsView, setContentsView] = useState(savedContentsView);
  const contentsViewRef = useRef(contentsView);
  const committedViewRef = useRef(contentsView);
  folderRef.current = folder || "";
  onContentsViewChangeRef.current = onContentsViewChange;

  const flushContentsViewSave = useCallback(() => {
    if (viewSaveTimerRef.current) {
      window.clearTimeout(viewSaveTimerRef.current);
      viewSaveTimerRef.current = null;
    }
    const pending = pendingViewSaveRef.current;
    pendingViewSaveRef.current = null;
    if (pending && onContentsViewChangeRef.current) {
      onContentsViewChangeRef.current(pending.folder, pending.view);
    }
  }, []);

  const scheduleContentsViewSave = useCallback(
    (next, options = {}) => {
      if (pendingViewSaveRef.current && pendingViewSaveRef.current.folder !== folderRef.current) {
        flushContentsViewSave();
      }
      pendingViewSaveRef.current = {
        folder: folderRef.current,
        view: normalizeContentsView(next),
      };
      if (options.commit) {
        flushContentsViewSave();
        return;
      }
      if (viewSaveTimerRef.current) {
        window.clearTimeout(viewSaveTimerRef.current);
      }
      viewSaveTimerRef.current = window.setTimeout(flushContentsViewSave, 220);
    },
    [flushContentsViewSave]
  );

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
          if (pendingViewSaveRef.current) {
            flushContentsViewSave();
          }
        }
        return;
      }
      scheduleContentsViewSave(next, options);
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
    [
      commitLiveContentsView,
      flushContentsViewSave,
      interactionRef,
      listRef,
      locked,
      scheduleContentsViewSave,
    ]
  );

  useLayoutEffect(() => {
    const next = contentsViewForFolder(contentsViewByFolder, folder);
    const folderChanged = synchronizedFolderRef.current !== (folder || "");
    const pending = pendingViewSaveRef.current;
    if (
      !folderChanged &&
      pending?.folder === (folder || "") &&
      sameContentsView(contentsViewRef.current, pending.view)
    ) {
      return;
    }
    if (!folderChanged && sameContentsView(contentsViewRef.current, next)) {
      return;
    }
    if (folderChanged) {
      if (layoutFrameRef.current) {
        window.cancelAnimationFrame(layoutFrameRef.current);
        layoutFrameRef.current = null;
      }
      if (viewCommitTimerRef.current) {
        window.clearTimeout(viewCommitTimerRef.current);
        viewCommitTimerRef.current = null;
      }
      pendingViewRef.current = null;
      pendingViewRenderRef.current = false;
      layoutSnapshotRef.current = null;
    }
    synchronizedFolderRef.current = folder || "";
    contentsViewRef.current = next;
    committedViewRef.current = next;
    setContentsView(next);
  }, [contentsViewByFolder, folder]);

  useLayoutEffect(() => {
    contentsViewRef.current = contentsView;
    committedViewRef.current = contentsView;
    listRef.current?.style.setProperty("--contents-icon-size", `${contentsView.iconSize}px`);
    const snapshot = layoutSnapshotRef.current;
    layoutSnapshotRef.current = null;
    applyLayoutSnapshot(listRef.current, snapshot);
  }, [contentsView, listRef]);

  useEffect(
    () => () => {
      if (layoutFrameRef.current) {
        window.cancelAnimationFrame(layoutFrameRef.current);
      }
      if (viewCommitTimerRef.current) {
        window.clearTimeout(viewCommitTimerRef.current);
      }
      if (viewSaveTimerRef.current) {
        window.clearTimeout(viewSaveTimerRef.current);
      }
    },
    []
  );

  return { contentsView, requestContentsView };
}
