const { useEffect } = React;

const BROWSER_BACK_BUTTON = 3;
const BROWSER_FORWARD_BUTTON = 4;

function isBrowserNavigationButton(evt) {
  return evt.button === BROWSER_BACK_BUTTON || evt.button === BROWSER_FORWARD_BUTTON;
}

export function useMouseNavigation({ onBack, onForward }) {
  useEffect(() => {
    function preventBrowserNavigation(evt) {
      if (!isBrowserNavigationButton(evt)) {
        return;
      }
      evt.preventDefault();
    }

    function handleBrowserNavigation(evt) {
      if (!isBrowserNavigationButton(evt)) {
        return;
      }
      evt.preventDefault();
      evt.stopPropagation();
      if (evt.button === BROWSER_BACK_BUTTON) {
        onBack();
        return;
      }
      onForward();
    }

    window.addEventListener("mousedown", preventBrowserNavigation, true);
    window.addEventListener("mouseup", handleBrowserNavigation, true);
    window.addEventListener("auxclick", preventBrowserNavigation, true);
    return () => {
      window.removeEventListener("mousedown", preventBrowserNavigation, true);
      window.removeEventListener("mouseup", handleBrowserNavigation, true);
      window.removeEventListener("auxclick", preventBrowserNavigation, true);
    };
  }, [onBack, onForward]);
}
