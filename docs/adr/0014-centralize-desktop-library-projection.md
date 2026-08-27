# ADR-0014: Centralize Desktop Library Projection Adoption and Invalidation

- Status: Accepted
- Date: 2026-08-27
- Decision owners: project maintainer and Codex implementation session

## Context

The immediate-mode GUI repeatedly rebuilt library projections while drawing. The Resource route called `ResourceLibraryLifecycle::project` every frame, the Resource editor independently loaded detail every frame, and visible Knowledge tasks were obtained through `KnowledgeEngine::snapshot`, which opened SQLite for each cache miss. A GUI-owned `KnowledgeTaskCache` partially hid those reads without owning invalidation, cross-process changes, maintenance generation changes, or task materialization.

This mixed durable truth, projection construction, cache adoption, and widget presentation. It made frame cost depend on SQLite and made a successful write visible through several unrelated refresh paths. Timestamp polling was not safe: equal-second writes can collide, clocks are not a total order, and a restored database can reuse old timestamps while representing a different library generation.

## Decision

Introduce `Desktop Library Projection` as the Desktop-only Module that owns adoption and invalidation of library projections.

- Its external Interface is `frame(DesktopProjectionDemand) -> DesktopProjectionFrame` plus `accept(DesktopProjectionFact)`. A warm frame performs bounded channel draining and memory reads only; it does not call SQLite, a Lifecycle Module, or a sidecar.
- Demand is typed and finite. It accepts Article scopes, semantic Resource collection/detail demand, Excerpt scopes, and visible Knowledge task keys. Pagination cursors remain internal: the GUI can only request `LoadMoreResources(collection)` and observe `has_more`. It does not expose a generic projection registry or repository abstraction.
- Projection freshness is explicit: `Loading`, `Current`, `Refreshing` with the last successful projection retained, `Failed` with bounded technical detail and an optional last success, and `Maintenance` with no adopted data.
- The asynchronous inbox is bounded to 32 facts per frame. Equal demand does not enqueue duplicate work. A bounded worker-side coalescing queue retains terminal results while the delivery channel is full. Inactive Article, Resource, and Excerpt scopes use bounded retention; family-global counts travel with their authoritative projections.
- Article, Resource, and Excerpt results carry a `ProjectionStamp`: the opaque maintenance generation from the existing writer gate plus the monotonic family revision. Results from another generation, a lower family revision, or the wrong typed scope are rejected as whole projections.
- Ordinary invalidation is stale-while-refresh. A generation change is not: all old-generation data is cleared before the new generation can be adopted.
- Cross-process changes are detected by a worker-owned long-lived database connection that watches the complete Article, Resource, and Excerpt revision vector every 250 ms. The GUI does not poll SQLite.
- The Module is a `MaintenanceParticipant`. Quiescence closes its worker-owned database before acknowledgement; resume reopens under the new generation.
- Knowledge task materialization remains owned by Knowledge Processing. `KnowledgeProjectionObserver` accepts observation intent, materializes task snapshots on the workflow thread, publishes the latest snapshot to shared memory, and emits only bounded change hints. Desktop Library Projection adopts that memory; GUI no longer calls `KnowledgeEngine::snapshot` during frames and owns no parallel task cache.

### Knowledge observation residency

Knowledge observation is demand-scoped rather than an append-only cache:

- The deduplicated Knowledge keys in the current `DesktopProjectionDemand` are the complete residency set. A newly demanded key is observed once. A key absent from the next frame is forgotten immediately; there is no hidden TTL, terminal grace period, or Knowledge LRU.
- GUI operations that need observation beyond the currently visible panel hold that residency explicitly through `knowledge_watch`. A requested task remains demanded while queued or running and is released after its terminal notice is published.
- Knowledge Processing stores `residents` separately from `snapshots`. Forgetting a key synchronously removes both residency and materialized data from workflow-owned shared state. The workflow thread checks that state before every publication, so a late change or queued Observe command cannot recreate a snapshot for a non-resident key.
- Re-observing an evicted key materializes its latest durable task from SQLite; eviction never deletes durable task or attempt history.
- Maintenance clears every materialized Knowledge snapshot before quiescence is acknowledged but retains the explicit resident set. Resume rematerializes those residents from the reopened generation. Observation requests made during maintenance become residents without publishing old-generation data.
- Bounded change hints are repaint/coalescing aids only. They neither grant residency nor keep a key alive. Dropping Desktop Library Projection forgets all keys it owns.

## Durable revision vector and schema evolution

Schema version 8 adds the singleton `library_projection_revisions` table with independent non-negative `article_revision`, `resource_revision`, and `excerpt_revision` positions.

A writer advances every affected family in the same SQLite transaction as the durable change. A real no-op does not advance a revision. The maintenance generation is intentionally not stored in the main database; it reuses the existing sidecar epoch so restore and replacement create a hard adoption boundary.

The first implementation slice connected the Resource family:

