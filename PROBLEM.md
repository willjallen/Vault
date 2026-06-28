# Upload And Download Throughput Problem

This document describes the actual performance problem we are solving in Vault, what is known from measurements, what the current code does, what is not the main problem, and the concrete steps that should be taken next. The purpose is to keep the work locked on upload and download throughput. It is not a general architecture wish list, and it is not a place to invent adjacent cleanup work unless that work directly improves measured transfer throughput.

## The Problem

Vault must support high-throughput uploads and downloads for real production users behind TLS termination, OIDC, nginx, and Docker. The system also needs resumable uploads because users may have unstable connections. That means the implementation has to split large uploads into parts, retry failed parts, resume interrupted sessions, and complete the file without corrupting canonical database state.

The operational problem is that some users observe upload rates far below their measured available upload bandwidth. Kevin's retained production logs showed uploads around 4 to 5 MB/s for a 114 MB file and about 1.6 MB/s for a 38 MB file. The same deployment path was also able to accept a much larger upload from another user at about 71 MB/s. Kevin's measured internet speed was reported around 20 MB/s. Therefore the problem cannot be dismissed as "the server is globally slow" or "the route is incapable of moving bytes." It is also not acceptable to hand the user homework and ask them to prove their ISP, VPN, browser, or nginx path. The codebase has to provide enough benchmark coverage and telemetry to explain where throughput is being lost.

The local problem is also real. On a local machine, especially on loopback or a 10 GbE-capable host, Vault should not be capped at the same rates as a remote residential path. After the upload hot-path refactor in commit `ab4c3b7`, local Uvicorn benchmarks improved substantially, but local aggregate upload throughput still measured around 300 to 437 MiB/s depending on file size, chunk size, and concurrency. The writev-based worker path moved the best 10-user local upload case to about 506 MiB/s aggregate. The manifest-backed local storage change moved the 128 MiB single-user case to about 393 MiB/s and the 10-user 64 MiB case to about 558 MiB/s at 8 workers. That is better, but still below local download throughput around 1 GiB/s. A 10 GbE link is roughly 1.25 GB/s decimal, or about 1.16 GiB/s before real overhead. Local loopback and local disk paths should be used to isolate software overhead from physical network overhead.

The problem is therefore:

1. Remote users with enough measured bandwidth can still underutilize their uplink.
2. Medium-sized files can create too few concurrent upload streams to hide per-stream limits.
3. The server still touches every uploaded byte in Python's ASGI path before the request can complete.
4. The current implementation still performs hot-path work that may not be necessary for the default frontend path.
5. We need permanent tests, benchmarks, and low-overhead telemetry that identify the limiting stage without adding per-chunk database writes or per-chunk logging.

The goal is not to make the upload implementation abstractly pretty. The goal is to move real bytes faster, prove why they move faster, and prevent regressions.

## What Is Not The Primary Problem

The SQLite issue that caused app wedging was a real production risk. It was caused by high-contention database behavior and was addressed by moving active transfer state out of SQLite and into a filesystem-backed transfer store with an in-process hot cache. That was necessary for correctness and availability. It also removed per-part database writes from the upload path. However, the remaining throughput problem is no longer primarily a SQLite problem. Upload part PUT requests no longer write `UploadPart` rows for every part. The canonical database tracks session lifecycle and final document state; the hot transfer state lives outside SQLite.

The old "assembly copy" issue was not the explanation for Kevin's medium-file underutilization, but it was a measured local completion and contention cost. The previous system wrote part files, assembled them in the background into `assembled.tmp`, then stored the assembled file as the final blob. Vault now verifies contiguous parts in order to compute the canonical SHA-256 and commits local uploads as manifest-backed blobs instead of rewriting a second full file. This reduces local upload wall time and completion tail, but it does not directly explain why Kevin's PUT requests had too few active streams to fill a 20 MB/s uplink. An earlier in-flight ordered assembly experiment was measured and rejected because the extra coordination and flush behavior made multi-user uploads substantially slower.

Postgres is not the answer to this particular problem. Moving canonical metadata from SQLite to Postgres would not make HTTP request bodies arrive faster, would not make XHR send more streams, and would not reduce ASGI body parsing overhead. SQLite can remain appropriate for canonical metadata if the code avoids using it for high-frequency transfer-part state.

