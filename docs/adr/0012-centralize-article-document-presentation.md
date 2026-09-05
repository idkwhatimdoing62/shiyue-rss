# ADR-0012: Centralize Article Document Presentation

- Status: Accepted
- Date: 2026-08-26
- Decision owners: project maintainer and Codex implementation session

## Context

Article presentation was split between `text.rs` and a large immediate-mode rendering branch in `gui.rs`. `text::content_blocks` exposed the complete parser grammar as `Block`; the GUI reparsed the selected Article, merged inline runs, chose spacing and typography, rendered every block kind, maintained image and formula workers, mapped egui geometry back to selected text, and performed link side effects. A new HTML shape, media state, or selection rule therefore crossed several files and could invalidate unrelated GUI state.

The presentation path also lacked a stable document identity. Theme and viewport changes could legitimately relayout a document, while image and formula completion changed geometry asynchronously. Selection and Excerpt capture nevertheless need stable character meaning, and stale media events must not mutate a newer Article revision. Tests need to observe the complete behavior without depending on the private parser grammar or real network timing.

## Decision

Introduce `Article Document Presentation Pipeline` as the sole production Module for preparing and presenting one Article document.

- The external Interface is intentionally egui-specific. It accepts an immutable Article presentation request and `&mut egui::Ui`, performs the complete document presentation, and returns typed outcomes for selected text, link activation, selection restoration, and selection lifecycle. Media retry remains an internal presentation interaction. `GuiApp` no longer parses HTML, matches semantic block variants, constructs article galleys, or owns media state.
- The Interface does not expose a renderer trait, widget DSL, semantic `Block` collection, or `PreparedDocument`. egui belongs in this Module's external Interface because geometry, selection hit testing, viewport visibility, and immediate rendering form one cohesive desktop behavior. A second production renderer does not currently exist.
- `PreparedDocument` is private Implementation. It owns the fingerprinted title and article-specific semantic structure needed to assemble presentation runs, canonical plain text, links, and media references during a frame.
- Every prepared document is identified by a length-delimited SHA-256 content fingerprint over the exact presentation inputs: title, body HTML, and effective base URL. Equal fingerprints may reuse preparation even across equal-content Article records; Article identity remains part of the request and selection identity. A changed fingerprint creates a new document revision. Theme, viewport, scroll position, and media progress do not change the fingerprint or trigger HTML reparsing.
- Every selectable textual unit maps to a stable character range in the prepared document's canonical plain text. The offsets count Unicode scalar values, not UTF-8 bytes or egui galley positions. Relayout and image or formula completion may change rectangles but must not change the character range or extracted text for the same fingerprint.
- The Module owns inline-run assembly, article spacing, typography selection, headings, quotations, code, lists, captions, definitions, tables, formulas, images, document selection geometry, viewport-aware media scheduling, retry state, and rejection of stale asynchronous events.
- Image and formula work never blocks the egui frame. Media events carry content identity and update only Module-owned content caches. Selection drag state carries the document fingerprint and is discarded when the Article or its content changes, so late media completion cannot mutate selection meaning for a replaced document.
- Link activation is returned as a typed intent. The Pipeline does not launch a browser, persist an Excerpt, change Article state, write the database, or navigate a Route.
- Presentation failures are bounded and visible. Image failures retain user-facing text, technical detail, attempt count, and retryability. Formula failure preserves the TeX source as a readable fallback. Malformed HTML degrades to a readable partial document rather than panicking.

## Module seam and ownership

`src/article_document_presentation.rs` and its private submodules own the Interface, content fingerprint, prepared-document cache, semantic preparation, presentation policy, selection mapping, media coordinator, and egui Implementation. Existing HTML extraction logic may be moved from `text.rs`, but its intermediate types become private to this Module.

`GuiApp` supplies Article identity, title, body HTML, effective base URL, viewport, and interaction context, then adopts returned outcomes. The Pipeline owns article typography and presentation theme policy. GUI may retain Route-level scroll memory, but it does not infer document structure or reach into media caches.

