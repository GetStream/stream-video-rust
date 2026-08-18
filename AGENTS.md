# AGENTS.md

This repository is the Rust SDK for Stream Video: server REST APIs, common
types (users, tokens, events, and webhooks), and the WebRTC SFU participant
stack. The RTC stack is always compiled. Chat, Feeds, Moderation, and frontend
UI are out of scope unless the task explicitly expands the project.

## Working in this repository

- Before editing, inspect the worktree and current branch. Create or switch to a
  task branch unless the user explicitly says otherwise, and preserve unrelated
  changes.
- Keep changes focused and follow existing public API and error conventions.
- Do not report `SDK_TYPE_GO` to the SFU. Use the Rust client identity and the
  versioned `stream-rust-x.y.z` header.
- Never inspect, modify, or commit `.env`, credentials, generated secrets, or
  logs containing tokens. `.env.example` is the public configuration reference.

## Rust and async expectations

- Ensure [rust-skills](https://github.com/leonardomso/rust-skills) is available
  before coding or review. Install or refresh it with
  `npx add-skill leonardomso/rust-skills`, then apply the relevant rules rather
  than loading unrelated categories.
- Prefer typed errors and `Result`; avoid panics and `unwrap` in library code.
- Keep queues and buffers bounded. Make background-task cancellation and cleanup
  deterministic, and do not hold synchronous locks across `.await` points.
- Preserve compatibility with the Rust version declared in `Cargo.toml`.

## Upstream compatibility

Do not implement wire behavior from memory or a stale checkout. Before changing
an area below, fetch or inspect the latest upstream default branch and compare
the relevant source:

- Protobuf and SFU wire types: `GetStream/protocol`,
  `protobuf/video/sfu/{event,models,signal_rpc}`. Keep the vendored files in
  `proto/video/sfu` current with upstream.
- REST names and server behavior: `GetStream/getstream-go`.
- Join, reconnect, errors, and signaling behavior: `GetStream/stream-video-js`,
  especially `packages/client/src/Call.ts`.


## Tests

- Use unit tests for deterministic local behavior such as validation,
  serialization, codecs, and state transitions.
- Stream protocol behavior belongs in live integration tests, not mock-only
  tests. Live tests must use unique resources, skip only when credentials are
  absent, and clean up calls and tasks on success, failure, and timeout.
- Keep tests behavioral: avoid endpoint-call smoke tests, duplicate coverage,
  timing-sensitive CI assertions, and assertions that merely restate an
  implementation detail.

Run checks appropriate to the change, with the full gate before handoff:

```bash
cargo fmt --all --check
cargo test --locked
cargo test --locked --doc
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

The README is the crate-level doc (`#![doc = include_str!("../README.md")]`), so
its links must be absolute — a relative one breaks the docs.rs build.

Use small conventional commits (`feat:`, `fix:`, `test:`, `docs:`, `chore:`), create and
push the working branch, and never force-push or push directly to `main` unless explicitly requested.
Never add a `Co-authored-by: Cursor` trailer, or any other Cursor authorship line, to commits.