# ADR-0005: Centralize the Feed Subscription lifecycle

- Status: Accepted
- Date: 2026-08-25
- Decision owners: project maintainer and Codex implementation session

## Context

Adding, deleting, enabling, disabling, and changing a Feed interval used to be assembled independently by the GUI and CLI. Each adapter opened SQLite directly, applied part of a change, and sometimes started RSS refresh itself. This made the visible outcome depend on the caller: duplicate URLs, maintenance timing, initial-refresh failure, and a delete racing a refresh commit did not have one authoritative meaning. It also left low-level Feed mutation methods available as an accidental second write interface.

The RSS Refresh Workflow in ADR-0004 deliberately owns network execution and per-Feed result commits. It should not also own the user's durable decision to follow or remove a Feed. A separate lifecycle boundary is needed to order the durable subscription change and the optional refresh intent without exposing either subsystem to GUI widgets or CLI command handlers.

## Decision

Introduce a `Feed Subscription Lifecycle` module as the only application-level write seam for durable Feed Subscription changes.

- The external change vocabulary is `Add`, `Delete`, `Enable`, `Disable`, and `SetInterval`. The same module also provides subscription `List` and `Get` queries so adapters do not need to pair a write with a separate database lookup.
- Outcomes are explicit. Durable changes report `Created`, `Existing`, `Changed`, `Unchanged`, `Deleted`, or `NotFound`. Optional initial refresh reports `Queued`, `Succeeded`, `Degraded`, or `Deferred`.
- Feed URLs are trimmed, parsed, restricted to HTTP(S), and serialized through the URL parser. Query strings, paths, and trailing-slash meaning are preserved. Local HTTP(S) Feed URLs remain allowed. Adding the same normalized URL is idempotent and expresses a new refresh intent rather than creating another row. A compatibility lookup recognizes legacy rows stored before normalization.
- Persistence precedes refresh dispatch. A first-refresh HTTP, timeout, or parse failure keeps the Subscription and is a typed `Degraded` outcome. If maintenance begins after the durable change but before GUI dispatch, the outcome is `Deferred`; the Feed remains due for refresh. Infrastructure failure to dispatch is a typed technical error whose user message states that the change was already saved.
- `Enable` clears durable failure state, makes the Feed due, and requests one refresh. `Disable` is reversible and only prevents future selection; an already active request may finish. `SetInterval` requires a positive duration, recomputes `next_fetch` from the latest successful fetch (or current time), and does not itself fetch.
- `Delete` permanently removes the Feed and its local Articles through the existing foreign-key cascade. It is idempotent at the external seam. If a Feed is deleted while a refresh result is in flight, deletion wins: the RSS workflow reports the per-Feed terminal state `Removed`, discards the late result, does not record a failure, and never recreates the Feed.
- Data Maintenance is not another lifecycle participant and there is no subscription-change queue. Changes presented while maintenance is already active fail immediately as `Maintenance`. Existing generation fencing still protects a race in which maintenance begins after a change starts.
- The module keeps SQLite concrete. Tests use temporary real SQLite databases instead of a repository abstraction. Refresh is the only replaceable internal seam, with two production adapters: a GUI session adapter that returns after queueing and a CLI one-shot adapter that waits for the terminal Run.
- GUI and CLI no longer call low-level Feed mutation methods. The GUI keeps Add and Delete as modal operations and edits enabled state and interval in a right-side Feed Settings panel owned by the selected Feed Route. The CLI preserves its public syntax and text-oriented output, waiting for initial or enable refresh and keeping exit codes `0` success, `2` degraded, and `1` technical failure.

## Error boundary

The lifecycle classifies technical errors as `Input`, `Maintenance`, `Storage`, or `RefreshDispatch`. User messages are concise; flattened technical detail is logged or returned to the CLI and is capped and minimally redacted. Network and Feed-format failures are valid refresh outcomes rather than lifecycle failures once their durable Feed backoff state has committed.

## Persistence and migration

This decision adds no table and requires no schema migration. Existing `feeds` and `articles` rows retain their meaning. URL normalization applies at the lifecycle seam, while a raw compatibility lookup prevents an existing pre-normalization URL from being duplicated.

## Consequences

GUI and CLI now agree on idempotence, maintenance behavior, first-refresh failure, interval scheduling, and deletion. The application has one place to test user-visible subscription semantics, and low-level database writes are internal implementation details instead of a parallel public API. Feed Settings participates in the existing Route/Panel reducer and unsaved-change guard rather than introducing another window state.

The cost is a dedicated composition module and typed outcomes in adapters. A GUI session can only promise that refresh was queued; the terminal result remains observable through the RSS Refresh Workflow. Multi-field GUI saves are a short ordered sequence of lifecycle changes, so a later failure can leave an earlier valid change committed; no transaction is claimed across network dispatch.

## Rejected alternatives

- Keep direct database calls in GUI and CLI: fewer types, but preserves divergent ordering and error semantics.
- Move subscription ownership into RSS Refresh Workflow: combines durable user intent with session-bound network execution and makes both modules shallower.
- Add a generic repository abstraction over SQLite: expands the interface without replacing a volatile dependency or improving the real migration/constraint tests.
- Persist subscription-change jobs: creates recovery and cancellation semantics for fast local settings that users did not request.
- Cancel an active HTTP request on Disable or Delete: Disable only governs future selection, while Delete is safely enforced at commit time; cancellation would add timing-dependent semantics without improving data integrity.