Article Library Lifecycle continues to own bookmark, read-later, archive, read, Tag, and deletion state. Excerpt & Thought Lifecycle continues to own saved selections and Thoughts. Web Clipping Lifecycle and RSS Refresh Workflow continue to own how Article content enters storage. Desktop Interaction continues to own Routes, Modals, Popovers, and system-level effect execution.

## Dependencies and adapters

HTML parsing, semantic preparation, fingerprinting, selection mapping, egui presentation, image decoding, and MathJax execution are in-process or locally substitutable dependencies. `ImageStore` remains the concrete content-addressed local cache.

Remote image retrieval is a true external dependency behind a private `ImageFetch` Adapter. Production uses the existing guarded HTTP client; tests use deterministic fakes for cache hits and retryable failure-to-success transitions. The Adapter is private because there is one production policy and one test substitute, not a supported extension ecosystem. Formula execution uses private job/event machinery without creating a public generalized media interface.

## Verification contract

- The same content fingerprint prepares once across repeated frames and theme, viewport, and scroll changes.
- A changed title, body HTML, or effective base URL produces a new fingerprint and generation.
- Semantic text, canonical plain text assembly, and character ranges are deterministic for a fixed fingerprint.
- Selection extracts identical text before and after image or formula completion and across relayout at different viewport widths.
- Long documents are assembled into bounded Galley groups at canonical paragraph boundaries rather than forming one Article-sized Galley or one widget per paragraph. A group contains at most 16 paragraphs or 1,600 characters (except an indivisible single paragraph). Document geometry must remain stable when the viewport offset changes; presentation must not replace estimated offscreen heights with measured heights during scrolling.
- Ordinary scrolling submits paint and selection geometry only for viewport-near text groups. Offscreen text keeps its exact allocated height and canonical character range without producing draw shapes; an explicit Excerpt restoration may materialize complete geometry for one frame so it can locate an offscreen anchor.
- Media work is queued only by presentation visibility policy, never performed synchronously in an egui frame, and duplicate in-flight work is coalesced.
- A late media event can update only its content-keyed cache entry and cannot alter the active document identity or selection state. Explicit retry has observable loading, success, and failure transitions.
- Link interaction returns intent without invoking the operating system in Module tests.
- Real HTML fixtures cover headings and inline runs, nested lists, links, tables, definitions, captions, broken markup, relative URLs, images, formulas, and long mixed-language text.
- egui frame tests exercise hit testing and selection through the external Interface. Parser-only tests remain only where they verify a private invariant not observable through presentation.
- The crate root exposes no parser Module. `Block` and every supporting grammar type are visible only to the parent Article Document Presentation Module; GUI, Knowledge Processing, and Web Clipping callers cannot name or match them.

## Migration

1. Add the Module facade and content-fingerprinted prepared-document cache while delegating semantic preparation to the existing parser.
2. Move the exhaustive `Block` rendering branch, inline assembly, article spacing, and typography into the Module without changing visible behavior.
3. Move article selection frame, galley mapping, and text extraction behind stable document character ranges.
4. Move image and formula caches, workers, event draining, viewport scheduling, retry, and content-keyed late-event isolation into the Module; introduce the private `ImageFetch` Adapter and fake.
5. Replace browser calls and Excerpt capture hooks with typed outcomes adopted by Desktop Interaction and Excerpt & Thought Lifecycle.
6. Privatize `PreparedDocument` and parser grammar, remove obsolete GUI fields and helpers, and replace their shallow tests with Interface-level fixture and egui-frame regressions.

Each step must preserve a buildable application and the existing reading behavior. Old helpers are removed only after their callers and coverage have moved.

## Implementation record

Implemented on 2026-08-26. `src/article_document_presentation.rs` now owns the egui facade, bounded fingerprint cache, exhaustive semantic rendering, inline/list assembly, Unicode selection mapping, selection-revision guard, image and formula coordinators, and the private `ImageFetch` production/test seam. `GuiApp` invokes one `show` operation and adopts typed outcomes; its former `Block` match, galley helpers, selection frame, image/formula workers, and presentation-specific tests were removed. Regression coverage now lives with the Module and includes fingerprint reuse/revision, mixed real HTML at two viewport widths, Unicode quote offsets, mixed-language spacing, inline-code typography, deterministic retry, offline cache behavior, bounded long-document Galley size and layout-call count, stable document height across real `ScrollArea` offsets, and offscreen Excerpt restoration. An initial viewport-virtualization optimization was removed after its relative viewport coordinates were compared with absolute widget coordinates and its estimated heights caused visible document reflow. The replacement groups up to 16 paragraphs or 1,600 characters per Galley, preserving stable geometry while bounding both Galley size and per-frame widget count.

