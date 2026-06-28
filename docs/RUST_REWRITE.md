# Rust Rewrite Parity Ledger

This file is the migration contract for replacing the Python service with Rust end to end. The Rust implementation is not complete until every item here is implemented and verified by tests or benchmarks that cover the same behavior as the current service.

## Current Rust Status

Implemented:

- `vault/server`: Rust Axum service crate.
- Runtime config for host, port, data directory, database path, object path, legacy object path fallbacks, transfer path, storage backend, storage prefix, Python-compatible numeric safety bounds, and gzip response settings.
- SQLite connection pool with WAL, foreign keys, `synchronous=NORMAL`, and 30 second busy timeout.
- Initial SQLite schema bootstrap for the current canonical tables and indexes.
- SQLite startup schema validation rejects incompatible existing metadata without altering or dropping it, while preserving known additive migrations for user preferences, transfer/export tables, upload verification columns, share-link fields, and export-job fields.
- Root folder seeding for the Vault root and Archive root.
- `/health` and `/api/health`.
- Rust checks are part of `.pre-commit-config.yaml`: `cargo fmt`, `cargo clippy`, and `cargo test`.
- Header auth canonical user sync.
- Dev auth local-domain guard and configured dev identity sync.
- Vault group creation, membership sync, root permissions for synced groups.
- Effective admin resolution from stored admin flag, configured admin groups, and bootstrap admin emails.
- HMAC signed session payload validation and active-user session lookup.
- OIDC mode request handling for Rust HTTP routes: API requests without a valid session return `401`, browser app-shell GETs redirect to `/login?rd=...`, `/login` builds an OIDC authorization redirect and signed state cookie, `/auth/callback` performs discovery, token exchange, JWKS-backed ID-token validation, userinfo merge, canonical user sync, and session cookie creation, and `/logout` clears session/state cookies with safe local redirects.
- Local content-addressed storage for direct blobs.
- Local multipart manifest storage for already-verified upload parts.
- Local object range reads, object listing, delete cleanup, and path traversal rejection.
- Configured Rust storage backend selection for local, S3-compatible, and Cloudflare R2 storage, with S3/R2 direct blob put, file put, upload part-file promotion, full reads, range reads, deletes, content-addressed keys, bucket metadata persistence, and missing-configuration validation.
- Rust local storage reconciliation reports and apply-mode repair behavior, including missing referenced local object detection, corrupt referenced object detection without deletion, recoverable local-location restoration, local orphan blob/object cleanup, and remote orphan metadata preservation.
- Folder path normalization, public Vault/Archive path parsing, root lookup, folder creation, and path cache reconstruction.
- Folder permission flag validation and effective folder ACL lookup with nearest-direct-rule inheritance.
- Shared Rust folder access gates for read and write routes, preserving hidden-vs-visible-only error semantics across exports and folder mutations.
- Document path helpers, archive detection, active document ACL delegation, archived document ACL snapshots, and archived access caps.
- Shared Rust document access gates for read, write, and edit-only routes, preserving hidden-vs-visible-only error semantics and archived-document edit rejection across downloads, exports, uploads, and document mutations.
- Authenticated Rust `/api/folders/sidebar` route with visible folder children and folder metadata.
- Authenticated Rust `/api/folders/contents` route with folder/document ACL filtering, search, Python-compatible recursive expansion only for non-empty searches, and document row access payloads.
- Authenticated Rust `POST /folders` route with form parsing, Vault-only creation, duplicate rejection, nearest-existing-parent write ACL enforcement, creator metadata, folder history event, and folder state event.
- Authenticated Rust `GET /api/folders/properties` route with summary metadata, visible subtree counts, history, direct permissions, and available groups.
- Authenticated Rust `PATCH /api/folders/properties` route with color/icon validation, write-access enforcement, folder metadata history, and folder state event emission.
- Authenticated admin-only Rust `PUT /api/folders/permissions` route with direct permission replacement, flag validation, duplicate-group rejection, missing-group rejection, folder permissions history, and folder state event emission.
- Authenticated Rust `PUT /api/folders/retention` route with TTL policy validation, delete-policy admin guard, full subtree write-access enforcement, descendant document expiry recomputation, retention history, and document-aware folder state event emission.
- Authenticated Rust `/api/documents/{doc_id}/detail` route with read-access enforcement, base document row payload, version history, event history, and inconsistent current-version detection.
- Authenticated Rust `/api/lock` and `/api/unlock` routes with bulk action normalization, document write-access enforcement, archived edit rejection, lock ownership checks, document event history, client metadata capture, and batch state event emission.
- Authenticated Rust `/api/my-edits` route with active-lock ownership filtering, current write-access filtering, document row payloads, lock payloads, and path sorting.
- Authenticated Rust `/api/delete-forever` route with archive permanent-delete policy enforcement, archived-only deletion, per-item write-access checks, active-lock ownership checks, cascaded document cleanup, and `document.deleted` state event emission.
- Authenticated Rust `/api/move` route with destination normalization, nested item pruning, document/folder write-access enforcement, archive move rejection, duplicate target rejection, active-lock checks, subtree TTL recomputation, history event capture, and `batch.move` state event emission.
- Authenticated Rust `/api/rename` route with single-item bulk action normalization, document/folder write-access enforcement, archive move rejection, archived document rename rejection, duplicate target rejection, active-lock checks, TTL recomputation, history event capture, and `batch.rename` state event emission.
- Authenticated Rust `/api/archive` and `/api/restore` routes with nested item pruning, archive ACL snapshots, archive metadata mutation, owned-lock release on archive, restore metadata validation, duplicate restore-target rejection, folder subtree archiving, Vault folder placeholder cleanup, and batch archive/restore state event emission.
- Authenticated Rust `GET /documents/{doc_id}/download` and `GET /documents/{doc_id}/versions/{version_id}/download` routes with current document access rechecks, document read-access enforcement, current-version consistency checks, blob location lookup, stored blob size/SHA-256 validation before full or ranged responses, download headers, ETag/Range handling, download history events, and `document.download` state events.
- Authenticated Rust `GET /documents/{doc_id}/checkout` route with document write-access enforcement, archived edit rejection, current-version consistency checks, local storage response, lock acquisition/ownership checks, checkout history event, and `document.checkout` state event emission.
- Authenticated Rust `POST /api/download` single-document fast path with legacy action-item payload parsing, read-access enforcement, local storage response, download headers, download history event, and `document.download` state event emission.
- Authenticated Rust multi-item/folder `POST /api/download` and export routes: `POST /api/exports`, `GET /api/exports/{job_id}`, `DELETE /api/exports/{job_id}`, and `GET /api/exports/{job_id}/download`, with owner/admin transfer isolation, readable document resolution, persisted request/user context, queued background execution, per-entry progress updates, Python-compatible ZIP compression threshold and entry heuristics, ZIP64 header/footer support, local ZIP artifact creation, content-addressed artifact storage, export status payloads, range-capable artifact downloads, and queued/running/finalizing cancellation.
- Rust action-item normalization prunes explicit child selections when a parent folder is selected before bulk mutations, downloads, and exports see the item list, preserving the Python parent-selection contract.
- Rust legacy `POST /documents` route returns `410 Use resumable upload sessions`, and legacy `GET /documents/{doc_id}` preserves the visible-access-gated `303 /` redirect.
- Authenticated Rust local resumable upload routes: `POST /api/uploads`, `GET /api/uploads/{session_id}`, `PUT /api/uploads/{session_id}/parts/{part_number}`, `POST /api/uploads/{session_id}/complete`, and `DELETE /api/uploads/{session_id}`.
- Rust upload sessions support create and check-in modes, adaptive chunk sizing, signed upload-part tokens, streamed part ingest with range/checksum/idempotency checks, transfer-directory sidecar part state instead of per-part SQLite writes, content-addressed local multipart promotion, S3/R2 part-file promotion, canonical document/version commit, stale archived-state rechecks after part upload, check-in rename, lock release, upload/check-in state events, and transfer cleanup.
- Rust expired document retention sweep behavior for startup, periodic, and dev-admin TTL sweeps: expired archive TTL files are moved to the flat Archive with origin metadata, expired delete TTL files are deleted directly, active locks are skipped, and `retention.expired` state events notify affected resources.
- Rust expired transfer sweep behavior for dev-admin TTL sweeps: expired active/completing uploads are marked expired and scratch files are removed, expired terminal upload sessions are deleted, expired queued/running/finalizing exports are cancelled, expired terminal exports are deleted, and unreferenced local export artifacts are removed without deleting blobs still referenced by document versions.
- Rust startup transfer recovery resets interrupted completing uploads with recoverable part files back to active, fails interrupted uploads with missing part files, requeues interrupted running/finalizing exports, removes partial export scratch/artifacts, deletes unreferenced partial export blobs, and starts pending queued export jobs.
- Rust legacy `POST /documents/{doc_id}/checkin` route returns `410 Use resumable upload sessions`.
- Authenticated Rust `/api/events/stream` route with `Last-Event-ID` replay, zero-row-safe latest-id startup behavior for fresh streams, `event: state` SSE payloads, normalized resource lists, heartbeat comments, health-safe idle concurrency, and notification-driven wakeups after committed state events.
- Authenticated Rust `/api/bootstrap` route with auth/runtime metadata, current folder validation, normalized default user preferences, site settings defaults, app version, and user context.
- Authenticated Rust `GET /` and `GET /s/{code}` app-shell routes with initial bootstrap/contents/sidebar/my-edits state, share-code bootstrapping, appearance header overrides, script-safe JSON state embedding, and manifest-backed frontend asset references.
- Rust `/static/{*path}` route with manifest-compatible static directory configuration, content type handling, path traversal rejection, and startup validation for required dist assets.
- Rust security-header middleware applies default `nosniff`, frame-deny, referrer, permissions-policy, CSP nonce, custom CSP replacement, optional disablement, and HTTPS/public-URL-gated HSTS headers across HTTP responses; app-shell script nonces match the CSP nonce.
- Rust gzip response middleware honors `VAULT_GZIP_MINIMUM_SIZE` and `VAULT_GZIP_COMPRESSLEVEL`, disables at minimum size `0`, and preserves identity/range download responses.
- Rust user preference normalization for theme, palette, booleans, favorites, and sidebar section state.
- Rust preference patch payloads preserve Python's missing-`preferences` no-op behavior.
- Rust app-shell and frontend preference normalization accept only real boolean values; boolean strings are invalid and fall back.
- Rust favorite item path/detail enrichment for visible folder and document favorites, with inaccessible and missing favorites filtered out.
- Rust folder/document presentation payloads normalize SQLite/RFC3339 timestamps to Python-compatible UTC ISO strings, format `modified_display` and document history `display` values with the same human-readable strings as the Python service, and choose folder summary latest-document metadata by parsed UTC time instead of raw string order.
- Rust folder summary presentation only includes `access` on folder rows and favorite folders; folder properties and resolved folder shares keep Python's base summary shape without `access`.
- Rust document-row presentation paths reject visible documents with inconsistent current-version metadata, including stale pointers and empty pointers with existing version rows, instead of silently rendering versionless rows.
- Rust sidebar root folder children preserve Python's raw path sorting for mixed-case folder names.
- Rust folder contents sorting and search matching use Python-compatible Unicode lowercasing for user-visible folder and document names.
- Authenticated Rust `GET /api/preferences` and `PATCH /api/preferences` routes with strict patch validation, canonical DB storage, and enriched favorite responses.
- Authenticated Rust `GET /api/settings` route with normalized site settings.
- Authenticated Rust share-link routes: `POST /api/share-links` and `GET /api/share-links/{code}` with target validation, configured public URL generation, live target access/location rechecks, disabled/expired link rejection, stale target cleanup, and cascade behavior for freshly bootstrapped databases.
- Authenticated Rust `GET /api/admin/directory` route with users, groups, dev mode, effective admin status, memberships, and settings.
- Authenticated dev-admin Rust debug routes:
  `POST /api/admin/debug/error`, `POST /api/admin/debug/timeout`, `POST /api/admin/debug/emit-state`, `POST /api/admin/debug/sweep-ttl`, `POST /api/admin/debug/storage-report`, `POST /api/admin/debug/seed`, and `POST /api/admin/debug/reset-database`.
