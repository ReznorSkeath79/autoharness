# AutoHarness — Handoff to Codex

You are taking over **AutoHarness**: a macOS-only, Apache-2.0, all-Rust coding-agent control
center. It accepts one objective, routes it to direct execution / bounded loop / swarm / DAG,
and drives the user's installed **Codex or Claude CLI** inside isolated git worktrees under a
Seatbelt sandbox. Read this, then `AGENTS.md` (conventions + hard-won facts) and `PLAN.md`
(the authoritative product plan).

## 1. State

- **Use the current checkout**, not the obsolete path or commit that used to
  live in this handoff. Confirm it with `git rev-parse --show-toplevel`.
- **Do not hand back less than** formatting, strict workspace Clippy, the full
  workspace suite, and the acceptance evidence recorded in `docs/STATUS.md`.
- **All local V1 phases are complete in code.** The phase tracker and external
  production dependencies are in `AGENTS.md` and `docs/STATUS.md`.

```sh
cargo build --workspace
cargo test --workspace
cargo run -p autoharnessd            # daemon (the UI starts it for you)
cargo run -p autoharness             # the app
./scripts/package.sh                 # universal .app + DMG
```

**Two build prerequisites that are not obvious:**

1. GPUI is a **git dependency pinned to a Zed revision**, not a crates.io release. Bumping the
   rev is a deliberate act.
2. It needs the **Metal toolchain**: `xcodebuild -downloadComponent MetalToolchain` (688 MB,
   one time). Without it `gpui_macos`'s build script fails on shader compilation and the error
   does not mention the missing component.

**Do not run `cargo run --release -p autoharness` locally.** Release builds read the client
token from the Keychain, and an ad-hoc-signed binary trips a blocking consent dialog. Debug
uses a 0600 token file. This is deliberate and documented.

## 2. Shape of the thing

```
crates/core       domain types; router, graph compiler, loop detector, policy guard — all pure
crates/protocol   JSON-RPC 2.0 envelopes, 4-byte length framing, method constants, DedupCache
crates/store      SQLite (WAL), migrations, append-only event ledger, replay, FTS5 memory
crates/engines    EngineAdapter contract, normalized events, Fake/Codex/Claude adapters
crates/daemon     socket server, run lifecycle, worktrees, sandbox, scheduler, planner, handoff
crates/ui-gpui    GPUI shell: client.rs (socket), theme.rs, palette.rs, query_editor.rs, fuzzy.rs, ansi.rs
bins/…            autoharness (app), autoharnessd (daemon)
```

**The UI is a thin socket client.** Nothing in `core`/`protocol`/`store`/`engines`/`daemon`
knows what draws pixels. That is what made replacing the renderer contained, and it is worth
preserving.

## 3. Invariants — breaking these is a regression, not a design choice

Full list in `AGENTS.md`. The ones people break:

1. **Persist before broadcast.** Every event hits the ledger before subscribers see it.
2. **Fail closed on the sandbox.** No sandbox, no proxy, or a failed canary ⇒ `run.start` is
   refused with structured diagnostics. There is no unsandboxed fallback.
3. **No external writes in V1.** No push, PR, merge, deploy, publish.
4. **The user's checked-out branch is never touched.** Every editing run works in its own
   worktree on `ah/run-*`; worktrees are reclaimed only when clean AND commit-free.
5. **Stale work is never reported as success.** Runs orphaned by a dead daemon are reconciled
   to Blocked in `Daemon::bind`, before the socket serves anyone.
6. **Engine processes spawn only** through `engines::process::spawn_json_lines_child`;
   daemon-owned commands only through `Sandbox::run_command`, daemon-owned git only through
   `Sandbox::run_bookkeeping`.
7. **Uncertainty routes DOWN.** A low-confidence or unsupported plan falls back to a bounded
   loop; nothing escalates to a swarm on doubt.
8. **A policy candidate may never touch a capability boundary**, and ceilings may only be
   lowered. Re-validated at promotion, not just at proposal.

## 4. Things that cost real time to learn — do not relearn them

**Reusing the user's login (this is the feature that makes the product usable):**
- Codex needs `CODEX_HOME` → the real `~/.codex`.
- Claude keeps credentials in the **macOS Keychain**, and reaching them needs BOTH `USER`/
  `LOGNAME` in the env AND `$HOME/Library/Keychains` — macOS resolves the keychain search list
  through HOME, so a relocated HOME hides the login. `SessionDirs::create_for_engine` links it.