Redis is also not required for the current throughput problem. The active transfer store now uses filesystem sidecars for restart safety and an in-process cache for hot state. The immediate bottleneck is not "we lack a key-value store." The immediate bottleneck is how many useful streams we create, how much work we perform per uploaded byte, and how efficiently Uvicorn/ASGI/Python hand those bytes to lower-level hash and file-write operations.

Nginx, VPN, and ISP cannot be used as blanket explanations. They may still appear in a complete benchmark matrix, because some production deployments pass through nginx and TLS termination. They are not a product requirement and they are not the architecture for the upload fix. Vault's self-contained path must remain functional through direct Uvicorn. The finding that one user hit 71 MB/s while another hit 4 to 5 MB/s through the same broad deployment path means we need to understand client/file/concurrency/path interactions rather than treating the entire network path as one opaque cause.

## Known Measurements

The production observations around Kevin's uploads were:

1. `ScoutMaster.plasticity`, 114,004,243 bytes, 4 parts, completed in 23.141 seconds, about 4.93 MB/s.
2. `ScoutMaster.plasticity`, 114,004,243 bytes, 4 parts, completed in 21.911 seconds, about 5.20 MB/s.
3. `BelugaDecimated.fbx`, 38,866,444 bytes, 2 parts, completed in 24.309 seconds, about 1.60 MB/s.

The important pattern was per-stream consistency. Kevin's 32 MiB parts finished around 1.4 to 1.6 MB/s per active HTTP upload stream. With 3 to 4 active parts, aggregate speed landed around 4 to 5 MB/s. With only 2 parts, once the small part finished, only one large stream remained, and aggregate throughput dropped toward one stream's rate.

This is not a complete root cause by itself, but it is already enough to identify one concrete flaw in the current upload strategy: fixed 32 MiB chunks do not guarantee enough active streams for medium-sized files. A 38 MB file produces only two parts. A 114 MB file produces four parts. The frontend allows up to 8 upload workers, but it can only use as many workers as there are parts. Therefore the current default cannot hide per-stream throughput limits for common medium-sized asset files.

The comparison upload from Will was:

1. `ContructorMasterFile.plasticity`, 533,643,336 bytes, completed in 7.517 seconds, about 70.99 MB/s.

That comparison proves the server path can move much faster in at least one real case. It does not prove Kevin's case is outside the app's control, because the app controls part sizing, concurrency, retry behavior, and request hot-path work. It does prove we should not treat the deployment as globally capped at 5 MB/s.

After commit `ab4c3b7`, a local Uvicorn benchmark using `uvloop`, `httptools`, streamed 256 KiB request-body chunks, and the actual app routes measured:

1. 1 user, 128 MiB file, 32 MiB parts, 8 workers: about 299.7 MiB/s upload and 990.5 MiB/s download.
2. 10 users, 64 MiB each, 32 MiB parts, 8 workers: about 437.3 MiB/s upload and 1118.3 MiB/s download.
3. 10 users, 64 MiB each, 4 MiB parts, 8 workers: about 387.4 MiB/s upload and 1087.8 MiB/s download.

Before the final hot-path fixes, representative local numbers were lower:

1. 1 user, 128 MiB, 32 MiB parts: about 223.5 MiB/s.
2. 10 users, 64 MiB each, 32 MiB parts: about 278.2 MiB/s.
3. 10 users, 64 MiB each, 4 MiB parts: about 224.0 MiB/s.

The improvements are material: roughly 34 percent, 57 percent, and 73 percent respectively for those local cases. Against the older DB-heavy path, the 10-user 32 MiB case improved from about 229 MiB/s to 437.3 MiB/s, roughly 91 percent.

The remaining local upload ceiling is now better isolated. A minimal ASGI sink that only receives and counts bytes measured about 1.35 GiB/s for a 128 MiB local upload. The same sink with SHA-256 enabled measured about 0.62 to 0.64 GiB/s. Vault's part-ingress path now lands in the same class as the hashing sink, which means raw Uvicorn/ASGI receive is not the local ceiling. The remaining cost is the durable upload path: queue handoff, worker scheduling, part-file writes, full-file assembly/hash, and completion metadata.

## Current Upload Architecture