- Rust debug routes enforce dev-mode hiding outside development, admin access, debug HTTP error/status responses, event-stream retry generation, state-event resource filtering with Python-compatible omitted-resource defaults, local sample-file seeding, read-only local storage report shape, TTL sweep result shape, and database reset with root reseeding.
- Authenticated admin-only Rust `PATCH /api/admin/settings` route with strict patch validation, persisted site settings, admin directory response, and settings state event emission.
- Authenticated admin-only Rust user and group management routes:
  `PATCH /api/admin/users/{user_id}`, `POST /api/admin/groups`, `PATCH /api/admin/groups/{group_id}`, `DELETE /api/admin/groups/{group_id}`, `POST /api/admin/groups/{group_id}/members`, and `DELETE /api/admin/groups/{group_id}/members/{user_id}`.
- Rust admin user/group mutations enforce group-name normalization, duplicate-group conflicts, folder-permission delete protection, membership idempotency, state event emission, and last-active-admin safety.
- Rust site settings normalization and strict patch validation for archive permanent-delete policy.
- Rust runtime auth/session configuration validation rejects invalid auth modes, unsafe production session secrets, invalid cookie security modes, incomplete OIDC configuration, and insecure production OIDC/public URLs while preserving local development exceptions.
- Rust OIDC route parity covers provider error callbacks, missing callback code/state rejection before provider exchange, missing discovery endpoints, missing ID-token rejection, state mismatch rejection, userinfo subject mismatch rejection, proxied callback URL derivation from forwarded host/proto headers including comma-separated proxy chains, custom session/state cookie names, public-client token exchange without leaking a configured client secret, and `client_secret_post` token exchange without Basic auth.
- Rust OIDC login generates Python-compatible URL-safe state and nonce tokens using the configured `VAULT_OIDC_NONCE_BYTES` value with the same 16-byte lower bound; Rust OIDC discovery caches provider metadata by issuer for the configured `VAULT_OIDC_DISCOVERY_TTL_SECONDS`; production Compose passes both settings into the container.
- Rust non-OIDC auth route parity covers `/login` and `/auth/callback` redirecting to `/` without setting cookies, while `/logout` remains available and clears session/state cookies with a safe redirect.
- Rust dev-auth route parity keeps development auth separate from header auth: disabled dev mode rejects even when identity headers are present, and enabled dev mode resolves only the configured local development identity.
- Rust benchmark harness includes direct-host and Docker-container Rust app runners, Rust in-process receive/hash/write sink variants, opt-in throughput threshold enforcement for the documented local-direct upload/download target profile plus generic upload/download floors for ad hoc regression gates, and per-case server CPU/RSS capture when process metrics are available.
- Rust upload part PUTs now avoid per-part SQLite writes, avoid no-checksum JSON sidecars, verify signed upload-token bounds statelessly without per-part session-row reads or a process-global claims cache, atomically promote part files without overwriting already-promoted duplicates, return `204 No Content` acknowledgements, and keep full resumable state on `GET /api/uploads/{session_id}`.
- Docker image replacement: the production Dockerfile builds content-hashed frontend assets, compiles the Rust `vault-server` release binary, runs it as the non-root `vault` user, preserves `/data` runtime state, serves bundled static assets, and healthchecks the Rust `/health` endpoint.

