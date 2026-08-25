# ADR-0006: Centralize the Article Library lifecycle

- Status: Accepted
- Date: 2026-08-25
- Decision owners: project maintainer and Codex implementation session

## Context

Article Bookmark, Read Later, Archive, read state, Tags, and Batch Article Actions used to be assembled directly in the GUI from low-level `Db` calls. The GUI also patched its article rows, counts, and Feed unread values independently after writes. This created several competing meanings for the same operation: archived Articles could remain visible in a normal collection, missing rows in a batch could produce a partial change, an Article with no Tags could be confused with a missing Article, and a failed write could leave optimistic UI state behind.

Web Clippings add another invariant. They are Articles stored under a hidden Feed and must always belong to Article Bookmarks, while their intake and permanent deletion remain separate workflows. The lifecycle boundary therefore needs to recognize fixed bookmark membership without taking ownership of Web Clipping creation or deletion.

## Decision

Introduce an `Article Library Lifecycle` module as the only application-level read and write seam for Article Bookmark, Read Later, Archive, read state, Tags, and Batch Article Actions.

- The external interface is deliberately small: `project(scope)` returns an authoritative `Article Library Projection`, and `apply(change, refresh_scope)` performs one explicit lifecycle change and returns the new authoritative projection.
- Changes set an explicit target state; they do not toggle based on caller state. Repeating the same valid change is successful and reports `Unchanged`.
- Bookmark, Read Later, Archive, read state, and Tags are independent. Archiving hides an Article from Feed, Article Bookmark, and Read Later collections without clearing those states. Restoring it reveals the retained states again.
- A Web Clipping has fixed Article Bookmark membership. Attempts to remove that membership are rejected as input errors. Web Clipping intake and permanent deletion remain sibling workflows.
- Batch actions are atomic, deduplicate repeated IDs, reject an empty selection, and roll back every change when any selected Article is missing. The supported batch vocabulary is Archive, Bookmark, and Read Later, each with an explicit target value.
- Replacing Tags means replacing the complete set. Names are trimmed, empty names are dropped, duplicate names are removed case-insensitively, unused tag rows are cleaned up, and the library search index is updated in the same transaction.
- A projection contains the scoped Articles, normalized Tags, fixed-bookmark IDs, Article Bookmark/Read Later/Archive counts, and Feed unread counts. `Article(id)` distinguishes `NotFound` from a present Article with an empty Tag set.
- GUI routes map to projection scopes and adopt projections wholesale. The GUI does not infer membership, patch counts optimistically, or issue a second query after a successful change. Opening an Article is not blocked when the follow-up mark-read change fails; the failure is reported while the body remains available.
- Failures are typed as `Input`, `NotFound`, `Maintenance`, or `Storage`. They contain a concise user message and separate technical detail. A failure never authorizes a projection update.
- Writes use the shared SQLite writer permit and one transaction ordered as: validate input, acquire permit, resolve all targets, mutate, rebuild and validate the requested projection, then commit. This gives last-successful-commit semantics; a later writer may supersede a returned projection.
- The module keeps SQLite concrete. Its tests use temporary real SQLite databases, constraints, triggers, FTS, and maintenance markers. There is no repository trait or in-memory cache.

## Module boundary

The lifecycle does not own Feed Subscription or RSS Refresh, Web Clipping intake or deletion, Excerpts and Thoughts, Knowledge Processing, Data Maintenance, or Desktop Interaction. Route, Modal, Panel, Popover, Notice, selection, and editor drafts remain GUI state. Those adapters may request a lifecycle change or adopt a projection, but they cannot bypass the lifecycle with low-level Article state writes.

## Persistence and migration

This decision adds no table or column and requires no schema migration. Existing `articles`, `tags`, `article_tags`, hidden Web Clipping Feed, and library FTS structures retain their meaning. The change centralizes their application semantics and removes the superseded shallow lifecycle methods from `Db`.

## Consequences

All desktop Article collections now share one definition of visibility, counts, idempotence, batch atomicity, Tag normalization, Web Clipping membership, maintenance behavior, and storage failure. The module is independently testable through real persistence, while the GUI receives coherent state instead of reconstructing it across multiple calls.

The cost is that a successful mutation rebuilds the requested projection inside the transaction, which performs more reads than an optimistic local patch. The current personal, local SQLite workload favors correctness and a narrow interface over a cache invalidation protocol. If profiling later shows a real bottleneck, projection granularity can be changed behind the same external seam.

## Rejected alternatives

- Keep direct `Db` calls in GUI handlers: fewer types, but preserves duplicated invariants and partial UI updates.
- Let the GUI optimistically patch rows and counts: appears responsive, but creates rollback and invalidation rules for every collection and concurrent writer.
- Expose a flexible patch object or generic Article repository: broadens the interface and permits invalid state combinations without hiding more implementation knowledge.
- Move Article lifecycle into Knowledge Processing or RSS Refresh: mixes local user intent with background content processing or network execution and makes both modules shallower.
- Add an Article cache now: duplicates SQLite truth and requires a coherence protocol that the measured workload does not justify.
