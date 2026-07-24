# Performance Testing

Vault's canonical performance-regression harness is the Criterion target in
`vault/server/benches/vault_performance.rs`. It exercises application code and
an in-process Axum router against deterministic SQLite and local-storage
fixtures. Fixture construction is outside the measured intervals.

Run the complete suite from the repository root:

```bash
cargo perf
```

For a fast compile-and-single-pass smoke check without collecting statistics:

```bash
cargo perf -- --test
```

The suite covers these performance-sensitive paths:

- non-admin folder contents with a wide repository and unrelated rows;
- non-admin sidebar construction with a deep folder hierarchy;
- warm header authentication, both sequentially and with eight concurrent requests;
- a 256 MiB in-process document download with the response body drained incrementally;
- a complete 256 MiB upload through real session, part, and completion routes;
- twelve-user concurrent 16 MiB uploads and range downloads through real routes;
- twelve users each uploading and downloading 16 MiB simultaneously;
- a forced-deflate export of a 32 MiB compressible document;
- state-event tail reads after a large event history;
- local-storage reconciliation across referenced, missing, and unreferenced objects;
- direct local-storage reads.

## Comparing a change

Criterion baselines are the source of truth for timing comparisons. Before a
change, save a named baseline:

```bash
cargo perf -- --save-baseline before-fix
```

After the change, compare the same suite against it:

```bash
cargo perf -- --baseline before-fix
```

A fix is not performance-clean if Criterion reports `Performance has regressed`
outside the harness's 5% noise threshold. Rerun a noisy result once with other
heavy work stopped; a repeated regression must be corrected before merging. Do
not compare raw times from different machines as if they were interchangeable.
Criterion reports regressions but does not return a failing process status for
them, so the comparison output must be inspected before accepting a fix.

The Criterion report is written below `target/criterion/`. The regular
repository gate still owns functional tests, linting, and deterministic safety
invariants:

```bash
pre-commit run --all-files --config .pre-commit-config.yaml
```

Timing and throughput do not prove bounded memory, query count, or task growth.
Fixes for streaming, repository-scale queries, and queues must add deterministic
resource-contract tests to the regular gate as well as running this suite.

`extras/bench_transfers.py` remains useful for deployment-scale transfer and
container/topology experiments against the real application routes. It does
not register or depend on a diagnostic sink in the production server. It
complements this harness; it is not the canonical per-change regression suite.

## Real-browser HTTP protocol A/B

The in-process and Python harnesses do not reproduce browser HTTP/2 request
stream scheduling. Any suspected browser/proxy upload defect must be tested
through the deployed TLS-terminating proxy with a real browser before changing
Vault's protocol support.

Use two otherwise identical test origins or test windows: one negotiated as
HTTP/2 and one constrained to HTTP/1.1. Confirm the negotiated protocol in the
browser network panel for every part request. Keep the Firefox build, browser
profile, host, VPN state, link shaping, file, Vault revision, proxy buffering
mode, and upload-session settings fixed. Do not route by user agent; protocol is
the experimental variable. Run at least three fresh sessions per protocol, and
do not reuse a part committed by the other arm.

Capture the following on one synchronized clock:

- a Firefox HTTP log covering request DATA and stream termination;
- a packet capture on the client/proxy path;
- Nginx request and HTTP/2 connection/stream fields;
- Vault's `upload_part_start` and `upload_part_stop` events, including
  `expected_bytes`, `received_bytes`, `duration_ms`, and
  `termination_reason`;
- client-visible transitions through uploading, awaiting acknowledgment,
  stalled, reconciling, and retrying.

Compare completion rate, part duration, received-minus-expected byte boundary,
and termination reason between the two arms. A successful retry does not prove
the original request completed, and XHR upload progress reaching the part size
does not prove that Vault acknowledged it. Repeated HTTP/2-only truncation is
evidence for a transport implementation problem; a shared failure requires
continued investigation above or below that protocol layer. Preserve browser
logs and packet captures as sensitive artifacts because they can include
authentication and network metadata.

## Temporary-data safety

The Criterion fixture never uses the operating system's default temporary
directory. It validates that `target/perf-tmp` is below this workspace's
canonical `target` directory, then asks `tempfile` to create unique children
there. `TempDir` removes only its unique child when the fixture exits normally.
The harness never recursively deletes `target`, `target/perf-tmp`, or guessed
stale paths. A hard-killed run may leave up to three uniquely named fixture
directories totaling roughly 800 MiB. They are left for manual inspection;
cleanup must validate each exact child path before removing it.

Upload request bodies are generated as repeated 256 KiB streaming chunks, so a
256 MiB logical transfer does not require a second 256 MiB request buffer.
Heavy upload iterations build fresh application state and sessions outside the
measured interval, then drop that iteration's unique `TempDir` after timing.
The twelve-user upload and download cases each transfer 192 MiB in aggregate;
the mixed case moves 192 MiB in each direction. Peak temporary application data
stays around 800 MiB or less, including the copy fallback used when hard-link
publication is unavailable. All database, transfer, source, and object files
live inside unique `TempDir` children, so a normal exit leaves no temporary
application fixtures. Cargo build output and Criterion reports intentionally
remain under the repository's `target/` directory on the same drive.