The frontend uses resumable upload sessions. Upload fanout is path-aware: low-latency control probes use 8 workers, while high-latency or unknown paths use up to 16 workers. The server returns a session with `chunk_size`, and the client slices the browser `File` object into parts of that size. Each worker sends one `XMLHttpRequest` PUT to `/api/uploads/{session_id}/parts/{part_number}`. The client sends `X-Upload-Offset`, `X-Upload-Size`, and `X-Upload-Token`. The current frontend does not send `X-Upload-Sha256` for each part.

The server maximum transfer chunk size is `VAULT_TRANSFER_CHUNK_BYTES`, defaulting to 32 MiB. Upload session creation now chooses a per-session `chunk_size` under that maximum. `UploadSession` stores the chosen chunk size, and the client already trusts the server's returned `chunk_size`.

The upload part route now avoids database access for normal token-authenticated part PUTs. It verifies the signed upload token against the session id, checks offset and size against the transfer store's session sidecar, checks whether the part already exists, then hands the raw ASGI receive callable to the transfer engine. The route includes a comment explaining why this endpoint bypasses Starlette's `Request.stream()` wrapper: it is the byte-hot path, and ingress should not be serialized behind route-layer hashing, coalescing, or file writes.

The transfer engine receives ASGI body chunks and places byte buffers into a bounded in-memory queue. A worker in a dedicated transfer `ThreadPoolExecutor` reads from that queue, hashes the part only when a part checksum was supplied, batches existing body buffers up to 4 MiB, and writes a temporary part file under the transfer session directory. On POSIX, the worker uses `os.writev()` for those batches so it does not copy every chunk into a Python `bytearray` before writing. The queue backpressure path deliberately uses nonblocking `put_nowait` plus `asyncio.sleep(0)` instead of `asyncio.to_thread(queue.put)`. This avoids executor starvation under many simultaneous parts, where writer workers could occupy executor threads while event-loop tasks waited for executor threads just to enqueue data.

When a part has been spooled, the transfer store atomically promotes the temporary part file into the session's `parts` directory and writes JSON metadata sidecars. This is restart-safe transfer state outside SQLite. The canonical SQLite database still owns the upload session lifecycle and final document/version records.

The transfer engine also schedules background assembly. It appends available contiguous parts into `assembled.tmp`, computes the full-file SHA-256 while appending, and marks the assembly complete when all parts have been written and the total size matches. Completion waits for this assembled file, verifies the digest if an expected digest is supplied, and then stores it through the configured storage backend.

For local storage, `put_file()` usually renames the assembled file into the content-addressed object path. If source and target are on different filesystems, it falls back to copying. Therefore completion is normally cheap after assembly exists. The expensive local work is the initial part write plus background assembly read/write/hash, not a second final copy.

## Current Download Architecture

The client supports streaming downloads and segmented range downloads. `DOWNLOAD_CONCURRENCY` is currently 4. `DOWNLOAD_SEGMENT_BYTES` is 64 MiB. `DOWNLOAD_SEGMENTED_MIN_BYTES` is 128 MiB. That means files below 128 MiB normally use a single response stream, while larger files can be split into parallel range requests. The client buffers downloaded chunks into 32 MiB write batches and applies a 512 MiB write backpressure threshold.

The local benchmark after the upload refactor measured downloads around 990 to 1118 MiB/s, which means the local server download path is already much closer to expected high throughput than upload. That does not prove remote downloads are solved. It does mean the next work should distinguish upload and download bottlenecks. If remote downloads underperform on medium files, the segmented download threshold and range concurrency should be tested the same way upload part sizing is tested. The same "too few active streams" issue can exist on downloads if the client uses a single stream for files below 128 MiB and a user's path has a per-stream ceiling.

## The Most Concrete Current Cause For Kevin's Uploads

The clearest known issue is fixed chunk size producing too few active upload streams for medium files.

For Kevin's 114 MB file with 32 MiB chunks:

```text
part 1: 32 MiB
part 2: 32 MiB
part 3: 32 MiB
part 4: about 13 MiB
```

The browser can use at most four upload workers because there are only four parts. If each stream behaves around 1.5 MB/s, the best aggregate is around 6 MB/s before overhead and tail effects. The observed 4 to 5 MB/s is consistent with that shape.

For Kevin's 38 MB file:

```text
part 1: 32 MiB
part 2: about 5 MiB
```

