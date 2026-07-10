import { useTransfers } from "./useTransfers.js";
import { normalizeSiteSettings } from "./siteSettings.js";

const { useCallback, useMemo, useRef } = React;

function connectionError() {
  const error = new Error("Lost connection to the server.");
  error.status = 0;
  return error;
}

export function useAuthFetch({ initialBootstrap, requestConfirm, showNotice }) {
  const baseDomain =
    initialBootstrap.base_domain ||
    (window.location.hostname.includes(".")
      ? window.location.hostname.split(".").slice(1).join(".")
      : "");
  const authMode = initialBootstrap.auth_mode || "headers";
  const customDownloadsEnabled = normalizeSiteSettings(
    initialBootstrap.settings
  ).customDownloadStreamingEnabled;
  const downloadLocationGuidanceDismissed =
    initialBootstrap.preferences?.downloadLocationGuidanceDismissed === true;
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
    showNotice({
      detail: "Redirecting to login...",
      dismissible: false,
      kind: "info",
      title: "Session expired",
    });
    const rd = encodeURIComponent(window.location.href);
    const loginUrl =
      authMode === "headers" && baseDomain
        ? `https://auth.${baseDomain}/?rd=${rd}`
        : `/login?rd=${rd}`;
    window.location.href = loginUrl;
  }, [authMode, baseDomain, showNotice]);

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

  const saveDownloadLocationGuidanceDismissed = useCallback(async () => {
    const response = await apiFetch("/api/preferences", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        preferences: { downloadLocationGuidanceDismissed: true },
      }),
    });
    if (!response.ok) {
      const detail = await response.json().catch(() => ({}));
      throw new Error(detail.detail || "Could not save download preference");
    }
  }, [apiFetch]);

  const transfersApi = useTransfers({
    customDownloadsEnabled,
    downloadLocationGuidanceDismissed,
    onUnauthorized: redirectToLogin,
    requestConfirm,
    saveDownloadLocationGuidanceDismissed,
  });

  return {
    apiFetch,
    logoutUrl,
    ...transfersApi,
  };
}
