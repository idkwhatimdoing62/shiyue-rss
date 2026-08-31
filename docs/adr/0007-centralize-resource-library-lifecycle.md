# ADR-0007: Centralize the Resource Library lifecycle

- Status: Accepted
- Date: 2026-08-25
- Decision owners: project maintainer and Codex implementation session

## Context

Resource creation, review, editing, archiving, deletion, Web Clipping import, health updates, and AI processing handoff were previously assembled across the GUI, CLI, the old `ResourceService`, and Knowledge Processing. Callers could read a broad list and reconstruct their own collections, issue a database write and then separately start AI work, or interpret one legacy `status` value as both human curation and source health. That made partial writes, stale counts, lost processing requests, and contradictory states possible.

Resource Library and Knowledge Processing also need a precise boundary. Resource Library owns durable human intent and library membership. Knowledge Processing owns network fetch, local snapshot production, private-resource handling, AI enrichment, retries, and observable task execution. Neither module may take over the other's workflow.

## Decision

Introduce `Resource Library Lifecycle` as the only application-level read and write seam for Resource creation, complete manual editing, curation transitions, permanent deletion, Web Clipping import, and Resource Library projections.

- The external interface is deliberately small: `project(scope)` returns an authoritative `Resource Library Projection`; `apply(change, refresh_scope)` performs one explicit lifecycle change and returns an authoritative projection plus post-commit processing handoff receipts.
- A Resource has an immutable identity. Canonical URL conflicts return the existing Resource instead of silently mutating it.
- Human curation and source health are orthogonal. `curation_state` is `pending_review`, `active`, or `archived`; `health` is `unknown`, `healthy`, or `broken`. Broken is an overlapping operational view, not a competing curation state, and archived Resources are omitted from it.
- CLI Agent creation enters Pending Review and does not start processing. GUI creation and Web Clipping import enter Active. Moving Pending Review to Active requests Resource Completion after the database commit.
- A complete manual edit atomically replaces title, purpose, use-when guidance, private note, privacy, rating, Categories, and Tags. Purpose, use-when guidance, Categories, and Tags receive manual provenance so later AI output cannot overwrite them.
- Public-to-private conversion is rejected while Resource Completion is queued or running. Private Resources remain local and Knowledge Processing completes them without a cloud provider call.
- Permanent deletion is allowed only from Pending Review or Archived. It rejects queued or running processing, removes terminal Resource Completion history, snapshots, classifications, and the Resource in one transaction, and never deletes a linked Article. The derived search index follows the transaction through ADR-0008 rather than lifecycle-owned FTS writes.
- Web Clipping import validates the complete selection before commit and is all-or-nothing. Canonical URL and linked Article constraints make reruns idempotent. Newly imported Resources are Active and Healthy because their local content already exists.
- Collection projections are ordered by `(updated_at DESC, id DESC)`, use a stable cursor, return authoritative full collection counts, and cap a page at 200 rows. GUI routes adopt the projection instead of loading 1,000 rows and filtering locally.
- Failures are typed as Input, Not Found, Invalid Transition, Processing Active, Maintenance, or Storage, with separate user-facing and technical detail.
- Writes use the shared SQLite writer permit and one transaction ordered as: validate input, acquire permit, resolve targets, mutate, build the requested projection, validate the permit, and commit. Processing handoff occurs only after commit. A handoff failure leaves the durable Resource change successful and returns a Deferred receipt that can be retried.

## Module boundary

Resource Library Lifecycle owns Resource identity, curation, health projection, manual-field provenance, classification replacement, Web Clipping import, deletion rules, and Resource collection projections.

Knowledge Processing owns Resource fetch, snapshot success/failure recording, enrichment, privacy-safe execution, task state, retry, and technical failure detail. It crosses a narrow internal Resource target seam for loading processing input and applying processing results. Its task request adapter implements the Resource Library post-commit handoff.

Library Search and ranking are owned by the sibling module in [ADR-0008](0008-centralize-library-search-and-ranking.md). Article Library, Feed Subscription, RSS Refresh, Local Data Maintenance, and Desktop Route/Modal state also remain sibling modules.

## Persistence and migration

Schema version 4 adds `curation_state`, `health`, `categories_source`, `tags_source`, and `source_failure_count` to `resources`, plus curation and health indexes. Migration is additive and backfills:

- legacy `pending_review` and `archived` into the matching curation state; every other legacy status becomes Active;
- legacy `broken` into Broken health, a successful snapshot into Healthy, otherwise Unknown;
- classification provenance from successful enrichment and existing manual Tags.

The legacy `status` column remains only for backward-compatible storage during this release. Production projection and search membership no longer read it. A later versioned migration may remove it after compatibility support is no longer needed.

## Implementation record

The initial implementation temporarily retained a private `ResourceStore` with broad storage-shaped CRUD, transition, snapshot, enrichment-run, usage-event, query, and Web Clipping methods. Once all application callers had adopted the lifecycle seam, that interface no longer had production consumers and became a misleading second way to express Resource behavior.

The broad store was retired on 2026-08-28. The private SQLite file now exposes only three stateless persistence operations required across sibling boundaries:

- serialize one Resource search result with its Categories and Tags;
- load privacy-safe Resource Completion input;
- apply enrichment inside a caller-owned fenced transaction.

Lifecycle commands, Web Clipping import, Resource projections, validation, provenance, and deletion remain exclusively in the parent `Resource Library Lifecycle` module. Snapshot identity and task advancement remain in Knowledge Processing. No schema or user-visible JSON contract changed as part of this narrowing.

Behavioral coverage formerly attached to the broad store moved to the owning modules: Resource lifecycle tests cover linked-Article preservation, private processing input, manual-intent preservation, and idempotent Web Clipping import; Knowledge Processing covers content-addressed snapshot reuse; Library Search owns the mixed-library Recall@5 regression fixture. Schema evolution continues to own released-database upgrade and integrity fixtures.

## Consequences

GUI and CLI now share one definition of Resource collections, transitions, import idempotence, deletion safety, maintenance behavior, and projection counts. AI work cannot begin before its Resource commit, manual data is protected from enrichment, and the Knowledge workflow can update health without owning library membership.

The cost is an explicit projection read inside each write transaction. The module intentionally keeps SQLite concrete; real-database tests cover constraints, rollback, task races, migration, and cursor behavior instead of introducing a repository abstraction or cache. Search-index consistency is verified by ADR-0008.

## Implementation record (2026-08-31)

The Resource Collection curation adapter was completed in `src/gui/resource_feature.rs`. It owns the GUI transition intent for Pending Review, Active, and Archived Resources, invokes `SetCurationState`, and returns the authoritative projection and user-facing outcome. The GUI root retains collection rendering, filtering, pagination, navigation, and task retry presentation; it no longer calls the Resource lifecycle directly for curation transitions. This preserves the single lifecycle seam and the existing transaction, handoff, and Notice semantics.

## Rejected alternatives

- Keep the broad `ResourceService`: convenient for callers, but exposes storage-shaped operations and lets them assemble invalid workflows.
- Use one status enum for curation, processing, and health: fewer columns, but makes Pending Review, Active, Broken, and Archived mutually exclusive when they are not.
- Start Knowledge Processing before commit: lowers apparent latency, but permits tasks for rows that never committed.
- Let AI merge manually edited classifications: avoids provenance fields, but can silently destroy user intent.
- Delete queued work together with a Resource: simpler cleanup, but races an active worker and obscures whether external work still exists.
- Add a generic repository or in-memory cache: increases interface surface and coherence work without evidence that local SQLite reads are a bottleneck.
