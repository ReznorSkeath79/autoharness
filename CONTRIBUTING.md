# Contributing

## Setup

- macOS 14+
- Rust 1.97+ (edition 2024)
- Xcode, plus the Metal toolchain for the GPUI shell:
  `xcodebuild -downloadComponent MetalToolchain`

GPUI is pinned to a Zed revision in `Cargo.toml` rather than a crates.io
version. Do not "upgrade" it casually; the pin is what keeps the shell
building.

## The gate

Run all four, in order, before every commit. There is no CI to catch what you
skip — the shell needs a Metal toolchain, so the gate is local by design.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
./scripts/package.sh
```

`package.sh` is part of the gate, not an afterthought: it is the only step that
proves the release path still builds. An unsigned local bundle is fine.

A stale daemon holds the socket and will make a fresh build look broken. Kill
it between builds:

```sh
pkill -f autoharnessd || true
```

Live tests drive the real Codex/Claude CLIs and spend tokens, so they are gated
behind an environment variable:

```sh
AUTOHARNESS_LIVE_TESTS=1 cargo test -p autoharness-daemon --test live_direct_run
```

## What not to weaken

Some behaviour is load-bearing. Changing it needs a reason in the commit
message, not a passing test.

- **Uncertainty falls down, never up.** A low-confidence route proposal becomes
  `BoundedLoop`. `Swarm` and `DynamicDag` require human approval.
- **Fail closed.** No sandbox, no proxy, or a failed canary refuses
  `run.start` with diagnostics. Never add a fallback that runs anyway.
- **Persist before broadcast.** No event reaches a subscriber before the ledger
  has it.
- **The user's branch is never touched.** Editing runs work in their own
  worktree and branch.
- **Local only.** No external writes — no push, no PR, no deploy.

## Tests

Non-trivial logic leaves one runnable check behind. The convention here is a
test named after the bug it prevents, with a doc comment stating the symptom in
the user's terms — see `a_failed_run_says_why_in_its_own_transcript` and
`the_transcript_covers_every_turn_of_the_thread` in `crates/ui-gpui`. A test
called `test_thread_ids` tells the next reader nothing about what breaks if it
goes red.

## Commits

Present-tense subject describing the behaviour change, not the diff. If the
commit fixes a bug, the body says what the user saw, then what was actually
wrong. No `Co-Authored-By` trailers.

## Licensing

Apache-2.0 (see [LICENSE](LICENSE) and [NOTICE](NOTICE)). Contributions are
accepted under the same terms. Third-party code carries its own notice; do not
relicense it in place.