Not implemented:

- General permission enforcement helpers for all remaining mutating/download routes.
- Full folder/sidebar/contents payload parity for every presentation field and edge case.

## Remaining Allowed Work

After the current state-event parity fix and full repository gate are complete,
the Rust rewrite is limited to these five high-impact completion items:

- Permission parity for every remaining mutating and download route, including
  admin, owner, folder ACL, archive state, deleted state, shared-link, and
  bootstrap-admin behavior.
- Folder, sidebar, and contents payload parity for tree, listing, favorites,
  archive, shared, and my-edits response shapes.
- Upload and download correctness under restart, including resumable uploads,
  incomplete parts, completed blobs, sidecar state, hash promotion, failed
  requests, and server restarts.
- Frontend runtime smoke coverage against the Rust server for the core app
  workflows.
- Performance acceptance for upload and download throughput, with focused
  profiling only on proven bottlenecks.

## Route Parity

Authentication and session:

- `GET /login`
- `GET /auth/callback`
- `GET /logout`

Bootstrap and user settings:

- `GET /`
- `GET /s/{code}`
- `GET /api/bootstrap`
- `GET /api/settings`
- `GET /api/preferences`
- `PATCH /api/preferences`

Admin:

- `GET /api/admin/directory`
- `POST /api/admin/debug/error`
- `POST /api/admin/debug/timeout`
- `POST /api/admin/debug/emit-state`
- `POST /api/admin/debug/sweep-ttl`
- `POST /api/admin/debug/storage-report`
- `POST /api/admin/debug/seed`
- `POST /api/admin/debug/reset-database`
- `PATCH /api/admin/settings`
- `PATCH /api/admin/users/{user_id}`
- `POST /api/admin/groups`
- `PATCH /api/admin/groups/{group_id}`
- `DELETE /api/admin/groups/{group_id}`
- `POST /api/admin/groups/{group_id}/members`
- `DELETE /api/admin/groups/{group_id}/members/{user_id}`

Folders, documents, and state:

