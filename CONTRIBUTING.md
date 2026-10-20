# Contributing to the Stream Video Rust SDK

Thanks for helping improve Stream's Rust SDK. This repository contains the
server-side Stream Video REST client and the SFU WebRTC participant stack.
Changes for Chat, Feeds, Moderation, or frontend UI are outside this SDK's
scope.

## Local setup

Use Rust 1.88 or newer. A git checkout has no prebuilt codec archives, so the
first build downloads the pinned libvpx and libopus sources and builds them
with `configure` and `make`. This needs network access, a C compiler, `make`,
`perl`, and `nasm` on x86_64:

```bash
# macOS (arm64 needs no nasm)
xcode-select --install

# Debian / Ubuntu
sudo apt install build-essential perl nasm
```

The sources and the build stay in Cargo's `OUT_DIR`, so a later build reuses
them without network access. Each `OUT_DIR` has its own copy: `cargo build`,
`cargo test` and the editor's `cargo check` can each build the codecs once.

Clone the repository, then build all targets:

```bash
git clone https://github.com/GetStream/stream-video-rust.git
cd stream-video-rust
cargo build --all-targets
```

## Environment

The credential-free suite runs without local secrets. Live integration tests
and the examples load a local `.env` file:

```bash
cp .env.example .env
```

Add `STREAM_API_KEY` and `STREAM_API_SECRET` for Stream's live API. The Realtime
bot additionally needs `OPENAI_API_KEY`; its model can be selected with
`OPENAI_REALTIME_MODEL`. Never commit `.env` or include credentials in logs,
test output, issues, or pull requests.

## Run the examples

```bash
cargo run --example join_call
cargo run --example gpt_realtime_bot
```

## Verify a change

Run the relevant checks before opening a pull request:

```bash
cargo fmt --all --check
cargo build --locked --examples
cargo test --locked
cargo test --locked --doc
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo deny check
```

Pull requests also run the declared MSRV:

```bash
rustup run 1.88.0 cargo check --locked --all-targets
```

If you change the `include` list in `Cargo.toml`, run `cargo package --locked`
locally; it is what catches an `include` list that drops a file the build needs.

A scheduled workflow re-runs `cargo deny check` weekly against `main`. Security
advisories are published against dependency versions rather than commits, so
`main` can become vulnerable with no pull request open.

Live tests skip cleanly when their required credentials are absent. They use
real Stream and OpenAI services rather than mocks, so avoid running multiple
media suites concurrently against the same call.

## Performance baselines

The benchmarks are local capacity-planning tools, not timing-sensitive CI
gates:

```bash
cargo bench --bench media_baseline
cargo bench --bench timer_drift
```

Use benchmark results to evaluate a measured media-path change; do not treat a
single machine's timing as a portable SDK guarantee.

## Codec libraries

`build.rs` pins the libvpx and libopus sources (`LIBVPX` and `OPUS`: URL and
SHA-256). The published package carries prebuilt archives in `native/<target>`,
and `build.rs` links them when they are present. Git ignores `native/`: the
Publish workflow fills it from the Codecs workflow, which builds each target
with `build.rs` (glibc in manylinux_2_28, musl in Alpine, macOS on a macOS
runner). To update a codec:

1. Change the `LIBVPX` or `OPUS` source in `build.rs`. For libvpx, take the
   SHA-256 of the GitHub tag archive and check its commit with
   `gzip -dc <archive> | git get-tar-commit-id`. For opus, use the SHA-256 in
   `downloads.xiph.org/releases/opus/SHA256SUMS.txt`.
2. For libvpx, regenerate `src/rtc/codecs/vpx_sys.rs` with the bindgen command
   in `src/rtc/codecs/mod.rs`.
3. Run the Codecs workflow, put its artifacts in `native/`, run
   `cargo package --allow-dirty`, and check that the compressed size stays below
   the crates.io limit of 10 MB. The five targets use about 7 MB.

## Publishing

The crate name on crates.io is [`getstream`](https://crates.io/crates/getstream).
There is no `stream/` org prefix — ownership is whichever crates.io accounts
(and optional GitHub teams) you add as owners.

### First publish (local)

1. Sign in to [crates.io](https://crates.io) with GitHub.
2. Create a token at [crates.io/settings/tokens](https://crates.io/settings/tokens)
   with the `publish-new` scope, then run `cargo login` and paste it.
3. From a clean checkout of the release commit:

   ```bash
   cargo publish --locked --dry-run
   cargo publish --locked
   ```

4. On the crate's crates.io settings page, add other Stream maintainers as
   owners (`cargo owner --add <github-username>`). If the GetStream GitHub org
   has authorized crates.io, you can also add a team with
   `cargo owner --add github:GetStream:<team>`.

A version can never be overwritten. To pull a broken release from new
downloads, use `cargo yank --version <version>`.

### Later releases (GitHub Actions)

After the crate exists, configure Trusted Publishing on the crate settings
page: repository `GetStream/stream-video-rust`, workflow `publish.yml`, no
environment. That lets GitHub Actions publish without storing a crates.io
token.

Bump `version` in `Cargo.toml`, merge to `main`, then tag the same value:

```bash
# Cargo.toml version is 0.1.0-preview.1 → tag v0.1.0-preview.1
git tag -a v0.1.0-preview.1 -m "v0.1.0-preview.1"
git push origin v0.1.0-preview.1
```

The [Publish](.github/workflows/publish.yml) workflow verifies the tag matches
`Cargo.toml` and runs `cargo publish --locked`.

## Pull requests

- Keep changes focused and include tests for observable behavior.
- Preserve typed errors and deterministic async cleanup.
- Do not add mock-only tests for Stream protocol behavior.
- Run rustfmt, Clippy, doctests, and the relevant integration suite.
- Do not add generated credentials, `.env`, build artifacts, or benchmark
  output to a commit.
