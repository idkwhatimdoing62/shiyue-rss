# ADR-0004: Centralize RSS refresh as a session-bound workflow

- Status: Accepted
- Date: 2026-08-25
- Decision owners: project maintainer and Codex implementation session

## Context

RSS refresh behavior was split across a GUI-owned scheduler, reusable functions in `daemon.rs`, and CLI-specific composition. The GUI shared `busy`, `dirty`, maintenance, and paused atomics with the scheduler, polled maintenance every 100 ms while HTTP requests were active, and treated a dropped future as its cancellation protocol. CLI `add` and `update` independently opened the database, constructed an HTTP client, selected feeds, and called the detached fetch primitive. This exposed concurrency, database-generation fencing, refresh selection, and maintenance behavior to every adapter and made partial failure and pending intent impossible to observe consistently.

RSS refresh needs one lifecycle without becoming another persistent job system. Articles, each Feed's next refresh time, retry count, disabled state, and latest error are already durable. A half-finished aggregate run has no useful recovery identity after process exit.

## Decision

Introduce a single `RSS Refresh Workflow` module used by GUI, CLI `update`, and the immediate refresh after adding a subscription.

- An `RSS Refresh Run` is session-bound. It has a process-local monotonic `RunId`, a determined immutable target set, meaningful `Fetching`, `Committing`, and terminal states, exact counts, and per-Feed outcomes. Only the active Run and the latest terminal Run are retained in memory.
- Automatic refresh selects enabled Feeds due at Run start. GUI refresh and CLI `update` select every enabled Feed regardless of `next_fetch`. The Feed Subscription Lifecycle (ADR-0005) targets one enabled Feed after Add or Enable. Disabled, deleted, and local web-clipping storage Feeds are excluded before a Run begins.
- The active target set never changes. Concurrent intent already covered by that Run is absorbed; missing targets merge into at most one pending set. The pending set is reread against current Feed state immediately before the next Run. There is no unbounded request queue.
- A Run fetches at most eight Feeds concurrently and attempts each Feed once. Existing per-Feed `record_failure`, exponential backoff, `next_fetch`, failure count, and automatic-disable rules remain the only retry policy.
- Every Feed result is committed independently. Successful commits survive another Feed's failure. Any HTTP or parse failure whose Feed backoff state commits yields `Degraded`, including when every Feed fetch fails. `Failed` is reserved for infrastructure faults that prevent outcomes from being processed safely. No fabricated percentage is reported.
- A Feed deleted after Run selection but before its result commits has the terminal per-Feed outcome `Removed`. The late result is discarded without recording success or failure and cannot recreate the Feed. Aggregate completion still advances, while failure and new-article counts do not.
- The module owns HTTP execution and short-lived database commits. It never holds a database connection during an HTTP wait. Each result is committed through the Data Maintenance generation fence, so a result from before restore cannot enter the replacement library.
- The module exposes only refresh intent, a snapshot, `Changed(RunId)` or module-fault notices, a Data Maintenance participant, and bounded shutdown. GUI owns repaint, tray notifications, and user wording; CLI owns summaries, stderr, and exit codes.
- Maintenance quiescence is an explicit adapter. Entering maintenance aborts outstanding HTTP work, preserves generation-validated commits, marks the Run `Interrupted`, and merges unfinished targets into the single pending set. Resume recomputes Feed eligibility and starts a new Run rather than resuming an old request.
- Normal idle scheduling sleeps until the earliest `next_fetch` and wakes through commands. It does not poll every 100 ms. Cross-process maintenance that is already active is observed with bounded maintenance-only polling.
- Hiding the window to the tray does not stop refresh. True application shutdown stops accepting intent, interrupts the active Run, and waits at most two seconds. No Run is restored on next launch.
- CLI exit codes are `0` for `Succeeded`, `2` for `Degraded`, and `1` for `Failed` or `Interrupted`.
- Technical details are classified, flattened, capped, and minimally redacted before crossing the module boundary. Full HTTP bodies and credentials are not retained.

## Persistence and process ownership

This decision adds no table and changes no Feed schema. The project intentionally does not add a global RSS executor lease yet. Two processes can duplicate network fetches, while article uniqueness, independent Feed commits, and Data Maintenance generation fencing protect local data. A global lease should be reconsidered only if duplicate external traffic becomes a demonstrated problem.

## Consequences

GUI and CLI can no longer diverge on target selection, concurrency, partial failure, maintenance interruption, or result classification. Subscription persistence and initial-refresh ordering are delegated to ADR-0005 rather than reimplemented by adapters. GUI state is derived from the workflow snapshot instead of scheduler atomics, and Local Data Maintenance coordinates through a real participant boundary. Deterministic Fetch and Clock adapters make scheduling and state transitions testable without network access.

The cost is a dedicated workflow state machine and adapter boundary. Run history does not survive restart, and simultaneous processes may still issue duplicate HTTP requests. Those are deliberate limits rather than accidental omissions.

## Rejected alternatives

- Keep GUI, CLI, and add-subscription composition separate: fewer immediate types, but behavior and failure policy continue to drift.
- Persist Run and Attempt tables: duplicates durable Feed state and adds recovery semantics without a user need.
- Reuse the Knowledge Processing task engine: RSS has independent per-Feed commits, due scheduling, and no durable aggregate outcome; a generic job framework would expose more policy than it hides.
- Unlimited `JoinSet` concurrency: simple code, but resource use grows with the subscription count.
- Resume old HTTP futures after maintenance: stale responses would cross a replaced database generation.
- Add a cross-process executor lease now: materially more ownership machinery without evidence that duplicate fetch traffic is harmful.
