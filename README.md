# Vault

## Docker

The published image stores all local runtime state under `/data`. A standard deployment only needs one persistent volume:

```sh
cp .env.example .env
# Set VAULT_SESSION_SECRET in .env before starting.
docker compose up -d
```

By default `docker-compose.yml` runs the pinned image tag `ghcr.io/willjallen/vault:v1.0.0`, binds the service to `127.0.0.1:8000`, uses header-based auth for a trusted reverse proxy, and mounts a single named volume at `/data`. Set `VAULT_IMAGE` in `.env` when intentionally upgrading to a newer release. Set `VAULT_SITE_NAME` in `.env` to customize the displayed site name. `VAULT_TTL_SWEEP_INTERVAL_SECONDS` controls how often file expiry policies are applied. `VAULT_MAX_UPLOAD_BYTES` caps a single uploaded file and defaults to 5368709120 bytes. Large uploads use resumable chunks controlled by `VAULT_TRANSFER_CHUNK_BYTES` and `VAULT_TRANSFER_SESSION_TTL_SECONDS`; folder downloads create export artifacts controlled by `VAULT_EXPORT_TTL_SECONDS`.

For local development with the built image, dev mode, and dev auth enabled:

```sh
docker compose -f docker-compose.yml -f docker-compose.dev.yml up --build
```

Do not use the dev override for production. `VAULT_DEV_MODE=1` exposes admin-only debug tools and the app shows prominent development warnings. Production deployments should set a strong `VAULT_SESSION_SECRET` and either run behind a trusted header-auth proxy or configure `VAULT_AUTH_MODE=oidc` with the OIDC variables in `.env.example`.

For OIDC behind TLS termination, set `VAULT_PUBLIC_URL` to the external `https://` origin and leave `VAULT_SESSION_COOKIE_SECURE=auto` so session and OIDC state cookies are marked `Secure` even when the container receives internal HTTP. The Rust service also honors `X-Forwarded-Proto: https` for generated OIDC callback URLs, secure cookies, and HSTS decisions when a trusted reverse proxy supplies that header. The app emits baseline security headers by default and adds HSTS when the public request origin is HTTPS; tune `VAULT_HSTS_MAX_AGE_SECONDS` and `VAULT_HSTS_INCLUDE_SUBDOMAINS` for your domain.

The production image builds local, minified, content-hashed frontend assets with `npm --prefix vault/client run build:assets`; browsers do not load React, Font Awesome, fonts, or modules from public CDNs. Generated assets under `vault/client/dist/` are build output and are not tracked in git. The repository gate builds them locally before tests and validates the asset pipeline with `npm --prefix vault/client run check:assets`.

Embedded hosts can force the first-paint appearance without changing the user's stored browser preference by sending `X-Vault-Palette: winui` and, if needed, `X-Vault-Theme: light|dark|system` on the HTML request.
