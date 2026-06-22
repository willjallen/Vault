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