- `GET /api/folders/sidebar`
- `GET /api/folders/contents`
- `GET /api/folders/properties`
- `PATCH /api/folders/properties`
- `PUT /api/folders/retention`
- `PUT /api/folders/permissions`
- `GET /api/documents/{doc_id}/detail`
- `GET /api/my-edits`
- `GET /api/events/stream`
- `POST /folders`
- `POST /api/move`
- `POST /api/rename`
- `POST /api/archive`
- `POST /api/restore`
- `POST /api/delete-forever`
- `POST /api/lock`
- `POST /api/unlock`

Sharing:

- `POST /api/share-links`
- `GET /api/share-links/{code}`

Uploads and downloads:

- `POST /api/uploads`
- `GET /api/uploads/{session_id}`
- `PUT /api/uploads/{session_id}/parts/{part_number}`
- `POST /api/uploads/{session_id}/complete`
- `DELETE /api/uploads/{session_id}`
- `POST /api/download`
- `POST /documents`
- `GET /documents/{doc_id}`
- `GET /documents/{doc_id}/checkout`
- `GET /documents/{doc_id}/download`
- `POST /documents/{doc_id}/checkin`
- `GET /documents/{doc_id}/versions/{version_id}/download`

Exports:

- `POST /api/exports`
- `GET /api/exports/{job_id}`
- `DELETE /api/exports/{job_id}`
- `GET /api/exports/{job_id}/download`

## Test Parity

The existing Python test suite is the behavioral target until each test has an equivalent Rust integration test or is explicitly retired because the Python implementation no longer exists.

Rust equivalents started:

- `test_auth.py`
  - missing header identity rejects without dev auth
  - header identity trimming and group sync
  - root folder permissions for synced groups
  - admin group removal revokes effective admin
  - configured bootstrap admin emails grant effective admin without persisting the stored admin flag
  - disabled users reject without syncing profile or groups
  - concurrent header identity upserts recover to one canonical user/group membership
  - dev auth requires a local base domain
  - dev auth syncs configured local identity and groups
  - dev auth mode rejects when disabled even with spoofed identity headers
  - dev auth mode ignores identity headers and uses only the configured local development user
  - signed session payload expiration/user-id validation, including missing, expired, boolean, and non-ASCII payload rejection
  - signed session cookie lookup uses exact cookie names in multi-cookie headers
  - signed session resolves active users and ignores inactive users
  - OIDC state and session cookies honor HTTPS public URL, forwarded HTTPS, and explicit secure-cookie overrides
  - OIDC mode returns API `401` without a valid session
  - OIDC mode redirects browser app-shell GETs to `/login` with the current return path
  - OIDC login redirects to the configured provider authorization endpoint and stores a signed state cookie
  - OIDC login rejects insecure non-local authorization endpoints
  - OIDC login derives callback URLs from forwarded host/proto headers when no explicit redirect URL or public URL is configured, including comma-separated proxy-chain headers
  - non-OIDC login and callback routes redirect to `/` without setting cookies while logout clears auth cookies with a safe redirect
  - OIDC callback verifies a real RS256 provider token through discovery/JWKS, syncs the canonical user/groups, and sets a signed session cookie
  - OIDC callback rejects state mismatches before provider exchange
  - OIDC callback rejects userinfo subject mismatches
  - OIDC callback with `client_auth=none` does not send a client secret or authorization header
  - OIDC login and callback reject missing discovery endpoints before continuing the flow
  - OIDC callback with `client_auth=client_secret_post` sends the secret in the form without Basic auth
  - OIDC login, callback, and logout use configured valid session/state cookie names consistently
  - runtime validation rejects invalid session/state cookie names before cookie headers can split from cookie lookup
  - logout clears session and OIDC state cookies and rejects unsafe external return URLs

- `test_db_init.py`
  - SQLite runtime busy timeout is applied through the Rust connection setup
  - fresh database bootstrap creates the canonical schema and root folders
  - incompatible existing schemas are rejected without dropping existing data
  - missing known additive user-preference columns and settings tables are added without losing rows
  - missing known additive upload verification columns are added without losing rows
  - missing required model columns are rejected without silently repairing the table
  - unexpected model columns are rejected without rebuilding the table
  - missing or wrong unique model indexes are rejected on startup
  - unexpected unique indexes are rejected on startup
  - missing/wrong unique constraints and missing primary keys are rejected on startup
  - missing or unexpected foreign keys are rejected on startup
  - required-column nullability drift, column type drift, and unexpected check constraints are rejected on startup
  - unexpected triggers on model tables are rejected on startup

- `test_storage_reconciliation.py` / `test_streaming_transfers.py`
  - current-version detail and download routes reject missing, `NULL`, or empty-string current-version pointers on versioned documents as inconsistent metadata
  - local direct blob content-addressing and dedupe
  - local object key traversal rejection
  - local direct blob range reads
  - S3-compatible storage direct byte puts, upload part-file promotion, checksum mismatch rejection, full reads, byte ranges, deletes, content-addressed metadata, missing bucket validation against an in-process S3-shaped server, S3 AWS credential env fallback, and R2 account-derived endpoint env construction
  - local verified part promotion to multipart manifests without listing part files
  - local multipart manifest reads, ranges, and delete cleanup
  - local unverified part assembly into a content-addressed blob
  - storage reconciliation reports missing referenced local objects
  - storage reconciliation flags corrupt referenced local objects and apply mode does not delete them
  - storage reconciliation apply mode restores missing local `blob_locations` metadata for recoverable referenced objects
  - storage reconciliation apply mode removes local-only orphan blob metadata and local objects
  - storage reconciliation apply mode preserves remote orphan metadata when local deletion is unsupported
  - expired transfer sweep marks active uploads expired and removes terminal upload sessions with scratch cleanup
  - expired transfer sweep cancels active exports and deletes terminal export artifact objects
  - expired transfer sweep preserves export artifact blobs still referenced by document versions
  - dev debug TTL sweep route returns real transfer cleanup results
  - startup transfer recovery resumes recoverable completing uploads and fails interrupted uploads with missing part files
  - startup transfer recovery requeues interrupted running/finalizing exports and removes partial export artifacts/scratch
  - startup transfer recovery starts pending queued exports and lets the background worker complete them