- Resource Library Lifecycle advances `resource_revision` for `Created`, `Changed`, and `Deleted`, but not `Existing` or `Unchanged`.
- Complete manual Resource edits compare normalized fields, provenance, categories, and tags and return a true no-op without touching `updated_at` or the revision.
- Knowledge Processing advances `resource_revision` in the same transaction as snapshot success, fetch failure state, and enrichment application.

The follow-on revision-witness slice connects every persistent Article and Excerpt writer before their Desktop adoption is enabled:

- Writers derive a closed `ProjectionImpact` from the observable durable change. The revision Module deduplicates the affected Article, Resource, and Excerpt families and records the complete impact in one revision-vector update inside the caller's transaction.
- Article Library Lifecycle advances Article only for `Changed`; `Unchanged` returns the existing revision. Excerpt & Thought Lifecycle advances Excerpt for Created, Changed, and Deleted, but not Unchanged.
- Article Knowledge Processing advances Article only when summary, translation, or model content changes. Repeating identical output preserves the prior material timestamp and revision.
- RSS Refresh runs article persistence, exact mutable-field comparison, Feed scheduling updates, and revision recording in one transaction. New or observably changed Articles advance Article. A changed body also advances Excerpt when that Article has retained Excerpts; scheduling-only changes do not advance a family.
- A new Feed advances Article because Feed unread material gains a `(feed_id, 0)` entry. Enable, disable, refresh request, interval, and scheduling changes do not. Feed deletion always advances Article and conditionally advances Resource and Excerpt only when the cascade detaches or deletes that material.
- Web Clipping capture advances Article. Web Clipping deletion advances Article and conditionally advances Resource and Excerpt using the transaction's actual detach and cascade sets.
- Any revision-recording failure aborts the same transaction as the durable change. Revision recording after commit is forbidden.

The resulting impact matrix is:

| Durable change | Article | Resource | Excerpt |
| --- | --- | --- | --- |
| Article lifecycle change | when changed | no | no |
| Excerpt or Thought lifecycle change | no | no | when changed |
| Article Knowledge result | when content changed | no | no |
| RSS Article insert/mutable-field change | when changed | no | when body changed and retained Excerpts exist |
| Feed add | when created | no | no |
| Feed enable/disable/interval/scheduling | no | no | no |
| Feed delete | when deleted | when links detach | when retained Excerpts cascade |
| Web Clipping capture | yes | no | no |
| Web Clipping delete | yes | when links detach | when retained Excerpts cascade |

The vertical-adoption slice connects Article and Excerpt without routing either family through the Resource implementation. Each family retains its own typed scope, cache, known revision, load command, terminal event, and lifecycle projection. Article projections include the selected scope's persisted AI summary and translation material, so Article Knowledge completion is adopted through the same revision path instead of a frame-time lookup. A family revision invalidates only that family's scopes; a generation change clears all three families before any new-generation result can be adopted.

## Module seam and ownership

`src/desktop_library_projection.rs` is the Desktop projection facade and worker. `src/library_projection_revision.rs` owns the v8 revision vector, the closed impact vocabulary, atomic impact recording, family vocabulary, and projection stamps. Each Lifecycle Module remains authoritative for deciding whether its observable durable material changed and which families that change affects. Knowledge Processing remains authoritative for tasks and attempts. SQLite remains durable truth.

The GUI supplies current typed demand, renders the returned frame, and feeds committed Article, Resource, and Excerpt projections back through `accept`. Article route changes and Excerpt selection changes schedule worker loads instead of calling lifecycle `project` methods on the UI thread. Search-result excerpt restoration waits for the demanded Excerpt projection instead of opening SQLite synchronously. The GUI does not interpret revision rows, compare generations, open a projection worker connection, or maintain a parallel projection cache. Local Data Maintenance coordinates the worker through the existing participant Interface.

`DesktopProjectionFrame` is the sole desktop presentation authority for Article and Excerpt projections and their derived tags, AI material, fixed Web Clipping identities, library counts, and Feed unread counts. The GUI retains only interaction state such as selection, scroll, drafts, and pending navigation anchors. It reconciles that interaction state against the current frame rather than copying projection rows into GUI-owned fields. A lifecycle success enters the presentation path only through `accept`; the next frame is the only source rendered by widgets.

No public repository, revision store, cache Adapter, or maintenance Adapter is introduced. The SQLite implementation and worker are concrete and locally substitutable in Module tests.

## Verification contract

- A warm stable frame enqueues no work and performs no database access.
- Identical demand does not schedule duplicate loads.
- At most 32 asynchronous facts are adopted per frame.
- Knowledge residency exactly follows deduplicated frame demand; removal evicts the memory snapshot immediately and re-observation rematerializes durable state.
- Knowledge maintenance preserves residency, clears materialized snapshots before acknowledgement, and rematerializes them only after resume.
- Old-generation or regressing Article, Resource, and Excerpt projections are never adopted.
- Real Resource changes atomically advance the Resource revision; exact manual-edit no-ops do not.
- Article and Excerpt lifecycle no-ops retain their previous family revision and successful projections expose the transaction's revision stamp.
- A forced revision-recording failure rolls back the corresponding durable change.
- RSS duplicate material is a revision no-op; a real body change advances Article and conditionally Excerpt.
- Feed and Web Clipping cascades advance all and only the families actually affected in one transaction.
- A second SQLite connection observes a committed Article revision equal to the returned projection stamp.
- The worker detects another process's Article, Resource, and Excerpt revisions within 500 ms under normal scheduling; the regression suite exercises all three families through a real second SQLite connection with a 750 ms scheduler guard.
- Maintenance closes the worker database, clears adopted generation data, and resumes on the new generation.
- Article routes, Excerpt views, Resource route/detail, and visible Knowledge task rendering contain no direct per-frame projection or snapshot reads.
- Full tests, strict Clippy, and a release build pass.

