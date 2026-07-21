import { useTransfers } from "./useTransfers.js";
import { normalizeSiteSettings } from "./siteSettings.js";
import { authRedirectUrl } from "./authRedirects.js";
import { normalizedAuthFetchError } from "./authFetchErrors.js";

const { useCallback, useMemo, useRef } = React;

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
  const logoutUrl = useMemo(
    () =>
      authRedirectUrl({
        action: "logout",
        authMode,
        baseDomain,
        location: window.location,
      }),
    [authMode, baseDomain]
  );
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
    const loginUrl = authRedirectUrl({
      action: "login",
      authMode,
      baseDomain,
      location: window.location,
    });
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
        throw normalizedAuthFetchError(err);
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
