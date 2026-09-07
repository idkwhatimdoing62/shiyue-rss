# ADR-0015: Network Mode and Public Target Validation

- Status: Accepted
- Date: 2026-09-05
- Decision owners: project maintainer and Codex implementation session

## Context

Some desktop proxy clients route DNS through a TUN interface and return
synthetic addresses from `198.18.0.0/15`. Treating every non-public answer as
unsafe made otherwise reachable RSS feeds, web clips, and article images fail
when the proxy was enabled. Relaxing validation globally would re-open local
network access and DNS-rebinding risks.

## Decision

Add a persisted `NetworkMode` setting with two values:

- `Strict` (the default): only public destinations are accepted.
- `TunCompatible`: hostname answers in the proxy's `198.18.0.0/15` synthetic
  range are accepted so the proxy can translate them; all other private,
  loopback, link-local, special-use, and documentation ranges remain blocked.

The relaxation applies only to DNS answers and the connected peer. An
explicit IP literal is always validated as written, so `127.0.0.1`,
`192.168.x.x`, and synthetic literals remain rejected even in TUN mode.

RSS, web clipping, article-image loading, and Resource Knowledge Processing
construct their clients through the same mode-aware resolver and redirect
policy. Every redirect hop and the final response target remain subject to the
same validation. The GUI exposes the mode under 资料库管理 → 网络访问 and
persists it through Desktop Runtime settings; a restart is required because
workers create their clients at startup.

## Consequences

Users with a TUN proxy can load proxy-resolved feeds and images without
disabling SSRF protections. The opt-in mode is intentionally narrow and does
not make arbitrary private hosts reachable. Existing strict APIs remain as
compatibility wrappers, while production adapters pass the persisted mode
explicitly.

## Verification

- Unit tests cover synthetic-address recognition and explicit literal rejection
  in both modes.
- RSS, web clipping, image, and Knowledge Processing paths share the resolver
  and mode-aware redirect validation.
- `cargo test --locked`, strict Clippy, and release builds pass.