- `test_acl_permissions.py` / `test_folder_stale_state.py`
  - public folder path normalization and Archive root parsing
  - Vault folder path creation and Archive subfolder rejection
  - folder path cache reconstruction for roots and children
  - permission flag validation for read/write invariants
  - effective folder ACL inheritance and nearest direct ACL cutoff
  - admin folder access override
  - shared folder access helpers preserve read/write/hidden semantics for route callers
  - folder permissions route replaces direct folder permissions and updates downstream folder access
  - folder permissions route rejects write-without-read/view, duplicate groups, and missing groups without mutating existing rows
  - folder creation route requires write access on the nearest existing parent path
  - folder creation route rejects archive paths, duplicate folders, read-only parents, and invisible parents
  - child folders created under restricted parents inherit the effective restriction without copying ACL rows
  - failed folder archive attempts preserve source folder ACL rows
  - document folder paths and document paths
  - active document ACL delegation to folder ACLs
  - archived document ACL snapshot generation
  - archived document access capped by archive folder access and source snapshot access
  - shared document access helpers preserve read/write/hidden semantics and reject edit-only access for archived documents
  - Archive folder contents hides archived documents when the source ACL snapshot denies the viewer
  - restore route preserves current Vault folder ACLs instead of replaying archived ACL snapshots

- `test_http_api_contracts.py`
  - folder routes reject unauthenticated requests
  - folder creation route rejects unauthenticated requests
  - folder creation route persists creator metadata, folder history, and folder state events
  - settings route rejects unauthenticated requests
  - settings route returns normalized default settings
  - sidebar exposes only visible Vault root children
  - sidebar metadata, folder contents rows, and document rows expose Python-compatible payload key sets and representative values
  - sidebar root children use Python-compatible raw path sorting for mixed-case folder names
  - folder contents returns document access payloads
  - folder contents trims search queries and only expands recursive scope when `recursive=true` is paired with a non-empty search query
  - folder contents sorting and search matching use Python-compatible Unicode lowercasing for user-visible folder and document names
  - folder contents, folder properties, and document detail rows return Python-compatible normalized modified/expiry timestamp fields and presentation timestamp display strings
  - folder properties and resolved folder share summaries omit the folder `access` field while folder rows/favorites still include it
  - folder contents and folder properties default omitted `folder`/`path` query parameters to the Vault root
  - document detail version/event history timestamps serialize with Python-compatible `datetime.isoformat()` shapes
  - folder contents and folder properties serialize document/folder creation timestamps and folder history timestamps with Python-compatible `datetime.isoformat()` shapes
  - folder properties and document detail history payloads treat empty actor/message display strings as absent for Python-compatible fallback behavior
  - document detail history payloads use Python-compatible title-cased event display fallbacks when legacy event timestamps are blank
  - folder contents returns `404` for users without visible folder access
  - folder properties route hides inaccessible descendant folders/documents from counts and stats
  - folder properties patch persists color/icon, emits metadata history, and emits a `folder.properties` state event
  - folder properties patch rejects invalid color/icon values and missing write access
  - folder permissions route requires admin access, returns updated folder properties, emits permissions history, and emits a `folder.permissions` state event
  - folder retention route returns updated folder properties, emits retention history, and emits a document-aware `folder.retention` state event
  - document detail requires read access
  - document rows normalize expiry timestamps with Python-compatible UTC ISO payloads while preserving raw SQLite storage values
  - document detail returns version history and download URLs
  - document detail deduplicates matching version/event history rows using Python-compatible trimmed messages and normalized Unix-second timestamps
  - document detail deduplicates adjacent duplicate version checksums after Python-compatible version-number ordering
  - document detail lock payloads serialize `locked_at` with Python-compatible `datetime.isoformat()` shape
  - folder contents rejects visible documents whose current-version pointer is stale or empty while actual version rows exist
  - document detail returns `403` for visible-only users and `404` for users with no visible access
  - legacy document detail redirects visible users to `/` and hides inaccessible/missing documents as `404`
  - legacy document creation and check-in return `410 Use resumable upload sessions` for authenticated users before write-permission checks while preserving authentication failures
  - resumable upload session creation rejects read-only target folders as `403` without creating upload or document metadata
  - API download streams a single selected document, records download history, emits `document.download`, and rejects visible-only document access without recording events
  - API download reports normalized missing-folder paths in top-level `404` errors
  - export job route returns a queued job for folder selections, then background completion creates a downloadable local ZIP artifact and records per-document export history
  - export job creation counts readable selected documents before current-version filtering, so readable unversioned documents queue jobs and are skipped by the worker
  - folder export selections exclude inaccessible descendant documents while preserving direct unreadable-document failures
  - export job route rejects visible-only selected documents without queueing export work or recording events
  - export ZIP generation deflates known text entries when compression is enabled and keeps known precompressed media stored
  - export artifact downloads emit ZIP filename disposition headers, support byte ranges, and preserve `Content-Encoding: identity` for ranged ZIP responses even when clients advertise gzip
  - export routes use configured runtime TTL and ZIP compression settings for HTTP-created jobs
  - export routes report `finalizing` while completed ZIP artifacts are being promoted into storage
  - export cancellation during artifact promotion leaves no export artifact/blob/location metadata and removes unreferenced promoted ZIP objects
  - export cancellation during large ZIP entry writes is checked between payload chunks and removes partial export temp files
  - expired export artifact downloads return `410 Export expired` and transfer sweep deletes the expired export artifact/blob/location/object state
  - export runtime settings normalize TTL, worker count, compression threshold, and compression level at AppState construction
  - export ZIP writer emits ZIP64 entry metadata and end records when sizes, offsets, or entry counts exceed classic ZIP fields
  - API download multi-selection returns `202` with a queued export job payload that starts with Python-compatible zero totals and later exposes a downloadable ZIP URL
  - API download multi-selection defers current-version metadata inconsistency failures to the background export worker
  - API download folder selections queue with zero totals, then the worker exports only readable descendant documents
  - API download of an empty readable folder returns `202` and completes as an empty ZIP, while explicit export creation keeps rejecting empty selections
  - export job routes hide another user's jobs as `404`, cancel queued owner jobs, and return `409 Export is not complete` for cancelled artifact downloads
  - export job action normalization prunes selected child documents from parent folder selections before computing filename, item count, and persisted request payload
  - lock route returns item-level failed results for read-only users and ok results for writers
  - lock route action normalization prunes selected child documents from parent folder selections so folder-only failures do not also mutate child document locks
  - unlock route releases writer-owned locks and returns `Unlocked`
  - my-edits route requires authentication and returns owned active locks as document rows sorted by path
  - delete-forever route enforces admin-only policy as a top-level `403` and deletes archived documents for admins
  - bootstrap returns auth/runtime metadata, user context, preferences, settings, version, and current folder
  - bootstrap hides inaccessible requested folders as `404`
  - security headers are applied to HTTP responses with CSP nonce and HSTS gated by HTTPS public URL
  - HTTPS-gated security headers honor forwarded proto proxy-chain headers
  - app-shell script nonce matches the Content-Security-Policy nonce
  - security headers can be disabled and custom CSP replaces `{nonce}`
  - root app shell renders the frontend mount point, initial state, appearance override, manifest assets, and ignores folder query parameters
  - share app shell validates share-code syntax and embeds share_code in initial state
  - static asset route serves manifest assets and rejects missing/traversal paths

