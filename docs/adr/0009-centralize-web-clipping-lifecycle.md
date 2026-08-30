# ADR-0009: Centralize the Web Clipping Lifecycle

- Status: Accepted
- Date: 2026-08-25
- Decision owners: project maintainer and Codex implementation session

## Context

Saving a web page was orchestrated directly by `gui.rs`: the Modal classified URL versus pasted HTML, created the network worker, rejected late request identities, prepared readable HTML, chose a title and base URL, and called independent `Db` writes. Permanent deletion also called `Db::delete_web_clipping` directly. This split left capture progress implicit, made cancellation depend on the Modal still existing, lost redirect provenance, allowed deletion to race Article Summary processing, and forced the GUI to reload Article Library state after writes.

Web Clipping capture is a session-bound interaction rather than a persistent Knowledge Processing task. It nevertheless needs one authoritative module because safe HTTP fetching, preparation, cancellation, maintenance fencing, SQLite commit, deletion and projection adoption form one lifecycle with important ordering rules.

## Decision

Introduce `Web Clipping Lifecycle` as the only production interface for capturing and permanently deleting Web Clippings.

- The desktop submits raw URL or pasted HTML input through `begin_capture` and receives one non-reusable `CaptureLease`. At most one Capture is active per process; there is no queue and no automatic retry.
- The lease exposes identity, a revisioned snapshot, and explicit cancellation. Capture states are `Fetching`, `Preparing`, `Committing`, then `Succeeded`, `Failed`, or `Cancelled`. Failure kinds are Input, Security, Network, Content, Maintenance, Storage, and Cancelled, with a user message and bounded technical detail.
- The module owns input classification, URL normalization, safe fetching, readable HTML preparation, title precedence, effective base URL, source provenance, the hidden clipping Feed, immutable Article insertion, and same-transaction Article Library Projection.
- Title precedence is user override, page title, normalized original URL, then `未命名网页`. Repeated URL captures always create distinct Article identities.
- URL captures retain normalized original and final resolved URLs. Pasted HTML retains its optional effective base URL. `articles.url` remains the compatible original source field; `web_clippings` is authoritative for capture provenance.
- The existing blocking HTTP implementation becomes the production adapter behind a narrow internal fetch seam. It retains public HTTP(S)-only validation, credential rejection, DNS/IP and connected-peer checks, per-redirect validation, ten redirects, HTML-only responses, charset decoding, and an 8 MiB decompressed limit. Deterministic tests replace this true external dependency without introducing a generalized repository interface.
- The transition into `Committing` is the linearization point. Cancellation or Data Maintenance before it guarantees zero writes. Once it wins, cancellation reports `CommitAlreadyStarted` and the short SQLite transaction completes.
- Closing the Save Web Page Modal never owns terminal delivery. Before commit, dropping or explicitly cancelling the lease fences late results. During commit, the Modal may close immediately; desktop root state consumes the module's recent terminal snapshot by revision, adopts the returned projection, and presents the outcome.
- Permanent deletion uses `BEGIN IMMEDIATE`, confirms Web Clipping membership, calls Knowledge Processing's Article-target transaction seam, removes terminal Article Summary history, deletes the Article and its owned material, detaches independently curated Resources through the existing foreign key, and returns the requested Article Library Projection from the same transaction.
- Deletion is rejected while Article Summary is queued or running. Capture and deletion are rejected during Data Maintenance.
- This work does not add a new CLI capture interface. A future batch importer must use a separate adapter justified by a real caller rather than widening `CaptureLease` prematurely.

## Module seam and ownership

`web_clipping_lifecycle.rs` owns Web Clipping Capture, provenance, permanent deletion, observable state and maintenance participation. Its depth comes from hiding network security, HTML preparation, worker identity, cancellation races, transaction ordering and projection reconstruction behind a small GUI-first interface.

`web_clip.rs` remains the low-level production HTTP adapter because image loading and Resource Knowledge Processing also leverage its public-target safety rules. It does not own capture sequencing or persistence. `Article Library Lifecycle` exposes a crate-local `project_on` seam so capture and deletion reuse authoritative projection rules inside their own transaction. `Knowledge Processing` exposes `article_target::prepare_delete` so workflow table locality remains with its owning module.

