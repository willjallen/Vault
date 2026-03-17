FROM python:3.11-slim

ARG TZ=UTC
ARG BASE_DOMAIN=family.localhost
ARG VAULT_FILES_PATH=/vault-files
ARG VAULT_DB_PATH=/vault-metadata/vault-metadata.db

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    TZ=${TZ} \
    BASE_DOMAIN=${BASE_DOMAIN} \
    VAULT_FILES_PATH=${VAULT_FILES_PATH} \
    VAULT_DB_PATH=${VAULT_DB_PATH}

WORKDIR /app

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY app /app/app

EXPOSE 8000

CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000"]
