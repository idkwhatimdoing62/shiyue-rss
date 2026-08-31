# ADR-0001: Persist knowledge processing outside the GUI

- Status: Accepted
- Date: 2026-08-24
- Decision owners: project maintainer and Codex implementation session

## Context

Resource completion and Article summarization previously lived in `gui.rs` as unrelated ad-hoc threads and an in-memory task registry. Navigating between pages could hide status, a restart erased failure history, retries overwrote the user's mental model, and the GUI performed AI-result database writes. The first persistent implementation removed those writes from the GUI, but still exposed its SQLite workflow object, let the GUI keep a second in-memory task truth, treated CLI retry as a placeholder, and interrupted every running Attempt whenever any process started an engine.

The product needs observable background work without adding a separate task-center feature. Existing Resource and Article screens must show meaningful stages, failures, retries, and technical details. Saved source material must survive an AI failure.

## Decision

Introduce one long-lived Knowledge Processing Workflow module outside the GUI.

- `KnowledgeEngine` is the only external seam. GUI and CLI are adapters that express `ResourceCompletion` or `ArticleSummary` intent and observe snapshots. SQLite task rows, lease management, executor loops and retry selection remain private implementation details.
- A request is idempotent while the target has queued/running work. A failed or interrupted outcome creates a new Attempt on the same Task. A request after success creates a new Task. The adapters never choose between submit and retry.
- Persisted Task/Attempt state is the only truth. `Changed(TaskKey)` notices only prompt an adapter to reread a snapshot; they do not carry an independently authoritative task copy. The GUI does not maintain a task `HashMap` or query workflow tables directly.
- A Task represents one requested outcome. An Attempt represents one execution. Manual and automatic retries append Attempts and preserve earlier errors.
- Tasks and Attempts are persisted in SQLite. Connection tests use the same provider boundary and observable update channel but remain session-only.
- Resource completion advances from `Fetching` to `Organizing`; Article processing uses `Summarizing`. The UI shows stages rather than fabricated percentages.
- Only one queued/running Task of the same kind and target may exist. A SQLite executor lease ensures that all processes share one executor owner, which runs at most two jobs globally and selects eligible work FIFO.
- The lease heartbeat is five seconds and expires after fifteen seconds. Every acquisition increments a generation. An Attempt records the generation that claimed it, and every stage, business-result and terminal write verifies both the live lease and claim generation. A stale worker's late result is discarded.
- Cross-process observation uses a monotonic SQLite change clock and `knowledge_tasks.change_seq`. Watchers poll from a cursor and coalesce notices by `TaskKey`; no unbounded event table is retained.
- Transient failures receive exponential automatic retry delays of one and two seconds, at most twice. Authentication, security, input, provider-output, storage, and interrupted errors require user action.
- A successful Resource Snapshot is committed before organizing and remains available when organizing fails.
- Each worker opens its own SQLite connection. Provider credentials are read at Attempt start through `CredentialSource` and are never persisted with the Task. Technical details are centrally sanitized, secrets are redacted, and provider output is capped before persistence or display.
- Resource/Article business results, enrichment-run completion, Attempt terminal state and change sequence are committed in the same fenced transaction. A successful Resource Snapshot may be committed earlier, but its write and stage transition are also fenced atomically.
- Closing a page does not cancel work. Normal owner shutdown stops new claims, waits up to two seconds, marks its remaining generation `Interrupted`, and releases the lease. Starting a non-owner process never interrupts the live owner.
- On lease takeover, only the new owner interrupts Attempts claimed by an older generation. Queued Tasks continue. Interrupted work is not automatically retried; a new user/CLI intent creates the next Attempt.
- CLI retry is real workflow execution. By default it waits up to five minutes for terminal state; `--timeout` changes the wait and `--no-wait` only persists intent without acquiring the executor lease. A wait timeout does not cancel the Task.
- Provider connection tests use the same provider implementation and notice channel but remain session-only. They run only for the lease owner and discard a result if ownership changes.
- Knowledge Processing owns its Local Data Maintenance adapter and exposes it through the `KnowledgeEngine` external seam. GUI and CLI adapters never observe an atomic pause flag or implement safe-point polling themselves.
- Local maintenance uses an explicit quiesce/resume handshake. Quiesce stops new claims, rejects new Task and connection-test intent with `MAINTENANCE_IN_PROGRESS`, closes the workflow database connection, and releases the executor lease before acknowledging the shared maintenance deadline.
- Provider calls are not assumed cancellable. An Attempt may finish inside the deadline; otherwise the owning generation is marked `Interrupted` and fenced before the safe point is acknowledged. Detached late results cannot commit after the lease is released or the library generation changes.
- Queued Tasks survive maintenance. Interrupted Attempts are preserved and require a new user or CLI intent; resume only confirms that the engine accepts commands and can compete for the executor lease, not that it owns the lease.
- No cancellation interface or third facade is introduced. Cancellation requires separate semantics for provider interruption and transaction compensation and is deferred until there is a concrete product need.

## Database migration

Schema version 2 added `knowledge_tasks` and `knowledge_task_attempts`, including a partial unique index for active target work. Schema version 3 adds the executor lease, generation claims, monotonic change clock, per-Task change sequence, and the Enrichment Run → Attempt link. During v2→v3 migration queued work remains eligible, while old running Attempts without a fencing generation become `Interrupted` with `WORKFLOW_UPGRADE_INTERRUPTED`. Databases from a newer unsupported schema version are rejected instead of being opened unsafely.

## Consequences

Failures and retries remain diagnosable across restarts, GUI navigation cannot orphan work, and Resource/Article paths share one lifecycle. GUI and CLI cannot diverge on retry or status semantics, and opening a second process cannot steal or destroy valid work. The workflow module gains depth by hiding persistence, execution, fencing, maintenance participation and failure classification behind a small seam. The tradeoff is lease/heartbeat machinery, more transactional code, bounded maintenance interruption, polling latency, and a versioned migration for ownership metadata.

## Implementation record (2026-08-31)

The private Desktop Knowledge Processing adapter was extracted to `src/gui/knowledge_feature.rs`. It owns desktop interaction state for the DeepSeek key draft, connection-test state, watched task keys, workflow notice intake, task submission, retry requests, and user-facing task outcomes. `GuiApp` retains Route/Modal/Panel state, Desktop Library Projection demand and adoption, and Notice publication. The Knowledge Processing workflow, persistence, retry, fencing, and provider semantics are unchanged.

## Rejected alternatives

- Keep independent GUI threads: simple locally, but lifecycle and persistence remain duplicated.
- Persist only the latest status: loses Attempt history and makes retries impossible to diagnose.
- Expose the SQLite workflow/store to adapters: reduces a local wrapper but leaks retry, consistency and migration policy into every caller.
- Carry full Task snapshots in notices: creates a second ordering and consistency truth beside SQLite.
- Let every process run workers: appears simple but violates the global concurrency limit and permits stale writes after takeover.
- Add a generic job framework or transport facade: more abstraction than the two current workflows justify; the second adapter is served directly by the existing deep module.
- Cancel on page close: couples domain work to navigation and contradicts the expected background behavior.
