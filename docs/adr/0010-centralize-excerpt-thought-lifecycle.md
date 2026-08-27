# ADR-0010: Centralize the Excerpt & Thought Lifecycle

- Status: Accepted
- Date: 2026-08-26
- Decision owners: project maintainer and Codex implementation session

## Context

Excerpt and Thought behavior was spread across selection gestures in `gui.rs` and public `Db` helpers. Capturing the same passage could create duplicates, a Thought could exist in a row that was not retained as an Excerpt, counts were reloaded separately from rows, delete meant both “remove the Thought” and “remove the whole record”, and Library Search visibility depended on callers remembering every related write. Old exact-anchor duplicates also made an automatic merge unsafe because it could silently discard personal notes.

The product needs a durable saved-passage model that remains useful when an Article is archived, unbookmarked, or changes enough that its old anchor no longer resolves. It also needs one current optional Thought per Excerpt, explicit maintenance operations, predictable no-op behavior, and one projection shared by Article and complete-library views.

## Decision

Introduce `Excerpt & Thought Lifecycle` as the only production interface for creating, editing, projecting, and deleting Excerpts and Thoughts.

- An Excerpt is identified by Article plus its exact Stable Excerpt Anchor. Repeating capture on the same managed identity reuses the Excerpt; identical text at different anchors remains distinct.
- A Thought is the single optional current note owned by an Excerpt. Writing a Thought for a new capture first retains the Excerpt. Rewriting replaces the Thought; removing the Thought keeps the Excerpt; deleting the Excerpt also deletes its Thought.
- The interface exposes `project(scope)` and `apply(change, refresh_scope)`. Changes are `EnsureExcerpt`, `PutThought`, `RemoveThought`, and `DeleteExcerpt`. A Thought target is either an existing Excerpt identity or a complete new capture.
- Projection scopes are one Article or the complete saved collection. Every projection contains Excerpts, optional Thoughts, resolved/unresolved anchor status, managed/Legacy identity kind, Article navigation material, and authoritative collection and scope counts.
- The primary sidebar count counts Excerpts. Thought count is supplementary and never creates a second saved item.
- Article archive, read, read-later, bookmark, and Tag changes do not remove Excerpts. Permanent Article deletion cascades to its Excerpts and Thoughts.
- Resolved and Unresolved Excerpts remain retained and searchable. Unresolved means only that the current Article body cannot promise automatic navigation.
- Exact-anchor duplicates created before this decision are preserved as Legacy Excerpts. The newest `(updated_at DESC, id DESC)` row becomes the managed representative; other rows remain independently maintainable. If the representative is deleted, the next newest Legacy row is promoted on the next capture.
- A successful change, Library Search visibility, counts, and the requested projection commit in one short SQLite transaction guarded by the existing writer permit. There is no queue, background task, or automatic retry.
- CRLF and bare CR are normalized to LF. New Excerpts are limited to 256 KiB and Thoughts to 64 KiB. Blank Thoughts and invalid anchors are rejected. Existing oversized legacy data is preserved.
- Exact no-ops return `Unchanged`, do not update `updated_at`, and do not move the Excerpt in collection order. A missing explicit target is `NotFound`; removing an already absent Thought is `Unchanged`.
- Desktop Interaction owns text-selection gestures, Popovers, Modals, Notices, and delete confirmation. The GUI renders the returned projection and does not call the old selection persistence helpers. Deleting an Excerpt with a Thought requires confirmation; editing or deleting only the Thought is separate.
- This work does not add CLI commands. Future adapters must use the same lifecycle interface.

## Module seam and ownership

`src/excerpt_thought_lifecycle.rs` owns the typed interface, validation, errors, projection shape, exact no-op meaning, clock seam, and transaction sequence. `src/excerpt_thought_lifecycle/store.rs` owns SQLite identity encoding, managed/Legacy selection, projection queries, anchor resolution, search-index consistency, and schema migration.

The implementation is deliberately SQLite-specific and locally substitutable with temporary or in-memory databases. It does not introduce a generalized repository interface. A private `Clock` seam supplies deterministic timestamps in tests without widening the production interface.

Article Library Lifecycle continues to own Article collection state. Library Search continues to own query and ranking. Web Clipping Lifecycle and Feed Subscription Lifecycle own permanent Article deletion paths and cause the desktop root to adopt a fresh Excerpt & Thought Projection afterward.

## Persistence and migration

Schema version 7 adds nullable `article_selections.lifecycle_identity BLOB` and a partial unique index on `(article_id, lifecycle_identity)` for non-null identities. Non-null identifies managed Excerpts; null identifies preserved Legacy Excerpts.

Migration promotes every old Thought-only row to a retained Excerpt by setting `is_favorite = 1`, so no Thought is lost or hidden. For each exact Article-and-anchor group, the newest row receives the managed identity while duplicates remain null. The unified FTS rows are rebuilt after promotion. Migration and all source writes keep the authoritative records and derived search visibility in the same transaction.

## Verification contract

- Repeated `EnsureExcerpt` on one exact anchor reuses identity and preserves timestamps on no-op.
- Captured Thought creates a retained Excerpt; replace, same-content no-op, and remove have distinct outcomes.
- Removing Thought preserves Excerpt and search visibility; deleting Excerpt removes both Excerpt and Thought search rows.
- A missing existing target is not reported as success.
- Article body changes can produce Unresolved without deleting saved material.
- v6-to-v7 migration preserves duplicate legacy Thoughts, promotes one deterministic representative, advances `user_version`, and passes integrity checks.
- GUI Route/Modal compatibility includes Thought editing and Excerpt deletion from both Article and Excerpts routes.
- Full formatting, Clippy, unit, integration, migration, and fixed Library Search regression checks must pass before release.

## Consequences

The GUI now has one coherent saved-material snapshot instead of independently reloading rows and counts. Same-anchor capture becomes idempotent, Thought maintenance is explicit, old personal notes are not merged away, and Library Search observes every successful change atomically.

The trade-off is one more schema identity and a synchronous projection read after each change. The partial unique index intentionally applies only to managed records; Legacy duplicates remain visible until a person chooses to delete them.

## Implementation review amendment (2026-08-27)

A newly created capture must have a non-empty character range whose exact current rendered Article-text substring equals `selected_text`; zero-width, out-of-bounds, and mismatched anchors are Input failures and create no row. The canonical coordinate text comes from Article Document Presentation's private parser facade rather than raw saved HTML. Identity lookup still precedes this creation-only validation, so an already-saved Excerpt remains idempotently addressable after later Article-body changes make its anchor Unresolved.

## Rejected alternatives

- Keep direct `Db` helpers in GUI: leaves duplicate identity, no-op time, counts, search visibility, and delete meaning distributed across presentation code.
- Model Thought as an independent append-only entity: conflicts with the confirmed single-current-note behavior and adds history semantics the product does not need.
- Merge all legacy duplicates automatically: risks deleting or combining distinct personal Thoughts without consent.
- Deduplicate by selected text: incorrectly merges equal text at different Article locations.
- Introduce a generic repository interface: adds surface area while the only real durable implementation and test substitute are SQLite connections.
- Use a background queue for local note edits: weakens immediate feedback and adds retry state to short local transactions.