The small part finishes quickly, leaving one large stream. If that stream is around 1.4 to 1.6 MB/s, the observed aggregate near 1.6 MB/s is expected. This is a direct consequence of the app's fixed part size and fixed worker scheduling. It does not require blaming the VPN or ISP. It also does not require guessing at a mysterious server slowdown.

The concrete correction is adaptive per-session chunk sizing. The first attempt at "always target 8 parts" improved the 38 MB case but still left Kevin's 114 MB shape below his measured uplink when each stream was capped around 1.5 MiB/s. A later 12-worker policy improved the cap-shaped runs but still only reached about 16 MiB/s for the 114 MiB case. The only static policy tested so far that put the 114 MiB cap-shaped upload in the 20 MB/s class was 16 upload workers with up to 16 adaptive parts and roughly 8 MiB target chunks.

```text
if the file is just over the default 32 MiB chunk size:
    target up to 16 parts, bounded by the 4 MiB minimum chunk
else if fixed 32 MiB chunks would produce fewer than 16 parts:
    target roughly 8 MiB parts, clamped to 4-16 parts
else:
    use the configured maximum chunk size
```

With that policy:

```text
38 MB file -> about 4 MB chunks -> 10 parts
64 MB file -> about 8 MB chunks -> 8 parts
114 MB file -> about 8 MB chunks -> 14 parts
large files -> 32 MiB chunks, because they already have enough parts
```

If Kevin's per-stream rate remains around 1.5 MB/s, the 38 MB file moves from about two useful streams to roughly ten useful streams, while the 114 MB file moves from about four useful streams to roughly fourteen useful streams. Those numbers are not promises; they are the direct arithmetic of the observed per-stream rate. The implementation step is concrete, and the benchmark can prove the actual result.

The measured tradeoff is real. Sixteen workers are slower than eight workers on unconstrained local medium-file uploads because they create more HTTP requests and more part files. Sixteen workers are still needed for Kevin-shaped, per-stream-capped paths: the 109 MiB benchmark capped at 1.5 MiB/s per request reaches about 10 MiB/s with 8 workers and about 20 MiB/s with 16 workers. The client therefore uses a low-latency control probe to choose 8 workers on fast local paths and 16 workers on slow or unknown paths.

This is the highest-priority throughput change because it directly addresses the production evidence.

## Hot-Path Work That May Be Unnecessary

The second concrete issue was that the server hashed every upload part during the PUT request even though the default frontend does not send a per-part checksum. That is now conditional: if the client does not provide `X-Upload-Sha256`, the server spools the part without computing a per-part digest. The full-file digest is still computed during assembly for content addressing.

That part hash can help with idempotency if a client retries a part and provides a checksum, or if the server wants to compare an existing part to a later request. But for the default frontend path, the client sends offset, size, and token, not a checksum. The final assembled file still gets hashed to produce the content-addressed blob digest. Therefore the default hot request path should not pay for a full SHA-256 pass per part that does not materially protect the default upload from browser-to-server corruption. TLS and HTTP already protect transport integrity at lower layers. Disk corruption after receipt is not solved by a hash that is stored next to the same part unless that hash is later rechecked and treated as authoritative.

The implemented behavior is:

1. If `X-Upload-Sha256` is supplied, hash the part during ingest and reject mismatches.
2. If `X-Upload-Sha256` is absent, spool the part without computing a part digest.
3. Store `sha256: null` or omit the part checksum in the transfer sidecar.
4. For duplicate PUTs of an existing part without a stored checksum, accept only exact same part number, offset, and size.
5. If a later request supplies a checksum for an existing part whose checksum is unknown, either compute a lazy hash of the stored part or reject the checksum comparison as unavailable. The simpler first implementation should reject conflicting checksum validation rather than silently claiming a comparison was made.
6. Always compute the full-file SHA-256 during assembly or finalization because the content-addressed object key depends on it.

This removes one full SHA-256 pass from the request hot path for the default frontend. Tests cover uploads without checksum headers, checksum mismatch behavior when the header is supplied, duplicate part handling, resume, and final digest correctness.

This is not a guess about where time is going. It is a known current operation that can be removed from the default hot path without weakening the default client validation model, because the default client is not sending an expected part hash.

