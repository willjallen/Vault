import { useTransfers } from "./useTransfers.js";

const { useCallback, useMemo, useRef } = React;

function connectionError() {
  const error = new Error("Lost connection to the server.");
  error.status = 0;
  return error;
}

export function useAuthFetch({ initialBootstrap, setToast }) {
  const baseDomain =
    initialBootstrap.base_domain ||
    (window.location.hostname.includes(".")
      ? window.location.hostname.split(".").slice(1).join(".")
      : "");
  const authMode = initialBootstrap.auth_mode || "headers";
  const logoutUrl = useMemo(() => {
    const rd = encodeURIComponent(window.location.href);
    if (authMode === "headers" && baseDomain) {
      return `https://auth.${baseDomain}/logout?rd=${rd}`;
    }
    return `/logout?rd=${rd}`;
  }, [authMode, baseDomain]);
  const redirectingRef = useRef(false);

  const redirectToLogin = useCallback(() => {
    if (redirectingRef.current) {
      return;
    }
    redirectingRef.current = true;
    setToast("Session expired. Redirecting to login...");
    const rd = encodeURIComponent(window.location.href);
    const loginUrl =
      authMode === "headers" && baseDomain
        ? `https://auth.${baseDomain}/?rd=${rd}`
        : `/login?rd=${rd}`;
    window.location.href = loginUrl;
  }, [authMode, baseDomain, setToast]);

  const transfersApi = useTransfers({ onUnauthorized: redirectToLogin });

  const apiFetch = useCallback(
    async (url, options = {}) => {
      try {
        const res = await fetch(url, { credentials: "include", ...options });
        const redirectedToAuth =
          res.redirected && res.url && res.url.includes("auth.") && res.url.includes("://auth.");
        if (res.type === "opaqueredirect" || res.status === 401 || redirectedToAuth) {
          redirectToLogin();
          const error = new Error("Redirecting to login");
          error.status = 401;
          throw error;
        }
        return res;
      } catch (err) {
        if (err.status === 401) {
          throw err;
        }
        throw connectionError();
      }
    },
    [redirectToLogin]
  );

  return {
    apiFetch,
    logoutUrl,
    ...transfersApi,
  };
}
