# Coordinate destructive local-data maintenance outside the primary database

Shiyue now treats restore, VACUUM, and schema migration as an exclusive **Data Maintenance Window** shared by every compatible GUI and CLI process. Coordination lives in a versioned SQLite sidecar beside the library plus OS file locks, because restore can replace the primary database and therefore the primary database cannot be the coordination authority.

Each normal `Db` connection holds a shared writer lease for its lifetime. Starting maintenance first acquires the single-owner lock and publishes `active=true`, which rejects new connections and write intents while keeping the current epoch valid during the drain period. RSS Refresh (ADR-0004) and Knowledge Processing each own a `MaintenanceParticipant` adapter that receives explicit quiesce/resume commands. They stop claiming work, release their connections, and acknowledge the shared deadline only after reaching a safe point; GUI connections in every process close as they observe the sidecar. After all shared leases drain (maximum two seconds), maintenance acquires the exclusive writer lock and only then rotates the opaque epoch. This ordering lets an aborted pre-exclusive run resume existing connections, while any result created before a completed restore is fenced from the replacement database.

Knowledge Processing does not require provider calls to be cancellable. Work that finishes inside the quiesce deadline may commit under its existing executor fence. At the deadline, remaining Attempts become `Interrupted`, the executor lease is released, and detached late results are rejected by the lease generation and database epoch fences. Queued Tasks remain queued; maintenance-time Task and connection-test requests are rejected rather than replayed across library generations. Resume confirms that the workflow can accept commands and compete for ownership, not that it has acquired the executor lease.

The GUI submits only `Restore` or `Compact` intent to the maintenance module. The module owns safety backup, operation, integrity validation, reopen, participant resume, typed terminal status, and technical detail. The UI stays responsive on a dedicated maintenance view, disables all new database reads and mutations, does not queue user actions, and automatically reopens the restored generation. The CLI cooperates by avoiding database leases while RSS or AI network requests are in flight. Schema creation and migration use the same exclusive generation boundary at process startup.

An interrupted run is recovered before any long-lived connection opens. A valid database is retained; an invalid database is replaced from the persisted plain safety-copy path and revalidated. The restored database is the sole truth: writes and queued work created after the selected backup are not replayed. Participant resume is retried finitely and produces `Degraded` rather than false success. Unknown newer sidecar protocol versions fail closed, and unknown/legacy SQLite clients are not forcibly controlled—if they prevent safe exclusivity, maintenance fails without forcing the operation.

## Consequences

Destructive maintenance no longer relies on GUI booleans, leaked atomic pause state, or caller discipline; cross-process writers cannot silently race restore, and stale RSS/AI results cannot commit into a new library generation. The cost is a persistent sidecar and lock files, bounded observation latency for participants that must notice another process, temporary write unavailability, interrupted long-running AI Attempts at the deadline, and explicit quiesce/resume behavior in long-lived background modules.

## Rejected alternatives

- A flag inside the primary database cannot survive or safely coordinate replacement of that database.
- Per-method voluntary checks leave direct SQL and future write paths unprotected.
- Force-closing another process risks corruption and violates ownership boundaries.
- Queuing writes during restore would require replay semantics across two incompatible database histories.