## Accepted And Rejected Local Experiments

The accepted hot-path change after the adaptive/concurrency work is replacing the worker's Python `bytearray` coalescing with `os.writev()` batching. This keeps durable part files, keeps restart-safe sidecars, and removes one avoidable Python memory copy before the filesystem write.

Representative accepted writev measurements:

```text
single-128m, 8 workers:
  upload 325.3 MiB/s, part ingress 662.7 MiB/s, complete 0.128s, download 992.9 MiB/s

kevin-114m, 8 workers:
  upload 300.4 MiB/s, part ingress 657.6 MiB/s, complete 0.120s, download 935.5 MiB/s

ten-64m, 8 workers:
  upload 506.4 MiB/s, part ingress 627.5 MiB/s, complete 0.483s, download 1204.6 MiB/s

kevin-114m, 16 workers, 1.5 MiB/s/request cap:
  upload 19.8 MiB/s, part ingress 20.2 MiB/s, complete 0.029s
```

The minimal sink measurements are also important:

```text
sink receive-only, single-128m:
  about 1.35 GiB/s

sink receive + SHA-256, single-128m:
  about 0.62 to 0.64 GiB/s
```

This shows raw ASGI receive can exceed local download speed on this host when it is not hashing or writing. The remaining Vault upload gap is therefore in the durable processing path, not in Uvicorn's ability to receive bytes.

Rejected experiments:

1. Direct event-loop receive-to-file for no-checksum uploads. It removed the queue/thread handoff but measured worse: single-128m fell to about 257 MiB/s at 8 workers, and ten-64m fell to about 355 MiB/s. The synchronous file writes blocked the receive loop enough to lose more than the removed handoff gained.
2. In-flight ordered assembly while parts were still being written. It passed focused correctness tests after a promotion race fix, but it measured much worse under load: ten-64m at 8 workers fell to about 246 MiB/s with 1.4 seconds in completion, and ten-64m at 16 workers fell to about 122 MiB/s with 3.6 seconds in completion. The coordination and flush overhead cost more than the theoretical overlap saved.
3. Direct staged range writes into the final blob path. This was measured earlier and rejected because it slowed local part ingress under concurrent load.

These rejected results matter because they close off plausible but wrong directions. The next improvement has to reduce durable write/hash/assembly work without adding event-loop blocking, extra flush pressure, or fragile in-flight coordination.

## Remaining Measured Bottleneck Candidates

After the current refactor, local upload is still below local download. The remaining candidates are concrete stages, not vague theories:

Current Rust local-direct benchmark evidence after the upload hot-path cleanup:

```text
single-128m: upload about 503 MiB/s, download about 1032 MiB/s
ten-64m: upload about 647 MiB/s, download about 996 MiB/s
ten-64m-4m-parts: upload about 209 to 270 MiB/s, download about 849 to 1033 MiB/s depending on the writer experiment
rust-sink ten-64m-4m-parts receive+write: about 559 to 602 MiB/s
```

The 4 MiB part case is still the failing benchmark target. Removing per-part SQLite writes, no-checksum JSON sidecars, per-part token response signing, disabled-security nonce generation, and generic request tracing did not clear that case. The Rust in-process sink proves Axum/hyper can receive and write the same high-fanout request shape at roughly 560 to 600 MiB/s, so the remaining gap is inside upload session handling, part validation/promotion, and storage-session bookkeeping rather than the Rust HTTP stack itself. Reducing client workers to one per user measured about 618 MiB/s for the same 4 MiB workload, so high fanout still triggers app-specific contention that the sink route does not have.

1. Each HTTP body chunk is received by the Rust service and handed to the part writer path.
2. Queue backpressure can yield the route task if workers cannot drain quickly enough.
3. Worker threads hash bytes when part hashing is enabled.
4. Worker threads batch and write part files to the filesystem.
5. Background assembly reads part files, hashes the full file, and writes `assembled.tmp`.
6. Completion waits for assembly if the user completes before assembly catches up.
7. The frontend creates only as many simultaneous PUTs as the chosen part count permits.
8. High fanout still creates many concurrent body-receive and durable-write tasks inside one service process.

Each item can be measured. None should be assumed as the dominant limiter without numbers.

## Permanent Benchmark Harness

