# AutoHarness — fork bring-up

Fork of `codejunkie99/autoharness` → `ReznorSkeath79/autoharness`.
Working copy: `/Users/ferdzlopez/Work/AutoHarness` (internal SSD).
Remotes: `origin` = our fork, `upstream` = codejunkie99.
Branch: `feature/diri-experience` (the repo's default — there is no `main`).

## Bring-up (2026-08-13) — DONE

- [x] Fork upstream repo and clone into `~/Work/AutoHarness`
- [x] Add `upstream` remote
- [x] Install Rust toolchain — was **not present** on this machine at all
- [x] Install Apple Metal Toolchain (688 MB) — GPUI's shader build script needs it
- [x] Add `x86_64-apple-darwin` target for the universal build
- [x] `cargo fetch --locked` (pulls Zed's tree for the pinned GPUI rev)
- [x] `cargo build --workspace`
- [x] Gate 1/4 — `cargo fmt --all -- --check`
- [x] Gate 2/4 — `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [x] Gate 3/4 — `cargo test --workspace --all-targets`
- [x] Gate 4/4 — `./scripts/package.sh`
- [x] Launch packaged `.app`, confirm daemon + socket + ledger
- [x] Socket round-trip via the `client_smoke` example

## Verified results

| Step | Result |
|---|---|
| Toolchain | rustc 1.97.1, rustfmt 1.9.0-stable (repo requires 1.97+, edition 2024) |
| Build | Clean, 2m09s cold |
| fmt | Exit 0, zero diffs |
| clippy | Exit 0 under `-D warnings` |
| tests | **606 passed, 0 failed, 1 ignored**, 20 test binaries |
| package.sh | Exit 0 — unsigned universal `.app`, 19 MB zip, 22 MB dmg |
| Binaries | `autoharness`, `autoharnessd`, `autoharness-updater` all `x86_64 arm64` |
| Bundle | `dev.autoharness.app`, v0.1.0, LSMinimumSystemVersion 14.0, adhoc signature |
| Runtime | App + daemon both up; `sandbox ready`; socket bound; `token_source=File` |
| RPC | `client_smoke` connected, listed 18 engines, replayed ledger |

The 606 figure matches the README badge exactly. The single ignored test is
`crates/pty/src/log.rs:613`, gated on a `DIRI_INTEROP_LOG` fixture — not
unfinished work. There are **zero** `todo!()`, `unimplemented!()`, `TODO`,
`FIXME`, `HACK`, or `XXX` markers in the tree.

## Engine detection on this machine

Detected by the live daemon:

- Installed: `claude` (2.1.229), `opencode`, `kimi`
- Not installed: `codex`, `gemini`, `cursor`, `aider`, `copilot`, `amp`, `droid`,
  `grok`, `devin`, `pi`, `kilo`, `kiro`, `qoder`, `hermes`, `antigravity`

Neither `codex` nor `claude` is a build-time dependency — both are resolved at
runtime and surface as diagnostics, never as build or registration failures.
Install `codex` only if we want to exercise the Codex app-server adapter.

## Not done — needs external inputs, not code

- **Signing / notarization** — needs a Developer ID identity, 10-char Team ID,
  and an Apple notary profile. Without them `package.sh` correctly produces an
  honest unsigned bundle and disables self-install.
- **Update feed** — needs `RELEASE_TEAM_ID` + `RELEASE_FEED_URL` at compile time
  plus hosting for the pinned HTTPS feed and ZIP.
- **Notification Center** — the packaged `.app` can register, but delivery still
  needs the macOS permission grant on first native post.
- **Screenshot verification** — `screencapture` returns "could not create image
  from display"; the terminal lacks macOS Screen Recording permission. Grant it
  under System Settings → Privacy & Security → Screen Recording to enable the
  visual acceptance loop against `docs/reference/cockpit-acceptance.png`.

## Open asks inherited from HANDOFF-FABLE.md

Upstream's own priority order for the next owner. All four are `crates/ui-gpui`:

1. **Settings as a full page** — currently a centred 720px overlay
   (`utility_overlay` / `surface_geometry` in `crates/ui-gpui/src/lib.rs`).
   Wanted: full-window, back button, section nav dropdown, in-page search.
2. **Onboarding as a real first-run flow** — currently a card inside the
   coordinator transcript (`crates/ui-gpui/src/onboarding.rs`). Wanted: the
   entire first-run surface.
3. **Auto-create a repo when none is chosen** — upstream flagged this as an
   UNRESOLVED design question and explicitly refused to guess. Do not silently
   `git init` on the user's disk. Decide the behaviour before implementing.
4. **Transition animations** — `motion.rs` has a partial port; missing exits,
   tab-switch motion, and the settings-page push.

Also called out as unverified upstream, worth confirming ourselves: the `+`
new-run popover has never been observed rendering, and a `project.add` fired on
first launch with nothing clicked.

## Local dev traps (from AGENTS.md / HANDOFF, confirmed relevant)

- **Never `cargo run --release -p autoharness`** locally — release reads the
  client token from Keychain and an ad-hoc binary trips a blocking consent
  dialog. Debug uses a 0600 token file. Our packaged run showed
  `token_source=File`, so it took the safe path.
- `pkill -f autoharnessd` between builds.
- GPUI is pinned to a Zed git revision, not crates.io. Bumping it is deliberate.
- Request ids are the daemon's idempotency key and the dedup cache outlives a
  connection — a client that restarts its numbering replays the previous
  session's answers.
- Seatbelt rules: use `/private/tmp` not `/tmp`, and `localhost` not `127.0.0.1`,
  or the rules silently fail to match.
- Live tests spend real tokens and are gated behind `AUTOHARNESS_LIVE_TESTS=1`.

## Review

The project builds, passes its own four-step gate at the numbers its README
claims, packages into a universal app, and runs — daemon, sandbox, socket,
ledger, and live engine detection all confirmed on this machine. No source
changes were needed; the two blockers were both missing local toolchain
(Rust, Metal), not defects in the repo.
