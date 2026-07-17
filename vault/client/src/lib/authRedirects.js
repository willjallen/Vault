function usesExternalHeaderAuth(authMode, baseDomain) {
  return authMode === "headers" && Boolean(baseDomain);
}

export function authReturnTarget(browserLocation, externalAuth) {
  if (externalAuth) {
    return browserLocation.href;
  }
  const pathname = browserLocation.pathname?.startsWith("/") ? browserLocation.pathname : "/";
  return `${pathname}${browserLocation.search || ""}${browserLocation.hash || ""}`;
}

export function authRedirectUrl({ action, authMode, baseDomain, location: browserLocation }) {
  const externalAuth = usesExternalHeaderAuth(authMode, baseDomain);
  const rd = encodeURIComponent(authReturnTarget(browserLocation, externalAuth));
  if (externalAuth) {
    const externalPath = action === "logout" ? "/logout" : "/";
    return `https://auth.${baseDomain}${externalPath}?rd=${rd}`;
  }
  const localPath = action === "logout" ? "/logout" : "/login";
  return `${localPath}?rd=${rd}`;
}