The ad hoc benchmark has been turned into `scripts/bench_transfers.py`. It starts a temporary local server with a temporary data directory. App benchmarks default to the Rust `vault-server` binary, using `target/release/vault-server` when it exists and otherwise falling back to `cargo run --release -p vault-server`. The legacy Python/Uvicorn app remains available with `--server python`, and the minimal Python ASGI sink remains available for receive-ceiling benchmarks.

The script supports:

1. Direct Rust app benchmark.
2. Legacy Python/Uvicorn app benchmark for comparison.
3. Minimal Python ASGI and Rust in-process sink benchmarks with checksum enabled or disabled, and optional write-to-file mode for receive+write or receive+hash+write measurements.
4. Upload matrix by file size, chunk size, upload concurrency, body chunk size, and optional per-request rate cap.
5. Download measurement after app uploads.
6. JSON output for trend comparison.
7. Human-readable summary output for quick local runs.
8. Docker-container Rust app benchmark using the production image runtime contract.

Current baseline cases include:

```text
upload:
  1 user, 38 MiB file
  1 user, 109 MiB Kevin-shaped file
  1 user, 128 MiB file
  10 users, 64 MiB each
  10 users, 64 MiB each with 4 MiB parts
```

The benchmark reports:

1. Wall time.
2. Aggregate MiB/s.
3. Per-part min, p50, p95, and max.
4. Completion wait time after last part PUT.
5. Download wall time and aggregate MiB/s for completed uploads.
6. Server CPU seconds, current RSS, and peak RSS when the host exposes process metrics.

The missing harness work is nginx/TLS-topology benchmarking. Server CPU/RSS capture, container-mode Rust benchmarking, Python and Rust receive/hash/write sink variants, and explicit throughput thresholds are now built into the script, so release or CI runs can fail instead of relying on after-the-fact interpretation and benchmark runs have enough resource context to distinguish CPU pressure from request fanout or storage behavior. The current script is already enough to reject bad hot-path ideas and prove medium-file fanout behavior.

## Low-Overhead Telemetry

Telemetry should be added at transfer-session and part-request granularity, not per chunk and not as database writes during the hot path. The system needs enough detail to answer "where did the time go?" without creating the same high-contention problem that caused earlier SQLite failure.

For each upload part, collect in memory and log once at request completion:

1. `session_id`
2. `part_number`
3. `expected_size`
4. `received_size`
5. `request_wall_ms`
6. `first_body_byte_ms`
7. `last_body_byte_ms`
8. `queue_wait_ms_total`
9. `worker_wall_ms`
10. `hash_ms` if measurable separately
11. `write_ms` if measurable separately
12. `response_status`

For each upload session, log once at completion:

1. file size
2. part count
3. chunk size
4. upload concurrency reported by client if included
5. first part start
6. last part finish
7. assembly finish
8. completion finish
9. aggregate upload MiB/s
10. completion wait ms

These logs should be structured JSON lines or a compact key-value log, not freeform text. They should not write to SQLite. They should be controlled by an environment variable such as `VAULT_TRANSFER_TELEMETRY=1` and safe to enable in production during a repro window.

The point of this telemetry is to separate:

1. client-to-server ingress time
2. server body receive overhead
3. queue backpressure
4. hashing/write time
5. assembly/completion wait

If `request_wall_ms` is high but worker write time is low, the issue is ingress or ASGI receive. If queue wait is high, the worker side is behind. If worker write time is high, hash or disk is the limiter. If completion wait is high but PUTs are fast, assembly is behind. This is how we stop guessing.

## Concrete Improvement Plan

### Step 1: Commit the benchmark harness

Status: implemented.

Acceptance:

1. The script reproduces the existing post-refactor baseline within reasonable variance.
2. The script can run direct Uvicorn app mode.
3. The script can run a minimal ASGI receive sink mode.
4. The script records enough data to compare chunk/concurrency changes.

### Step 2: Add adaptive upload chunk sizing

Status: implemented.

Current policy:

