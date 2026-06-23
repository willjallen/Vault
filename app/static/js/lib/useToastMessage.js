const { useCallback, useEffect, useRef, useState } = React;

export function useToastMessage() {
  const [toast, setToast] = useState("");
  const toastTimerRef = useRef(null);

  const showToast = useCallback((message, duration = 2200) => {
    if (toastTimerRef.current) {
      window.clearTimeout(toastTimerRef.current);
    }
    setToast(message);
    toastTimerRef.current = window.setTimeout(() => {
      toastTimerRef.current = null;
      setToast("");
    }, duration);
  }, []);

  useEffect(
    () => () => {
      if (toastTimerRef.current) {
        window.clearTimeout(toastTimerRef.current);
      }
    },
    []
  );

  return { setToast, showToast, toast };
}
