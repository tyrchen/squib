# Compat-suite transcripts

This directory holds **machine-readable** transcript fixtures for the compat suite —
`*.json` files that name the upstream getting-started / api_requests sequence as a
list of `(method, path, body)` request / `(status, body_substr)` expectations.

## How transcripts are consumed

Each transcript is loaded by a Rust integration test in `tests/`, which:

1. Spawns a [`crate::CompatServer`] under a per-test UDS.
2. Drives every step in order through the same `axum::Router` the production
   `squib` binary uses.
3. Asserts each response matches the declared expectation.

For now the per-row tests in `tests/{f,p,a,r}_rows.rs` and
`tests/getting_started.rs` are written as inline Rust (no JSON loader yet) — the
`Transcript` type in `src/transcript.rs` is the same data shape these files would
deserialise into. Adding a JSON loader is a small follow-up and is documented as a
P3 item in [`specs/93-improvements-review.md`](../../../specs/93-improvements-review.md).

## Provenance

The request/response pairs are derived from upstream Firecracker's
`docs/getting-started.md` and the `docs/api_requests/` examples (Apache-2.0). Squib
adjusts paths / IDs to aarch64 conventions and records every documented deviation
from [`specs/21-api-compat-matrix.md`](../../../specs/21-api-compat-matrix.md).