- `CLAUDE_CONFIG_DIR` is deliberately NOT set; it relocates the expected `.claude.json` and
  does not restore the login.
- Detection asks each CLI through the **same sanitized env a session gets**, so it cannot claim
  an auth state the run will not have. `claude auth status` exits **1** while reporting a
  logged-in account; `codex login status` prints to **stderr**. Ignore exit status, read both
  streams.

**Seatbelt (verified by live probes):** reads must be broad with explicit canonical-path denies
(dyld needs broad reads; `/tmp` → `/private/tmp` or rules silently do not match); network rules
must say `localhost`, never `127.0.0.1`.

**Detection of loops:** the hard part is NOT catching loops, it is leaving productive work
alone. TDD repeats the same failing command on purpose; research reads twenty files without
editing. Every trigger is gated on an explicit absence of progress, and there are
false-positive fixtures for all four cases. **Do not add a trigger without one.**

**GPUI specifics:** scrollable containers need `.id()` before `.overflow_y_scroll()`. Rust 2024
makes `impl Trait` capture every in-scope lifetime, so views built from a borrowed `MutexGuard`
need `impl IntoElement + use<>`. Reserve the traffic-light lane with a flex-none **spacer**,
not padding.

**Request ids are the daemon's idempotency key** and its dedup cache outlives a connection. A
client that restarts its numbering gets the previous session's answers replayed at it. This
cost a day of "run.create returned no id".

## 5. Testing rules

- Automated tests use **FakeEngine**; real-CLI tests skip gracefully when the binary is absent.
- **Live tests spend tokens and are gated**: `AUTOHARNESS_LIVE_TESTS=1 cargo test -p
  autoharness-daemon --test live_direct_run`. Verified green on codex 0.143.0 and claude
  2.1.221. Never spend tokens in a normal run.
- Sandbox canaries run real `sandbox-exec`; daemon tests build real temp git repos. Expected.
- Test fixtures may shell out to git; product code may not.
- **Do not weaken a test to make it pass, and do not claim green without running it.**

## 6. What is actually left

No missing local product seam is intentionally parked here. Durable objective
and steering queues, blocked-worktree recovery, node controls, provider-history
adoption, persisted settings/usage, authoritative worktree reclaim, artifacts,
native attention, semantic accessibility, automatic replay-safe reconnect, and
the signed update verifier/helper are implemented.

Production distribution still needs inputs that do not belong in Git: the
owner's Developer ID identity and Team ID, Apple notarization profile, and a
hosted pinned HTTPS feed plus signed ZIP. Without them self-install correctly
stays disabled. Notification delivery also depends on the user's macOS
permission. Raw interactive PTY ownership remains an architecture exclusion:
providers speak structured JSON, while captured checks already render ANSI
output and classified unified diffs.

## 7. diri is the reference — treat it as such

**<https://github.com/cristicretu/diri>** (Apache-2.0) is the primary influence on this
product's interface *and* its feature set, and that is a standing instruction, not a one-off.
Clone it and keep it open while you work:

```sh
git clone --depth 1 https://github.com/cristicretu/diri /tmp/diri
```

It is the same shape of product — a native macOS app driving parallel coding-agent sessions
through a persistent daemon — and it is further along on interface than we are. **Both projects
are GPUI and both are Apache-2.0, so code transfers directly.** Do not reinvent what is sitting
there working.

The goal is that AutoHarness feels as considered as diri does: the same density, the same
restraint, the same "nothing on screen that is not earning its place". Match the *quality*.

### Transplant or adapt, per file

The one way to get this wrong is to paste their code and leave their vocabulary in it. diri
speaks of **sessions, agents, hosts**. We speak of **runs, threads, nodes, worktrees, engines**.
A port that still says "session" everywhere reads as a foreign object bolted on, and it will
confuse the next person about which model the product actually has.

Two shapes of reuse, both fine, pick per file:

- **Transplant verbatim** where the code is domain-free — `query_editor.rs` and `fuzzy.rs`
  came across unchanged, tests and all, and compiled first try. Anything that talks only to
  `gpui` and `std` is in this category.
- **Adapt** where their vocabulary is baked in. `theme.rs` and `palette.rs` are the worked
  examples: structure, metrics and algorithm kept, surface rewritten in our nouns.

