# Vault

## Docker

The published image stores all local runtime state under `/data`. A standard deployment only needs one persistent volume:

```sh
cp .env.example .env
# Set VAULT_SESSION_SECRET to the 64-hex output of `openssl rand -hex 32` and,
# for header auth, set FORWARDED_ALLOW_IPS before starting.
docker compose up -d
```

By default `docker-compose.yml` runs the published image tag `ghcr.io/willjallen/vault:latest`, binds the service to `127.0.0.1:8000`, uses header-based auth for a trusted reverse proxy, and mounts a single named volume at `/data`. Set `VAULT_IMAGE` in `.env` to pin a specific release image tag. Set `VAULT_SITE_NAME` in `.env` to customize the displayed site name. `VAULT_TTL_SWEEP_INTERVAL_SECONDS` controls how often file expiry policies are applied. `VAULT_MAX_UPLOAD_BYTES` caps a single uploaded file and defaults to 5368709120 bytes. Large uploads use resumable chunks controlled by `VAULT_TRANSFER_CHUNK_BYTES` and `VAULT_TRANSFER_SESSION_TTL_SECONDS`; folder downloads create export artifacts controlled by `VAULT_EXPORT_TTL_SECONDS`. Export work is drained by `VAULT_EXPORT_WORKERS` fixed workers (hard-capped at 64), while `VAULT_EXPORT_MAX_ACTIVE_JOBS` and `VAULT_EXPORT_MAX_ACTIVE_JOBS_PER_USER` bound queued, running, and finalizing jobs globally and per user. Normal submissions wake the dispatcher immediately; a low-frequency 15-second SQLite poll is only the fallback for durable rows inserted without an in-process notification.

For local development with the built image, dev mode, and dev auth enabled:

```sh
docker compose -f docker-compose.yml -f docker-compose.dev.yml up --build
```

Do not use the dev override for production. `VAULT_DEV_MODE=1` exposes admin-only debug tools and the app shows prominent development warnings. Production deployments must set `VAULT_SESSION_SECRET` to exactly 32 random bytes encoded as 64 hexadecimal characters and either run behind a trusted header-auth proxy or configure `VAULT_AUTH_MODE=oidc` with the OIDC variables in `.env.example`. Arbitrary passwords and low-diversity values are rejected.

To rotate the signing root without immediately invalidating sessions or resumable-upload tokens, generate a new `VAULT_SESSION_SECRET`, move the old value into the comma-separated `VAULT_SESSION_SECRET_PREVIOUS` list, and restart Vault. New tokens use only the new root; prior roots are accepted only for verification. Keep an old root for at least the greater of `VAULT_SESSION_MAX_AGE_SECONDS` and `VAULT_TRANSFER_SESSION_TTL_SECONDS`, then remove it. At most four prior roots are accepted. The first release with domain-separated signing keys intentionally does not accept tokens created by older raw-HMAC releases, so users must sign in again after that upgrade and active resumable uploads may need to fetch a fresh token through their authenticated session.

Header authentication requires `FORWARDED_ALLOW_IPS` to contain the direct source IPs or CIDRs of the trusted reverse proxies, separated by commas. Vault matches the TCP peer that connected to it and never uses `X-Forwarded-For` to decide whether identity headers are trusted. Configure the address as seen inside the Vault container; a reverse proxy running on the Docker host commonly appears as the Docker bridge gateway rather than `127.0.0.1`. The proxy must remove or overwrite client-supplied `Remote-User`, `Remote-Name`, `Remote-Email`, `Remote-Groups`, and `X-Forwarded-*` headers. Changes to the trust list require a restart. Vault refuses to start in header mode when the trust list is missing or invalid.

For OIDC behind TLS termination, set `VAULT_PUBLIC_URL` to the external `https://` origin and leave `VAULT_SESSION_COOKIE_SECURE=auto` so session and OIDC state cookies are marked `Secure` even when the container receives internal HTTP. The Rust service also honors `X-Forwarded-Proto: https` for generated OIDC callback URLs, secure cookies, and HSTS decisions when the direct proxy peer is listed in `FORWARDED_ALLOW_IPS`; forwarded headers from every other source are removed. The app emits baseline security headers by default and adds HSTS when the public request origin is HTTPS; tune `VAULT_HSTS_MAX_AGE_SECONDS` and `VAULT_HSTS_INCLUDE_SUBDOMAINS` for your domain.

Bootstrap OIDC administrators with exact, case-sensitive subject identifiers in `VAULT_OIDC_BOOTSTRAP_ADMIN_SUBJECTS` (comma-separated and implicitly bound to `VAULT_OIDC_ISSUER`). `VAULT_BOOTSTRAP_ADMIN_EMAILS` applies only to trusted header/dev identities; OIDC email claims are not authorization identifiers.

The production image builds local, minified, content-hashed frontend assets with `npm --prefix vault/client run build:assets`; browsers do not load React, Font Awesome, fonts, or modules from public CDNs. Generated assets under `vault/client/dist/` are build output and are not tracked in git. The repository gate builds them locally before tests and validates the asset pipeline with `npm --prefix vault/client run check:assets`.

Downloads use a single-request File System Access stream by default when the browser exposes that API; it avoids range probes, parallel segments, workers, and whole-file browser buffers. Vault always presents one `Download` action and automatically uses the native download manager when the picker API is unavailable. The `customDownloadStreamingEnabled` site setting defaults to `true` and can disable the picker stream globally. Browsers using the native path show guidance explaining the browser-wide download-location setting until the user selects `Do not show again`; that dismissal is stored in the user's synced Vault preferences.

Embedded hosts can force the first-paint appearance without changing the user's stored browser preference by sending `X-Vault-Palette: winui` and, if needed, `X-Vault-Theme: light|dark|system` on the HTML request.