## Implementation record

The first slice was implemented on 2026-08-27. Schema evolution now replays v7 to v8 and verifies the revision singleton. Resource projections carry generation/revision stamps. Resource writes and Resource-target Knowledge writes bump the family revision atomically. `DesktopLibraryProjection` provides bounded semantic demand, internally owned cursor pagination, stale-while-refresh adoption, a 250 ms cross-process watcher, lossless bounded event delivery, LRU scope retention, maintenance participation, and generation/revision rejection. Knowledge Processing now owns an in-memory observation seam. The Resource route and editor consume one desktop frame and feed committed projections back through `accept`; their old direct reads and `KnowledgeTaskCache` were removed.

The revision-witness follow-on was implemented on 2026-08-27. `ProjectionImpact` now records a closed, deduplicated family set with one transaction-local vector update. Article and Excerpt lifecycle projections carry stamps; their no-op and rollback contracts are covered through their lifecycle Interfaces. RSS persistence now performs exact material comparison in one transaction. Feed and Web Clipping delete paths measure actual Resource detach and Excerpt cascade effects before deletion and record the combined impact atomically. Article Knowledge Processing treats identical provider output as a family no-op.

The Article/Excerpt vertical-adoption slice was implemented on 2026-08-27. `DesktopProjectionDemand` and `DesktopProjectionFrame` now expose typed Article and Excerpt scopes alongside Resource and Knowledge demand. The worker loads all three lifecycle projections, watches the complete revision vector, invalidates only the changed family under ordinary writes, and clears every family at a maintenance-generation boundary. Article and Excerpt lifecycle write results are immediately accepted into the same module; cross-process changes converge through the worker watcher. GUI route, tag, AI material, excerpt-list, article-excerpt, and search-result restoration paths no longer call Article or Excerpt projection reads directly.

Knowledge observation residency was made explicit on 2026-08-27. Desktop demand now reconciles the complete resident key set each frame and releases absent keys. Knowledge Processing separates resident intent from materialized snapshots, fences late publication after eviction, preserves resident intent across maintenance while clearing old-generation material, and rematerializes from durable truth after resume. Tests cover demand reconciliation, immediate eviction, terminal re-observation, and maintenance clearing/rematerialization.

The final GUI-consumption slice was implemented on 2026-08-27. `GuiApp` no longer retains parallel Article or Excerpt projections, derived metadata maps, fixed-membership sets, or library-count mirrors. Article rows, Excerpt rows, tags, AI material, Web Clipping identities, Feed unread counts, and Article collection counts are read directly from the current frame-owned `Arc` snapshots. Lifecycle outcomes are accepted into Desktop Library Projection and become visible only through a subsequent frame. GUI selection, remembered Article identity, and pending Excerpt anchors are reconciled against that authoritative frame. Entering maintenance now immediately clears adopted Article, Resource, and Excerpt data before the GUI releases its database handle. Regression tests lock down frame authority, maintenance clearing, and interaction-state reconciliation.

## Consequences

Desktop projection work now has strong Locality and the Module has greater Depth: a small Interface hides worker lifetime, SQLite projection reads, invalidation, revision watching, maintenance fencing, stale result rejection, task observation, and cache retention. GUI frame cost is decoupled from database size and cross-process writes have one observable adoption path.

The cost is retained memory, an additional worker connection, and explicit revision production in every future write seam. A missed family bump can leave a projection stale until another change; therefore new lifecycle writes must add transaction-level revision tests. Revision counters identify ordering, not semantic diffs, and they are not an event log.

## Rejected alternatives

- Keep direct per-frame lifecycle reads: preserves frame stalls and duplicate projection work.
- Keep a GUI-owned cache: the GUI cannot authoritatively materialize Knowledge tasks or coordinate maintenance and cross-process invalidation.
- Invalidate by `updated_at`: timestamps can collide, regress, or survive a database replacement without expressing generation.
- Add a trigger-only universal revision: triggers cannot express multi-family semantic no-ops cleanly and hide the transaction contract from lifecycle tests.
- Add a durable event log: the desktop needs current invalidation positions, not replay, retention, ordering across all domain events, or event-sourcing complexity.
- Build one global application snapshot: couples unrelated Article, Resource, Excerpt, Feed, and Knowledge evolution and makes every write invalidate the whole desktop.
- Introduce public repository/cache interfaces: there is one production SQLite/desktop implementation and deterministic local tests; a public extension seam would be shallow.
