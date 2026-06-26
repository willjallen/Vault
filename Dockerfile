FROM python:3.11-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    TZ=UTC \
    VAULT_DATA_DIR=/data \
    VAULT_DB_PATH=/data/vault.db \
    VAULT_OBJECTS_PATH=/data/objects \
    VAULT_STORAGE_BACKEND=local \
    VAULT_STORAGE_PREFIX= \
    VAULT_DOCKER_RUNTIME=1

WORKDIR /app

COPY requirements.txt .
RUN pip install --root-user-action=ignore --no-cache-dir -r requirements.txt \
    && groupadd --system vault \
    && useradd --system --gid vault --home-dir /app --shell /usr/sbin/nologin vault \
    && mkdir -p /data \
    && chown -R vault:vault /app /data

COPY --chown=vault:vault VERSION /app/VERSION
COPY --chown=vault:vault app /app/app

USER vault
VOLUME ["/data"]

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=2).read()" || exit 1

CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000"]