- `test_appearance_overrides.py`
  - app shell embeds sanitized valid host palette/theme appearance overrides into initial state and frontend dataset wiring
  - app shell ignores invalid host palette/theme appearance headers without reflecting rejected values

- `test_user_preferences.py`
  - preferences API returns default normalized preferences
  - preferences API patch persists canonical favorite IDs and returns enriched favorite rows
  - preferences API patch treats a missing `preferences` field as an empty no-op patch
  - app-shell and frontend preference normalization accept only real boolean values for boolean preferences
  - preferences API patch deduplicates favorites while preserving first occurrence order
  - preferences API patch rejects invalid theme, unknown keys, invalid favorites, and invalid sidebar state without changing existing stored values
  - preferences API resolves favorite folder/document rows from current targets after folder rename and old folder paths stop resolving
  - preferences API refreshes current favorite folder/document locations before access filtering and hides items moved under inaccessible parents
  - bootstrap expands visible folder favorites with path, archive state, and access payload
  - bootstrap expands visible document favorites with path, folder, and access payload
  - bootstrap filters inaccessible and missing favorite targets

- `test_retention_ttl.py`
  - expired archive TTL documents are moved to the flat Archive with origin metadata, clear expiry metadata, emit `retention.expired`, and restore with policy reapplied
  - expired delete TTL documents are deleted directly while active locks cause the document to be skipped without clearing expiry metadata
  - plain folders do not compute delete TTLs for old documents or emit retention state events
  - child folders inherit parent delete TTLs while old documents outside that scope remain untouched
  - moving documents and folders out of delete-TTL scope clears descendant expiry before retention sweep can delete them
  - document rename and check-in refresh delete-TTL expiry from the new modification/version time before retention sweep
  - dev debug `sweep-ttl` returns real document retention sweep results instead of placeholder document lists
  - retention update reapplies existing subtree document expiry and updates inherited folder/document contents payloads
  - retention update clears folder policy and descendant document expiry
  - retention update rejects delete TTL for non-admin users
  - retention update rejects invalid TTL actions, missing days, and out-of-range days without mutating existing policy
  - retention update rejects inaccessible descendants without mutating folder policy or document expiry

- `test_share_links.py`
  - share routes create document and folder links, return internal access mode, and resolve current target location after folder rename
  - share resolution enforces current viewer access and returns `404` for inaccessible users, bad codes, disabled links, and expired links
  - share creation uses configured public URLs and normalizes `file` targets to `document`
  - share creation rejects invalid targets, missing document ids, missing folders, and inaccessible document/folder targets
  - folder share stats exclude inaccessible descendant documents
  - share resolution rechecks stale document access after a document moves under an inaccessible folder
  - share resolution rechecks stale folder access after a parent move
  - deleted document and folder targets cascade share links on freshly bootstrapped databases

- `test_edit_stale_state.py`
  - lock route rejects archived documents with `Restore this file before editing` without creating locks or state/history events
  - unlock route rechecks current folder access after a document moves under a hidden direct ACL and leaves the active lock/release history untouched
  - unlock route rejects non-admin users unlocking another user's lock
  - unlock route rejects unlocked documents without recording a release event
  - my-edits route hides owned locks when the user no longer has current write access

- `test_checkin_stale_state.py`
  - lock route captures active document locks used by later check-in flows
  - checkout route streams the current version, acquires an active document lock, records checkout history, and emits `document.checkout` state
  - checkout route rejects archived documents and documents locked by another user without creating locks or recording checkout/state events

- `test_delete_stale_state.py`
  - delete-forever route rejects direct-archived and folder-archived documents locked by another user without deleting document, version, or lock rows
  - delete-forever route rejects active/non-archived documents with `Move the document to Archive before deleting`
  - delete-forever route rejects folder items with `Delete forever is only available for archived files`
  - delete-forever route rejects documents restored after direct or folder archive without deleting document, version, blob metadata, or local object data

- `test_folder_cycle_state.py`
  - folder path cache, subtree traversal, and document path helpers tolerate corrupt parent cycles without hanging

