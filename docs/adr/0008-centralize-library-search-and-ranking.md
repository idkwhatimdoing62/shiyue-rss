# ADR-0008: Centralize Library Search and ranking

- Status: Accepted
- Date: 2026-08-25
- Decision owners: project maintainer and Codex implementation session

## Context

Cross-library retrieval was split between `Db::search_library`, `ResourceStore::search_json`, GUI-only result handling, and CLI JSON assembly. Each path chose its own corpus, ranking, duplicate behavior, evidence shape, history writes, and synchronous execution policy. The split produced contradictory GUI/CLI results, allowed related Resources and Articles to occupy multiple ranking positions, and made search-index maintenance part of unrelated write workflows.

The product needs one local deterministic retrieval contract for people and agents before semantic retrieval is justified. It must preserve local privacy, explain why a result matched, keep normal desktop interaction responsive, and remain testable against a fixed mixed-library regression set.

## Decision

Introduce `Library Search` as the only query and ranking module for GUI, CLI, and agent adapters.

- Its external interface accepts a typed query, scope, result filter, origin, and bounded limit, and returns typed `Library Search Result` values plus non-fatal warnings. The interface also reads and clears human Search History.
- `Curated` searches Active Resources, Article Bookmarks, Web Clippings, Excerpts, and Thoughts. `AllArticles` additionally searches unarchived RSS Articles. `Archive` searches archived primary material. Excerpts and Thoughts remain eligible in Curated after their Article is archived.
- Results use one ranking position per primary identity. An eligible Resource is primary over an explicitly linked Article and then over an Article with the same canonical URL. Related Articles, Web Clippings, Excerpts, and Thoughts become bounded Search Evidence and navigation targets on that result.
- Relevance is deterministic and local. Exact identity and title matches lead, followed by the strongest matched field, bounded corroboration, a small manual-rating boost, and a small Broken-health penalty. Recency only breaks otherwise equal scores. Raw numeric scores are not exposed through presentation adapters.
- Active Broken Resources remain eligible. Private Resources receive no local human-search penalty, but the agent adapter rejects them so private fields cannot cross into remote AI work.
- GUI and user-run CLI searches record deduplicated Search History after the query succeeds. Agent searches do not. A history-write failure returns a warning without discarding valid results.
- Empty queries and limits outside `1..=200` are Input failures. Maintenance, storage, and index faults remain distinct failure kinds. Query and ranking failures fail the whole search.
- SQLite FTS, query normalization, eligibility, grouping, ranking, evidence selection, Search History, and the two-second execution budget remain hidden implementation knowledge. There is no repository interface, semantic-ranker interface, pagination contract, or persistent search task.
- The local implementation is synchronous. The desktop adapter runs it on a background thread and uses the Desktop Interaction request identity from ADR-0002 so closing a Modal or starting a newer query invalidates late results.
- CLI `resource search` remains the machine-readable command, adds the Archive scope, and emits search schema version 2. GUI and CLI serialize or render the typed result; neither reconstructs ranking.

## Module seam and ownership

Library Search owns query normalization, corpus eligibility, primary-identity grouping, relevance, bounded Search Evidence, the derived unified search index, and Search History.

Resource Library Lifecycle, Article Library Lifecycle, RSS Refresh, Web Clipping intake, Excerpt and Thought writes, and Knowledge Processing own their source records. Their committed SQLite transactions update the derived search index through Library Search's triggers; callers never refresh FTS rows manually. Desktop Interaction owns Route, Modal, Panel, request identity, and background-result adoption. Presentation adapters own GUI rendering and CLI JSON only.

## Persistence and migration

Schema version 5 replaces `resource_fts` and `library_fts` with one `library_search_fts` index. Migration drops the old triggers and tables, creates the unified index and source-write triggers, backfills Resources, Articles, Excerpts, and Thoughts, validates the derived row count, and commits atomically. A failed creation or validation rolls the entire migration back and leaves `user_version=4`.

Normal search never repairs or silently rebuilds the index. Index creation and repair belong to versioned migration or explicit maintenance, so a corrupt or missing index is observable as an Index failure.

## Verification contract

- The checked-in mixed regression set contains 25 Resources and 15 Articles with 25 accepted queries. Recall@5 must remain 100%; MRR is recorded but is not yet a release gate.
- Deterministic tests separately cover Curated, All Articles, and Archive scopes; Resource/Article grouping; Excerpt and Thought retention; Broken and Private behavior; Search History origin; lifecycle-trigger consistency; migration backfill and rollback; GUI late-result rejection; CLI schema version 2; invalid input; and timeout behavior.
- The local benchmark uses 1,000 Resources, 10,000 Articles, and 2,000 notes. Every measured query must remain below the two-second budget, while P50 and P95 are recorded for trend comparison.

## Consequences

GUI, CLI, and agent adapters now share one corpus, one ordering, one duplicate policy, and one evidence model. Source lifecycles no longer know FTS SQL, valid query results survive non-critical history faults, private Resource material is unavailable to the agent adapter, and desktop searches no longer block the UI thread.

The trade-off is deliberate SQLite coupling inside one deep module and a versioned derived index that must be migrated with source schema changes. Semantic retrieval remains deferred until a real second ranking adapter and a measured regression gap justify its interface cost.

## Implementation review amendment (2026-08-27)

Search History insert/update and clear operations now use the same fenced transaction as every other local-library mutation. A search may still return valid results while maintenance intent is active, but its non-critical history write becomes an explicit `HistoryNotRecorded` warning; clearing history returns a typed Maintenance failure. This closes the remaining mutation path that could bypass ADR-0003's maintenance authority.

## Rejected alternatives

- Keep separate Resource and Article search paths: preserves less refactoring, but keeps contradictory eligibility, ranking, and JSON behavior.
- Add a generic repository or ranker interface now: creates a shallow abstraction over one SQLite implementation and exposes storage mechanics without a second adapter.
- Add embeddings immediately: increases privacy, migration, indexing, and ranking complexity before the deterministic Top-5 baseline shows a recall gap.
- Let normal queries repair FTS automatically: hides corruption, adds unpredictable latency, and turns a read into an undeclared maintenance write.
- Persist every search as a background task: adds lifecycle and cleanup cost to a bounded local read whose only asynchronous requirement is desktop responsiveness.
