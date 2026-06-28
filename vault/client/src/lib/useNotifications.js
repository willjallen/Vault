const { useCallback, useEffect, useRef, useState } = React;

const DEFAULT_NOTICE_MS = 4200;
const NOTICE_EXIT_MS = 260;

function normalizeNotice(input, fallbackDuration) {
  const source = typeof input === "string" ? { title: input } : input || {};
  const detail = String(source.detail || "").trim();
  const title = String(source.title || "").trim();
  const duration =
    source.duration === null || source.duration === false
      ? null
      : Number.isFinite(source.duration) && source.duration > 0
        ? source.duration
        : fallbackDuration;

  return {
    detail,
    dismissible: source.dismissible !== false,
    duration,
    kind: source.kind || "info",
    progress: source.progress ?? true,
    title,
  };
}

export function useNotifications(defaultDuration = DEFAULT_NOTICE_MS) {
  const [notice, setNotice] = useState(null);
  const noticeId = useRef(1);
  const noticeTimers = useRef([]);

  const clearNoticeTimers = useCallback(() => {
    noticeTimers.current.forEach((timer) => window.clearTimeout(timer));
    noticeTimers.current = [];
  }, []);

  const clearNotice = useCallback(() => {
    clearNoticeTimers();
    setNotice(null);
  }, [clearNoticeTimers]);

  const dismissNotice = useCallback(
    (targetId) => {
      clearNoticeTimers();
      setNotice((current) => {
        if (!current || (targetId && current.id !== targetId)) {
          return current;
        }
        return { ...current, phase: "leaving" };
      });
      const removeTimer = window.setTimeout(() => {
        setNotice((current) =>
          !current || (targetId && current.id !== targetId) ? current : null
        );
      }, NOTICE_EXIT_MS);
      noticeTimers.current = [removeTimer];
    },
    [clearNoticeTimers]
  );

  const showNotice = useCallback(
    (input) => {
      clearNoticeTimers();
      const normalized = normalizeNotice(input, defaultDuration);
      if (!normalized.title && !normalized.detail) {
        setNotice(null);
        return;
      }

      const id = noticeId.current;
      noticeId.current += 1;
      setNotice({
        ...normalized,
        id,
        phase: "entering",
      });

      const enterTimer = window.setTimeout(() => {
        setNotice((current) =>
          current && current.id === id ? { ...current, phase: "visible" } : current
        );
      }, 16);
      const timers = [enterTimer];
      if (normalized.duration) {
        const leaveTimer = window.setTimeout(() => {
          setNotice((current) =>
            current && current.id === id ? { ...current, phase: "leaving" } : current
          );
        }, normalized.duration);
        const removeTimer = window.setTimeout(() => {
          setNotice((current) => (current && current.id === id ? null : current));
        }, normalized.duration + NOTICE_EXIT_MS);
        timers.push(leaveTimer, removeTimer);
      }
      noticeTimers.current = timers;
    },
    [clearNoticeTimers, defaultDuration]
  );

  const showError = useCallback(
    (message, duration = defaultDuration) => {
      const source = typeof message === "string" ? { detail: message } : message || {};
      const detail = String(source.detail || "").trim();
      const title = String(source.title || "").trim();
      if (!title && !detail) {
        clearNotice();
        return;
      }
      showNotice({
        detail,
        dismissible: source.dismissible,
        duration: source.duration ?? duration,
        kind: "error",
        progress: source.progress,
        title: title || "Error",
      });
    },
    [clearNotice, defaultDuration, showNotice]
  );

  useEffect(
    () => () => {
      clearNoticeTimers();
    },
    [clearNoticeTimers]
  );

  return {
    dismissNotice,
    notice,
    showError,
    showNotice,
  };
}