Desktop Interaction owns Route, Modal and Notice. It renders lease snapshots and adopts complete terminal outcomes; it cannot fetch, prepare, save, delete, infer counts, or filter late capture results itself.

## Persistence and migration

Schema version 6 adds `web_clippings`, keyed one-to-one by Article identity. It records input kind, original URL, final URL, base URL, capture time, and whether provenance is complete or legacy.

Migration backfills every Article in the hidden Web Clippings Feed. Existing URL captures recover `original_url` from `articles.url`; existing pasted HTML captures recover their input kind. Unknown final and base URLs remain `NULL` with `provenance_state='legacy'`. The migration never infers redirect history or treats an embedded `<base>` as proven capture provenance.

## Verification contract

- URL and pasted-HTML capture preserve title and base precedence.
- Repeated URL captures produce distinct Articles and complete original/final provenance.
- Safe-fetch tests cover protocol, credentials, private/special IP ranges, response type, charset, redirects and decompressed size.
- Cancellation during Fetching or Preparing produces a terminal Cancelled snapshot and zero `web_clippings` rows.
- Data Maintenance interrupts pre-commit Capture without waiting for a blocked network thread; a Committing transaction must drain.
- Active Article Summary rejects deletion. Terminal history, Article-owned selections and metadata are deleted; linked Resources remain and are detached.
- Capture and deletion return authoritative Article Library Projection values read in their write transaction.
- The v5-to-v6 migration backfills only recoverable legacy facts and advances `user_version` atomically.

## Consequences

The GUI no longer owns web-capture worker channels, URL/HTML branching, HTML preparation, direct persistence, deletion SQL, or manual reload sequencing. Capture progress is explicit, closing the Modal cannot corrupt or lose a commit result, deletion cannot race active Article Summary work, and provenance is queryable without interpreting saved HTML.

The trade-off is deliberate SQLite coupling inside a deep module, one more versioned table, and short-interval GUI polling while a lease is active. Blocking DNS/connect work cannot always be physically stopped immediately; cancellation and maintenance guarantee logical termination and zero pre-commit writes rather than instantaneous socket destruction.

## Implementation review amendment (2026-08-27)

The lifecycle now acquires a short linearized mutation permit and opens its immediate transaction before publishing `Committing`. Maintenance intent rejects acquisition of any new mutation permit, while a permit acquired before that intent may validate and drain. Therefore `Committing` is the actual cancellation and maintenance linearization point: pre-commit cancellation guarantees zero writes, and an already-published commit cannot be invalidated between state publication and transaction acquisition.

## Implementation record

The Desktop Web Clipping feature adapter was extracted on 2026-08-30. `src/gui/web_clipping_feature.rs` now owns the Save Web Page and Delete Web Page drafts, modal rendering, capture admission and cancellation, delete lifecycle invocation, and user-facing error mapping. `GuiApp` retains the Route/Modal reducer, revisioned terminal Capture Lease observation, Article Library Projection adoption, selected-article reconciliation, and Notices. The adapter returns typed outcomes so Modal closure, projection acceptance, and selection cleanup remain explicit root effects. No Web Clipping Lifecycle ordering, cancellation, maintenance, or persistence semantics changed.

## Rejected alternatives

- Keep request ids and worker channels in `gui.rs`: preserves less code movement but keeps lifecycle ordering and late-result correctness in presentation code.
- Model capture as a persistent Knowledge Processing task: adds restart, retry, queue and cleanup semantics that a single interactive save does not require.
- Use one broad intent dispatcher for capture and deletion: minimizes method count but mixes asynchronous lease work with synchronous destructive transactions and weakens locality.
- Add a flexible blocking `CaptureSession` for hypothetical CLI and batch callers: widens the interface before a second real caller exists.
- Store provenance columns on every Article: pollutes RSS Article shape with Web Clipping-only facts.
- Infer legacy final URL or base URL from saved HTML: records guesses as provenance and cannot reconstruct redirect history reliably.