```text
min_chunk = 4 MiB
max_chunk = configured VAULT_TRANSFER_CHUNK_BYTES, default 32 MiB
small_adaptive_max = 48 MiB
target_adaptive_chunk = 8 MiB

if fixed max_chunk would create at least 16 parts:
    chunk_size = max_chunk
else if size_bytes <= small_adaptive_max:
    target_parts = 16
else:
    target_parts = ceil(size_bytes / target_adaptive_chunk)
    target_parts = clamp(target_parts, min=4, max=16)

chunk_size = ceil(size_bytes / target_parts)
chunk_size = round_up(chunk_size, 1 MiB)
chunk_size = clamp(chunk_size, min_chunk, max_chunk)
```

For very small files, the chunk size can be smaller than 4 MiB if the file itself is smaller. The point is not to create unnecessary tiny requests; the point is to avoid one or two streams for medium files without turning every medium upload into the maximum possible request count.

Acceptance:

1. A 38 MB file creates about 10 parts instead of 2.
2. A 114 MB file creates about 14 parts instead of 4.
3. Large files still use 32 MiB parts by default.
4. Existing resume tests pass.
5. Benchmark proves whether the Kevin-shaped cases improve.

### Step 3: Test upload concurrency 8, 12, and 16

Status: implemented as adaptive 8-or-16 client fanout. The client uses 8 workers for low-latency paths and 16 workers for slow or unknown paths. The 16-worker path is needed for the 109 MiB, 1.5 MiB/s-per-stream cap-shaped benchmark to reach the 20 MB/s class.

Acceptance:

1. 38 MB and 114 MB medium-file cases improve compared to fixed 32 MiB chunks.
2. 10-user local benchmark does not collapse under higher concurrency.
3. Error/retry behavior remains correct.
4. Browser behavior is verified with real browser tests if possible, because XHR/fetch connection behavior can differ from Python `httpx`.

### Step 4: Make part hashing conditional

Status: implemented.

Acceptance:

1. Default frontend upload path avoids per-part SHA-256 work.
2. Uploads with explicit part checksum still validate and reject mismatches.
3. Resume and duplicate part handling remain deterministic.
4. Benchmarks show the effect on upload PUT throughput and CPU.

### Step 5: Add low-overhead transfer telemetry

Status: not implemented in this branch.

Add optional per-part and per-session summary logs. Do not write hot telemetry to SQLite. Do not log every body chunk. Do not log sensitive filenames unless needed and explicitly enabled.

Acceptance:

1. A single upload session log can explain part count, chunk size, start, finish, and completion wait.
2. A single part log can explain request receive time, queue wait, and worker time.
3. Telemetry can be enabled in production without creating high write contention.
4. Telemetry is covered by tests for shape and disabled-by-default behavior.

### Step 6: Benchmark minimal ASGI receive ceiling

Status: partially implemented. The benchmark has a minimal sink that can receive with SHA-256 enabled or disabled.

Implemented variants:

1. receive and discard
2. receive and hash
3. full Vault route

Missing variants:

1. receive and write to temp file
2. receive, hash, and write

Acceptance:

1. We know the maximum Uvicorn/ASGI receive rate on the local machine.
2. We know how much hashing costs.
3. We know how much file writing costs.
4. We know how much Vault route/store overhead adds over the minimal case.

If the minimal ASGI receive route is already far below the target, then the ceiling is Uvicorn/ASGI/process level, and optimizing transfer store details will not solve it. If minimal receive is much faster than Vault route, then Vault hot-path code still has removable overhead.

### Step 7: Benchmark download segmentation thresholds

Use the same benchmark harness to test downloads below and above 128 MiB. If remote-shaped or throttled-stream tests show one stream underutilizes available bandwidth, lower the segmented download threshold and test more range workers.

Acceptance:

1. Download throughput is measured separately from upload.
2. Medium files can use segmented downloads if that improves throughput.
3. Browser file writing remains correct with out-of-order range writes.
4. Backpressure prevents unbounded memory growth.

### Step 8: Only then consider process-level scaling

If benchmark data proves one Uvicorn process is the ceiling, evaluate multiple workers. This cannot be done casually because the transfer engine has in-process assembly state. Multiple workers would require either session-sticky routing or stronger cross-process coordination over the filesystem sidecars.

Concrete options:

1. Keep one worker and optimize hot-path CPU/copy overhead.
2. Add multiple workers with nginx sticky routing by upload session path.
3. Make transfer assembly fully cross-process safe using file locks and sidecar state.
4. Split upload ingest into a separate internal service only if self-contained deployment requirements still allow it.

Acceptance:

