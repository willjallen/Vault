# Progress

## 2026-06-22

- Adjusted the audit framing toward production data integrity and refactor-regression risk: malformed persisted state, broken invariants, data loss, and operations that make later reads/writes fail.
- Inspected the current worktree and found pre-existing CRLF/line-ending churn across many tracked files; treated it as existing local state and avoided staging it.
- Read the FastAPI route, storage, database, auth, and model code paths for document/folder mutations, archive flows, locking, and downloads.
- Reproduced a malformed-name bad state in an isolated temp database/object store:
  - `POST /documents` creates a normal document.
  - `POST /documents/{id}/move` with `new_path=bad\nname.txt` succeeds and persists the newline in `documents.name`.
  - `GET /documents/{id}/download` through a real Uvicorn socket disconnects because the newline reaches `Content-Disposition` and Uvicorn raises `RuntimeError: Invalid HTTP header value`.
- Added validation/tests to reject control characters in folder paths and item names, and made download headers strip legacy control characters so existing bad rows do not keep breaking downloads.
- Found deploy-time data-loss risk in `init_db()`: an incompatible existing schema triggered an automatic full schema drop even when `VAULT_RESET_DB_ON_START` was not enabled.
- Changed incompatible schema startup behavior to fail closed with a clear error unless explicit reset is enabled, and added a subprocess-backed DB init test proving pre-existing rows survive the failed startup.
- Reproduced direct upload into `Archive/...` as a normal user: the server created an archived document with no archive event, checkout rejected it as archived, and non-admin delete hit the permanent-delete admin gate.
- Added an upload-folder guard so new documents can only be created in Vault paths before any Archive folder rows are created.
- User clarified locks are advisory only and users may unlock each other's files; do not treat cross-user unlock/archive behavior as a permissions bug by itself.
- Reproduced direct `/folders` creation under `Archive/...`: creating `Archive/Project` succeeded, then archiving Vault `Project` failed with `A folder already exists at that path`.
- Added a folder creation guard so user-created folders must start in Vault; archive transitions can still create Archive folders internally.
- Added compatibility handling for legacy empty Archive placeholders so `Archive/Project` can be replaced by a real archived `Project` folder, while non-empty Archive targets still conflict.
- Reproduced the symmetric restore issue: an empty Vault `Project` placeholder blocked restoring `Archive/Project`; added the same empty-placeholder cleanup to folder unarchive while preserving non-empty target conflicts.
- Reproduced stale check-in race: a request validated a checked-out Vault document, the upload read archived it in another session, and check-in still wrote version 2 to the archived document. Added in-lock state refresh/recheck before version creation.
- Hardened checkout and lock against the same stale archive state by refreshing and rechecking editability inside `storage_write_lock()`, and added stale-session regression tests.
- Reproduced storage reconciliation non-idempotence: after deleting a document, `apply=True` reported orphan blob IDs but left the blob row, location row, and local object file intact. Added orphan blob/object cleanup to reconciliation apply.
- Reproduced stale document-location transitions: a stale move could restore an archived document as a plain move, and a stale archive could record duplicate archive transitions. Added in-lock location refresh/rechecks for document move/archive/unarchive.
- Reproduced stale duplicate upload race: `create_document()` checked document-path uniqueness before reading upload bytes, another session created the same path during the read, and the original request raised a raw `IntegrityError` after writing an unreferenced local object. Moved folder resolution/path uniqueness inside `storage_write_lock()` after the upload read but before blob writes, and added regression coverage proving the losing upload returns `400` without leaving orphan storage.
- Audited the OIDC/auth proxy boundary. The app is not an OIDC client; it trusts Authelia-style `Remote-*` headers and has an env-gated dev-auth fallback. Found deployment defaults that bound the service to `0.0.0.0:8000` while hardcoding `VAULT_DEV_AUTH=1`, meaning a compose deployment could mutate production data as local admin without the auth proxy. Changed compose to bind localhost and default dev-auth off, gated dev-auth to local base domains, stripped identity headers, and added auth/defaults tests.
- Reproduced stale permanent-delete data loss: an admin session loaded an archived document, another session restored it, and the stale delete path still permanently deleted the now-live Vault document/version rows, leaving an orphan blob. Moved the archive-only permanent-delete check inside `storage_write_lock()` with a refreshed document location, and added stale delete tests for document restore and folder restore.
