# Vault database migrations

`v2_0_0.rs` is the oldest supported database source. It owns the complete
released v2.0.0 bootstrap contract, including migration-history row 1
(`content previews`), but it is not an executable upgrade migration. Fresh
databases install that exact baseline and then run the same ordered migrations
as an upgraded v2.0.0 database. Nonempty databases without the v2.0.0 ledger
entry are rejected; the runner does not infer or migrate pre-2.0 state.

Each later Rust file owns the complete persisted-state transition required by
its target Vault version. Its descriptor contains the immutable ledger
metadata, apply callback, and target-state validator. Historical modules pin
their input and output values; they must never import mutable current metadata.
Once a migration ships, corrections are added as a new target migration
instead of editing the old file.

`mod.rs` is the only migration registry and runner. It models the immutable
v2.0.0 baseline separately from executable transitions, requires
`schema_migrations` to contain that baseline followed by an exact prefix of the
compiled registry, and records a transition only after it succeeds.
Classification, migration, validation, and ledger updates happen inside one
`BEGIN IMMEDIATE` transaction. Current migration number, dispatch, target
validation, and expected schema prefixes are derived from the baseline and
ordered descriptors.

`v2_1_0.rs` is the first executable transition. It completes the
v2.0.0-to-v2.1.0 persisted-data transition by normalizing the primary root.
The accepted `name='Vault'` input is a narrow compatibility representation
that can exist in a v2.0.0 database carried forward from an earlier install;
it does not make pre-2.0 databases supported migration sources.

Server-owned singleton state such as root folders must not be repaired from
request paths. Fresh bootstrap creates it, versioned migrations normalize it,
and startup/readiness validation rejects missing or malformed state. Reset
uses current root metadata and never calls a historical seeder.

Migration acceptance databases are generated at test runtime by independently
pinned, versioned builders under
`vault/server/tests/support/migration_fixtures`, beginning with the row-1
`v2_0_0` baseline. The released DDL, ledger metadata, and representative data
remain reviewable Rust source; generated SQLite files and blobs never enter
the repository. Tests must cover canonical sources, explicitly supported
representations, applied-history prefixes, rollback, idempotence, and
user-visible route behavior. Every newly supported source prefix must gain its
own builder.

Database migrations are forward-only. After a new ledger row is committed, an
older Vault image may reject the database even when the data change itself is
compatible. Operational rollback therefore means restoring the matching
pre-migration database snapshot together with the prior image.