1. No multi-worker change is accepted until a benchmark proves single-process ASGI is the limiting factor.
2. Any multi-worker design must preserve resumability, completion correctness, and duplicate part conflict behavior.
3. Tests must cover concurrent uploads where different parts could hit different workers if sticky routing is absent or broken.

## Initial Performance Targets

The benchmark work needs numeric targets, otherwise every run can be interpreted after the fact. The first target set should be conservative enough to be achievable by this application stack and strict enough to catch regressions.

For local direct Uvicorn, after adaptive chunking, conditional part hashing, and writev batching, target:

1. Single 128 MiB upload at or above 400 MiB/s.
2. Ten concurrent 64 MiB uploads at or above 500 MiB/s aggregate.
3. Ten concurrent 64 MiB uploads with 4 MiB chunks at or above 450 MiB/s aggregate.
4. Single 114 MB Kevin-shaped upload materially above the fixed-32-MiB baseline.
5. Single 38 MB Kevin-shaped upload materially above the fixed-32-MiB baseline.

Those numbers are not the final ambition. They are the next ratchet. The ten-user target is now met in the best 8-worker local run. The single 128 MiB upload target is not consistently met yet. The receive-only sink proves Uvicorn can receive much faster than the current Vault route, so the remaining work is durable-path overhead, not raw ASGI receive.

For local downloads, target:

1. Single 128 MiB download near the current 1 GiB/s range unless system load explains variance.
2. Ten concurrent 64 MiB downloads near the current 1 GiB/s aggregate range.
3. No regression from lowering the segmented download threshold if that change is made.

For production telemetry during slow-user investigation, target:

1. Every completed upload session can answer how many parts were available and how many ran concurrently.
2. Every slow upload can be classified as limited before app receive, inside app receive, in queue backpressure, in worker hash/write, or in completion wait.
3. The classification must come from our logs and benchmarks, not from user-provided manual experiments.

## What We Should Not Do Next

Do not repeat the rejected assembly-copy fixes. A direct staged-file experiment that wrote part ranges into the final staged blob was measured and rejected because it slowed local part ingress under concurrent load. An in-flight ordered assembly experiment was also rejected because it badly hurt multi-user completion time. Durable write amplification is still a remaining local throughput target, but the next design has to reduce it without event-loop blocking, extra flush pressure, or fragile in-flight coordination.

Do not add per-chunk database telemetry. That repeats the earlier design error.

Do not move to Postgres to solve upload throughput. It is the wrong layer.

Do not add Redis just to improve upload throughput. A separate store may be useful for other reasons later, but the current highest-value fixes are chunk/concurrency policy, hot-path checksum removal, and measurement.

Do not assume 32 MiB chunks are universally good. They are good for reducing request overhead on large files. They are bad for medium files when the path has a per-stream ceiling.

Do not assume a single local benchmark proves production. It proves a local ceiling and catches regressions. Production-like nginx/TLS/container benchmarks still need to exist.

## Definition Of Done

This problem is not solved by one more refactor. It is solved when:

1. The repo has a repeatable benchmark harness for uploads and downloads.
2. The benchmark includes Kevin-shaped medium-file cases.
3. The app uses adaptive upload chunk sizing so medium files can fill available concurrency.
4. The default upload path does not perform unnecessary per-part hashing.
5. Transfer telemetry can explain where time went for a slow upload without adding database contention.
6. Local direct Rust upload throughput is stable and regression-tested.
7. Download throughput is separately measured and range thresholds are tuned based on data.
8. Any remaining gap to local hardware capability is tied to a measured stage, such as ASGI receive ceiling, hashing, file writes, or process-level CPU saturation.

The immediate next implementation should be:

1. Add disabled-by-default per-session and per-part summary telemetry.
2. Add sink variants for receive+write and receive+hash+write so the benchmark can isolate filesystem write cost without Vault state. Status: implemented with `--sink-write`, combined with checksum on/off.
3. Investigate a durable-path redesign that reduces part-file plus assembly write amplification without event-loop blocking or in-flight flush coordination.
4. Add nginx/container benchmark mode so local direct-Uvicorn results can be compared to the deployed topology.

Those steps directly address the remaining measured problem. They do not require speculation about the user's ISP, and they do not chase storage cleanup that only indirectly affects throughput.