- `test_folder_stale_state.py` / `test_location_stale_state.py` / `test_name_validation.py`
  - archive route reports stale folder path action items as normalized item-level failures without mutating documents or emitting events
  - folder creation rejects embedded control characters in folder paths
  - folder creation rejects Archive paths
  - upload session creation rejects embedded control characters in file names
  - upload session creation rejects Archive paths for new documents
  - upload session creation sanitizes non-ASCII MIME types to filename-derived safe fallbacks while preserving valid parameterized MIME types
  - upload and download filename-derived MIME fallbacks match Python `mimetypes` behavior for Markdown and unknown `.log` files
  - download responses replace legacy control characters in filenames before emitting `Content-Disposition`
  - download responses emit ASCII fallback names plus RFC 5987 UTF-8 names for Unicode filenames
  - download responses reject malformed/non-ASCII legacy MIME metadata and fall back to safe filename-derived `Content-Type`
  - corrupt download responses fail closed without recording download history or state events
  - move route updates document folders, document history, TTL expiry, and `batch.move` state
  - move route rejects archived documents as Archive moves before creating missing Vault destination folders or recording move/state events
  - move route rejects duplicate document names, documents locked by another user, and direct Archive root moves without mutating source rows
  - move route reports Archive child-folder destinations as Python-compatible item-level failures for archived document moves
  - move route updates folder parents, folder history, descendant TTL expiry, prunes nested action items without document move history, and emits `batch.move` state
  - move route rejects folder descendant destinations before creating missing destination folders or recording move/state events
  - rename route updates document names, document history, TTL expiry, and `batch.rename` state
  - rename route rejects archived documents with `Restore archived files before renaming`
  - rename route rejects documents locked by another user and duplicate document names without mutating source rows
  - rename route updates folder names, folder history, original action item path payloads, and `batch.rename` state
  - rename route rejects root folder rename, duplicate sibling folder names, folder self/cycle moves, and locked descendants without mutating source folders
  - rename route reports invalid optional destination folders as Python-compatible item-level failures for both document and folder items
  - archive route archives documents into Archive, snapshots source ACLs, releases owned locks, writes archive history, and emits `batch.archive` state
  - restore route restores archived documents to their original Vault location, clears archive metadata, writes unarchive history, and emits `batch.restore` state
  - document rows and folder summaries keep version-commit modified timestamps across archive/restore location changes
  - archive route rejects already archived documents and folder subtrees with locks owned by another user without mutating source rows
  - archive route rejects folder subtrees with inaccessible descendants without mutating source rows
  - restore route rejects active documents, missing restore metadata, and duplicate active target names without mutating archived rows
  - archive route archives folder descendant documents, prunes nested selected items, and deletes archived Vault folder placeholders

- `test_archive_folder_placeholders.py`
  - archive route moves documents to the Archive root with original folder/name metadata
  - folder archive flattens descendant documents into the Archive root and removes the source folder tree without creating Archive subfolders
  - Archive allows duplicate archived display names from different original folders
  - Archive folder contents returns archived files without child folders and includes original folder/path metadata
  - Archive folder contents search matches legacy archived rows by original full path when `archived_original_name` is empty
  - restore recreates a missing original folder path from archived metadata and clears archive metadata
  - rename route rejects archived documents without mutating the archived file

- `test_download_stale_state.py` / `test_http_api_contracts.py`
  - current document download enforces read access, streams the current version from local blob storage, honors byte ranges, emits download headers, writes download history, and emits `document.download` state
  - explicit version download uses the version original filename, streams stored bytes, writes version-specific download history, and emits `document.download` state
  - API and explicit-version downloads recheck current folder ACL state after a document moves under a hidden direct ACL
  - download routes return `404` for missing versions and blobs with no storage location

- `test_streaming_transfers.py` / `test_checkin_stale_state.py` / `test_retention_ttl.py`
  - upload size limit is runtime-configurable and rejects oversized sessions before document/blob/session metadata or local objects are written
  - upload session creation keeps Python's strict mode values while preserving the missing-mode default to `create`
  - upload session chunk sizing is runtime-configurable and adapts to file size plus client-reported upload parallelism
  - upload session create mode streams a part into transfer-directory sidecar state, writes no per-part SQLite rows, completes into a content-addressed local multipart blob, creates document/version metadata, cleans transfer files, and serves the completed download
  - upload completion with an expected final checksum promotes verified part files as a local multipart manifest without assembling a second full-size direct blob
  - upload part checksum failure removes temporary part files and leaves document/blob/upload-part metadata untouched
  - upload completion records verification progress and completed upload session status returns processed/total verification bytes
  - upload sessions return signed upload-part tokens, and part uploads accept valid tokens without normal user headers while rejecting invalid or wrong-session tokens
  - upload parts do not require a client checksum header; missing checksums remain `null` in resumable metadata and checksum-bearing duplicates conflict
  - duplicate part uploads with matching checksum metadata are idempotent while conflicting content is rejected
  - concurrent lower-level part promotion cannot overwrite an already-promoted part file
  - upload session resume reports existing sidecar parts and completion succeeds without a final checksum by hashing stored parts
  - upload session check-in mode requires an owned active lock, creates a new version, optionally renames to the uploaded filename with a `document.move` state event before `document.checkin`, releases the lock, emits `document.upload.complete` state, and preserves version counters/current-version metadata
  - upload completion rechecks archived document state after bytes have arrived and rejects stale check-ins without creating a new document version
  - upload abort removes transfer scratch files, clears uploaded part payloads, blocks later completion without creating document/blob metadata, hides sessions from non-owners, and allows admin aborts
  - expired active upload session status requests mark the session expired, remove scratch files, and return `410 Upload session expired`

- `test_create_stale_state.py`
  - upload completion rechecks duplicate destination documents after bytes have arrived and rejects the stale create without creating a second document/version
  - rejected stale upload completion removes unreferenced promoted local objects so storage reconciliation reports no orphan blob metadata or unreferenced local keys

