# ADR-0011: Establish Versioned Schema Evolution

- Status: Accepted
- Date: 2026-08-26
- Decision owners: project maintainer and Codex implementation session

## Context

Schema creation and migration previously had two competing authorities. `db.rs` first executed a latest-schema `CREATE ... IF NOT EXISTS` batch and then ran numbered patches. On an older database, the latest batch could create an index against a column that its migration had not added yet. The startup failure around `resources.curation_state` was one concrete result. Domain modules also chained their version constants, tests commonly built a supposed historical database from the latest schema, and a database whose declared version disagreed with its real indexes could be repaired accidentally while opening.

Every public release from v0.1.0 through v0.5.0 wrote `PRAGMA user_version=0`, despite shipping at least four distinct unversioned schema shapes. Compatibility therefore cannot be derived from the integer alone. The application needs one authority that recognises these released shapes, advances them deterministically, rejects drift and newer databases without mutation, and leaves a useful rollback point before automatic startup evolution.

## Decision

Introduce `schema_evolution` as the sole production authority for creating, classifying, and advancing the local library schema.

- The current schema version is an explicit central value, currently 7. The ordered transition table is explicit: v0→v1 foundation, v1→v2 Knowledge Processing tasks, v2→v3 executor fencing, v3→v4 Resource curation and health, v4→v5 unified Library Search, v5→v6 Web Clipping provenance, and v6→v7 Excerpt identity.
- A new database starts at the oldest supported v0 core and replays every transition. There is no separate latest-schema bootstrap path.
- The read-only `inspect` capability classifies a connection as Uninitialized, Ready, NeedsEvolution, Drifted, or NewerUnsupported. Inspection never writes.
- The `evolve` capability accepts only Uninitialized, Ready, or supported NeedsEvolution states and returns a bounded report containing initial readiness, target, completed transitions, final readiness, and verification summary.
- Each immediate transition runs in its own SQLite transaction. Its postconditions and `foreign_key_check` pass before `user_version` advances and the transaction commits. A later failure leaves the last completed version durable and identifiable.
- Final evolution runs `integrity_check` and re-inspects the declared current version. Normal `Db::open` returns only a Ready database.
- Drift and newer unsupported versions fail closed. Opening never treats a missing table, column, trigger, or required derived index as an invitation for ad-hoc repair, and never downgrades a newer database.
- Automatic startup evolution runs through the existing cross-process Data Maintenance Window. It waits for writers, rotates the writer generation, creates and records a plain SQLite safety copy, and only then performs schema writes.
- Domain modules continue to own the SQLite details of their transitions and postconditions where that knowledge belongs: Library Search v5, Web Clipping v6, and Excerpt & Thought v7. They do not decide ordering, transactions, readiness, version advancement, or startup policy.
- Restore paths and maintenance-only opens use the same evolution authority. The old `db.rs` latest-schema constant and duplicate migration functions are removed.

## Released compatibility baseline

Independent SQL fixtures are retained for v0.1.0, v0.1.1, v0.2.0, v0.2.1, v0.3.0, v0.4.0, and v0.5.0. They are extracted from the tagged releases rather than generated from current schema code. Each fixture contains representative Feed, Article, Excerpt/Thought, and—where the release supported it—Resource data.

All seven fixtures must evolve from their original unversioned form to v7, preserve representative data, rebuild derived search state, and finish Ready. A current-version drift fixture and a newer-version fixture must remain unchanged after rejection. A failed transition must not advance its version.

## Module seam and ownership

`src/schema_evolution.rs` owns readiness, the current version, transition ordering, per-transition transactions, postcondition orchestration, foreign-key and integrity verification, typed failures, and the evolution report. `Db` owns normal data access but cannot create or patch schema objects directly. `local_data_maintenance` owns cross-process exclusion, safety-copy persistence, and writer-generation fencing.

Version-specific transition functions in domain modules are `pub(crate)` seams called only by Schema Evolution. Their corresponding verifier checks the authoritative and derived structures owned by that domain. Test-only intermediate-version construction also lives behind Schema Evolution so tests cannot revive a production bootstrap shortcut.

## Failure and observability contract

An evolution failure records a bounded kind, stage, optional `(from, to)` transition, last complete version, user-facing message, and technical detail. It never stores credentials or library content. Relevant stages are inspection, bootstrap, applying a transition, verifying a transition, and final library verification.

The maintenance sidecar records the active Schema Migration run and safety-copy path. Failure to create the safety copy aborts before schema mutation and is classified separately from transition storage or validation failure.

## Consequences

Startup can no longer install a future index before its prerequisite column, current and restored databases follow the same gate, and released databases are covered by real historical fixtures. Partial progress across versions is resumable because every completed version is valid and committed independently.

The trade-off is stricter opening behavior: manually modified or incompletely restored databases that were previously patched opportunistically now require an explicit recovery decision. Adding a schema version also requires a transition, verifier, fixture coverage, and an update to the central ordered list.

## Rejected alternatives

- Keep the latest `CREATE IF NOT EXISTS` batch before migrations: preserves the ordering bug and two schema authorities.
- Build new databases directly at the latest schema: creates a path that historical upgrades never exercise.
- Treat all `user_version=0` databases as one exact shape: would reject or corrupt released v0.1–v0.5 libraries.
- Advance `user_version` before validation: can mark a partial transition complete.
- Repair drift automatically during open: hides structural damage and performs unexpected writes without an explicit recovery boundary.
- Put every transition implementation in one giant module: centralises domain knowledge unnecessarily; ordering and policy are central, transition details remain with their owning domain.