Keep the `NOTICE` attribution as you go. Apache-2.0 permits all of this precisely because the
notice travels with it; that is a one-line obligation and it is what makes the reuse clean.

### Already taken

| Ours | From | What carried across |
|---|---|---|
| `theme.rs` | `diri-ui/tokens.rs`, `status.rs` | Type scale, radii, spacing, metrics, semantic colours, fills, status vocabulary, `Motion` + `AnimationPhase` |
| `palette.rs` | `diri-app/palette.rs` | ⌘K actions, penalized-keyword ranking, curated-order tiebreak |
| `ansi.rs` | `diri-proto/grid.rs` | `TermColor`/`TermStyle` model |
| sidebar | `diri-app/sidebar/view.rs` | 248px, threads nested under projects, badges, status marks, trailing state |
| `query_editor.rs` | `diri-app/query_editor.rs` | **Transplanted verbatim.** Caret, selection, word/line motion, readline bindings, clipboard |
| `fuzzy.rs` | `diri-app/fuzzy.rs` | **Transplanted verbatim.** `nucleo`-backed matching shared by the palette |

### Still there for the taking, roughly by value to us

1. **`history.rs`** — read-only discovery of existing **Claude and Codex transcripts** on disk.
   We already reuse the user's login; reusing their existing sessions is the natural next step
   and would make the app useful on first launch instead of empty.
2. **`worktrees.rs`** — a worktree sheet with a safe cleanup flow. We have a known GC gap (§6):
   failure paths deliberately keep worktrees, so `ah/*` branches accumulate with no way to see
   or clear them from the app. This is that feature, already designed.
3. **`notifications.rs` + `macos/notifier.rs`** — notify when a run needs you. We have the
   "needs you" state and nothing that tells you across app switches.
4. **`quick_open.rs`** — ⌘P folder indexing and ranking. `fuzzy.rs` is already in, so this is
   the indexing half only.
5. **`inspector.rs` + `seam.rs` + `workbench.rs`** — the trailing panel, its open/close motion,
   and pure layout state. Our three panes are fixed; theirs collapse and animate.
6. **`switcher.rs`** — Ctrl-Tab between threads, keyboard-first.
7. **`settings.rs`** — a real settings surface. We have none.
8. **`sounds.rs`** — status chimes, opt-in. Small, and it closes the "did it finish?" loop.
9. **`macos/menu_bar.rs`** — menu-bar rollup showing what wants you without raising the app.
10. **`updates.rs` + `diri-updater`** — a self-updater. We have the version comparison already
    (`core::version`) and stopped at the fetch; this is the other half.
11. **`usage/`** — token accounting with pricing. We record `engine.usage` and show none of it.

`diri-term` is the one thing NOT to take — see §6. It renders a cell grid produced by their
Swift daemon's PTY, and we have no PTY to produce one.

### The rules their tokens encode

Keep these even when you deviate from their layout:

- Text tone is **alpha over one foreground**, never a palette of greys — so a label is correct
  on any surface and there is no "which grey was that".
- Status colour is semantic and **"needs you" outranks everything**. Settled states draw as
  outlines so the eye lands on what is live or waiting.
- **Only what wants the user moves.** Motion that does not want you is noise.
- The type scale is three sizes separated by **weight**. A test asserts it stays three; adding a
  fourth size is how a dense tool becomes unreadable.
- Density over decoration. Their rows are 28pt with 8pt gaps for a reason.

### Looking at your work

There is no offscreen render harness any more (it went with the vello shell). To see a change:
run the app, `screencapture -o -x /tmp/x.png`, and read the image. Several real bugs — a tint
bleeding past a panel edge, a status chip colliding with its title, the window title sitting
under the traffic lights — were *only* ever visible that way and passed every test. **Look at
the thing before you call it done.**

## 8. First moves

1. Run `cargo fmt --all -- --check`, strict workspace Clippy, and
   `cargo test --workspace`; record the current counts rather than copying an
   old number from this handoff.
2. Read `AGENTS.md` end to end. It is long because each entry cost something.
3. `cargo run -p autoharness`, add a project, run something small, switch engines mid-thread and
   watch the handoff land.
4. Read `docs/STATUS.md` before changing a release gate; never weaken a
   fail-closed external dependency to make a local demo look complete.

**One commit per coherent change, with a message that says why, not what.** Update `AGENTS.md`
whenever you change conventions, structure, or phase status.
