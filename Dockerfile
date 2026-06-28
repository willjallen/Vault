FROM node:22-slim AS assets

WORKDIR /build/vault/client

COPY vault/client/package.json vault/client/package-lock.json ./
RUN npm ci

COPY vault/client ./
RUN npm run build:assets

FROM rust:1.95-slim-bookworm AS rust-builder

WORKDIR /build

COPY Cargo.toml Cargo.lock VERSION ./
COPY vault/server ./vault/server
RUN cargo build --release -p vault-server

FROM debian:bookworm-slim

ENV TZ=UTC \
    VAULT_DATA_DIR=/data \
    VAULT_DB_PATH=/data/vault.db \
    VAULT_OBJECTS_PATH=/data/objects \
    VAULT_STATIC_DIR=/app/vault/client \
    VAULT_STORAGE_BACKEND=local \
    VAULT_STORAGE_PREFIX= \
    VAULT_DOCKER_RUNTIME=1

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system vault \
    && useradd --system --gid vault --home-dir /app --shell /usr/sbin/nologin vault \
    && mkdir -p /data /app/vault/client \
    && chown -R vault:vault /app /data

COPY --from=rust-builder --chown=vault:vault /build/target/release/vault-server /app/vault-server
COPY --chown=vault:vault VERSION /app/VERSION
COPY --chown=vault:vault vault/client/assets /app/vault/client/assets
COPY --from=assets --chown=vault:vault /build/vault/client/dist /app/vault/client/dist

USER vault
VOLUME ["/data"]

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS --max-time 2 http://127.0.0.1:8000/health > /dev/null || exit 1

CMD ["/app/vault-server"]
