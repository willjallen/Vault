# Vault

## Docker

The published image stores all local runtime state under `/data`. A standard deployment only needs one persistent volume:

```sh
cp .env.example .env
# Set VAULT_SESSION_SECRET in .env before starting.
docker compose up -d
```

By default `docker-compose.yml` runs `ghcr.io/willjallen/vault:latest`, binds the service to `127.0.0.1:8000`, uses header-based auth for a trusted reverse proxy, and mounts a single named volume at `/data`. Set `VAULT_SITE_NAME` in `.env` to customize the displayed site name. `VAULT_TTL_SWEEP_INTERVAL_SECONDS` controls how often file expiry policies are applied.

For local development with the built image and dev auth enabled:

```sh
docker compose -f docker-compose.yml -f docker-compose.dev.yml up --build
```

Do not use the dev override for production. Production deployments should set a strong `VAULT_SESSION_SECRET` and either run behind a trusted header-auth proxy or configure `VAULT_AUTH_MODE=oidc` with the OIDC variables in `.env.example`.