- `test_docker_deploy.py`
  - Rust runtime config uses a single data directory to derive default database, object, and transfer paths
  - explicit database, object, transfer, static, bind, storage, upload-limit, transfer-chunk, transfer-TTL, export-TTL, export-worker, export-ZIP, and site-name settings override defaults
  - unsafe runtime numeric settings normalize to the same Python-compatible lower/upper bounds before app state construction
  - gzip runtime settings normalize to the same Python-compatible lower/upper bounds and drive response middleware
  - legacy `VAULT_LOCAL_OBJECTS_PATH` and `VAULT_FILES_PATH` env vars remain object-path fallbacks behind explicit `VAULT_OBJECTS_PATH`
  - app version is compiled from the repository `VERSION` file
  - production Dockerfile builds frontend assets, compiles the Rust `vault-server`, runs as non-root, preserves `/data`, and does not run Python/Uvicorn
- production compose uses the release image, one `/data` volume, production auth/session defaults, transfer path, configurable session cookie/secret requirements, OIDC state/discovery/timeout/auth endpoint settings, security-header/HSTS/CSP settings, S3/R2 env surface, and no dev auth secret
  - dev compose is the only compose path that enables dev auth
  - generated frontend/Rust build outputs are ignored
  - semver tag workflow builds and publishes the GHCR image without mutable `latest` tags


- `test_event_stream_concurrency.py`
  - event stream replays only rows after `Last-Event-ID` as `event: state` SSE payloads with normalized resources
  - state event writers store Python-compatible sorted/deduplicated resource lists and skip empty resource-only writes
  - event stream starts at the latest state event when no `Last-Event-ID` is provided or the header is invalid, including fresh databases with no state rows, then wakes from the notifier for newly committed state events without replaying old rows
  - ten idle event streams wait for notifications without emitting early or blocking `/health`

- `test_admin_settings.py`
  - admin directory route requires admin access
  - admin directory returns users, groups, memberships, dev mode, effective admin status, and settings
  - admin directory allows configured bootstrap-admin emails and reports effective admin without persisting the stored admin flag
  - admin directory user timestamps serialize with Python-compatible `datetime.isoformat()` shapes for SQLite-naive and RFC3339-aware stored values
  - settings patch route requires admin access
  - settings patch route allows configured bootstrap-admin emails without persisting the stored admin flag
  - settings patch route persists `archivePermanentDeleteAdminOnly`
  - settings patch route emits `admin.settings.updated` state events with `admin` and `settings` resources
  - settings patch route rejects unknown settings and non-boolean setting values
  - archive permanent-delete policy defaults to admin-only
  - relaxed archive permanent-delete policy still requires per-item write access
  - relaxed archive permanent-delete policy allows writers to delete archived documents forever
  - admin user update route toggles admin/active flags and preserves at least one active admin
  - admin user update route allows configured bootstrap-admin emails without persisting the stored admin flag
  - admin group routes create, normalize, rename, delete, and return admin directory payloads
  - admin group routes allow configured bootstrap-admin emails without persisting the stored admin flag
  - admin group membership routes add idempotently, remove, and return admin directory payloads
  - admin group routes emit `admin.group.*` state events
  - admin group routes reject invalid names, duplicate names, folder-permission deletions, and last-admin-breaking group changes
  - last-admin guard normalizes persisted group names before allowing user deactivation, group delete, group rename, or group membership removal
  - admin debug tools return `404` outside dev mode
  - dev mode exposes debug server/bad-request errors and timeout action responses, and timeout-triggered event streams send one retry directive before closing
  - dev debug seed creates a downloadable sample document under `Debug Samples`
  - dev debug emit-state filters invalid resources and records `debug.refresh`
  - dev debug emit-state uses the Python default refresh resources when `resources` is omitted
  - dev debug storage-report and sweep-ttl return expected report/result shapes
  - dev debug reset-database clears data, reseeds roots, and allows dev bootstrap to recover

- `test_acl_permissions.py`
- `test_admin_settings.py`
- `test_appearance_overrides.py`
- `test_archive_folder_placeholders.py`
- `test_auth.py`
- `test_checkin_stale_state.py`
- `test_create_stale_state.py`
- `test_db_init.py`
- `test_delete_stale_state.py`
- `test_docker_deploy.py`
- `test_download_stale_state.py`
- `test_edit_stale_state.py`
- `test_event_stream_concurrency.py`
- `test_folder_cycle_state.py`
- `test_folder_stale_state.py`
- `test_http_api_contracts.py`
- `test_location_stale_state.py`
- `test_name_validation.py`
- `test_retention_ttl.py`
- `test_share_links.py`
- `test_storage_reconciliation.py`
- `test_streaming_transfers.py`
- `test_user_preferences.py`
- `vault/client/tests/transferClient.test.mjs`

## Required Rewrite Order

1. Config, logging, health, SQLite bootstrap, and storage directories.
2. Auth/session foundation: header auth, dev auth, OIDC, cookies, user bootstrap, group membership.
3. Storage foundation: local storage, content-addressed blobs, reconciliation, download headers/ranges.
4. Folder/document read model: roots, paths, ACLs, payloads, bootstrap, sidebar, contents, document detail.
5. Mutating document/folder operations: create folder, move, rename, archive, restore, delete forever, lock, unlock.
6. Uploads: session creation, signed part token, native part ingest, active transfer state, completion, abort, expiry, recovery.
7. Exports: async ZIP jobs, artifact storage, cancellation, TTL sweep.
8. Share links and share bootstrap.
9. Admin settings and debug operations.
10. Frontend static serving and Docker replacement.
11. Benchmarks and production deployment parity.

## Completion Gate

The Rust rewrite is complete only when:

- `cargo fmt --all -- --check` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- `cargo test --workspace` passes.
- `pre-commit run --all-files --config .pre-commit-config.yaml` passes.
- The Docker image runs the Rust service as the primary server.
- The existing frontend works against the Rust service with no Python server.
- Upload/download benchmarks meet or exceed the accepted target for local direct service runs.