Parser privatization was completed on 2026-08-27. The crate-root `text` Module was removed and its semantic grammar moved to `article_document_presentation::parser`, whose widest visibility is `pub(super)`. `Block`, inline ranges, table cells, definitions, HTML snapshot internals, and parser helpers can now be named only by the parent Module and its private tests. Storage preparation and search snippets cross the external seam through the bounded `prepare_article_html` and `article_visible_text` projections; they do not expose parser state or grammar. Generic timestamp formatting was removed from the parser and kept local to Desktop GUI. A structural regression test prevents restoration of the crate-root parser or direct GUI, Knowledge Processing, and Web Clipping grammar access.

The scrolling paint path was tightened on 2026-09-01 after a deterministic long-document frame still submitted 81 draw shapes while positioned deep inside the Article. Text groups now preserve full canonical text and exact layout height while retaining rendered selection geometry only near the viewport. Cursor identity uses canonical character offsets rather than frame-local span indexes, so viewport culling does not invalidate a selection across frames. Regression coverage bounds the long-document paint budget while retaining stable document height and offscreen Excerpt restoration.

The repeated-frame selection path was tightened on 2026-09-01. Once a prepared Article fingerprint has assembled its canonical selection text, subsequent frames reuse that text while rebuilding only viewport-dependent geometry. This removes a full-document `String` reconstruction during ordinary scrolling without changing character offsets, excerpt restoration, or selection intents; the cache retains only the active fingerprint to keep memory bounded.

The ordinary-scroll layout path was tightened on 2026-09-01 as well. Measured text heights are retained for the active fingerprint, text style, and wrap width. A known-height offscreen block now allocates its stable geometry without invoking font layout; a Galley is materialized only when the block is near the viewport or full geometry is explicitly requested for Excerpt restoration. The cache is reset on content revision or width/style identity changes, so asynchronous media and relayout semantics remain unchanged.

Image relayout anchoring was tightened on 2026-09-01. When an asynchronously completed image changes height above the current viewport, the Pipeline reports the exact delta and Desktop Interaction applies it to the ScrollArea state on the next frame. Images intersecting or below the viewport do not adjust the user's position; document content and natural image sizing remain unchanged.

## Consequences

Article HTML is no longer reparsed every frame, asynchronous media cannot silently change selected text, and all article-specific presentation decisions have strong Locality. The Module has greater Depth: a compact egui Interface hides preparation, layout policy, selection coordinates, media concurrency, retries, and stale-event handling. Tests can replace the only true external dependency and assert complete presentation behavior without coupling to parser internals.

The trade-off is deliberate coupling between this Module's external Interface and egui. A future non-egui renderer cannot reuse the Interface directly, although it may reuse private preparation logic after a new real use case justifies a second Seam. Content fingerprints and retained prepared documents also consume memory, so the Implementation must keep a bounded cache and evict inactive generations.

## Rejected alternatives

- Keep `Vec<Block>` as the GUI Interface: leaks parser grammar, forces exhaustive GUI changes, and keeps preparation, layout, selection, and media behavior distributed.
- Put egui behind a generic renderer trait now: creates a shallow abstraction with one production Implementation and still leaks layout observations needed for selection.
- Expose `PreparedDocument` for testing or future renderers: makes a private semantic representation a compatibility contract and reduces freedom to improve preparation.
- Cache only by Article id: can present stale content after refresh or local web-clipping replacement.
- Use UTF-8 byte offsets or galley cursors as durable selection coordinates: offsets change with encoding details or relayout and cannot safely support Excerpt capture.
- Let the GUI own image and formula workers: preserves the current cross-cutting state and stale-event risks.
- Make the Pipeline persist selections or Article state: combines presentation with Article Library and Excerpt & Thought lifecycle transactions.
