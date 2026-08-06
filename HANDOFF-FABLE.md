# AutoHarness — handoff

You are taking over **AutoHarness** for iterative development. Read this, then
`HANDOFF.md` (the standing conventions and hard-won facts — still accurate
except where this file supersedes it), then `AGENTS.md`.

## Where things are

- **Repository:** the checkout you are reading this in
- **Branch:** `feature/diri-experience`, working tree clean
- **HEAD:** `735c497 fix: the empty execution pane actually closes`
- **Tests:** 576 passing, `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean, `cargo fmt` clean
- **Packaged app:** `target/dist/AutoHarness.app` (unsigned; self-update correctly disabled)
- **User data:** fresh database at `~/Library/Application Support/dev.autoharness.app/`.
  The user's ORIGINAL 1.8 GB of data was moved aside, NOT deleted, to
  `~/Library/Application Support/dev.autoharness.app.backup-20260806-072441`.
  Never delete that backup; the user disposes of it themselves.
- **diri reference clone** (Apache-2.0, code transfers verbatim — the standing
  instruction in `HANDOFF.md` §7 still applies): re-clone with
  `git clone --depth 1 https://github.com/cristicretu/diri /tmp/diri` if the
  scratchpad copy is gone.
- **bb reference clone:** `https://github.com/get-bb/bb` — **MIT, TypeScript/
  React**, so nothing transfers as code, only as design. Its settings surface
  (`apps/app/src/views/SettingsView.tsx` + `components/settings/`) is the
  pattern the user wants for ours: a full page with section navigation, not a
  sheet.

## What this session shipped (22 commits, `a56ec3a..735c497`)

Read the commit messages — each one records the failure it fixes and why. The
arc, compressed:

1. **Thread/worktree bug (the user's original complaint).** Switching engine
   mid-thread looked like starting over because it was: worktrees were keyed
   on the turn's run id, so every follow-up got a fresh tree off the base
   commit while the handoff brief claimed the previous work was on disk.
   Worktrees now key on the thread root (`thread_worktree_key`, daemon);
   `handoff::brief` states truthfully whether the tree was carried.
2. **Full handoff brief.** Per-section caps and 300-char clipping removed; one
   100k-char ceiling that ANNOUNCES what it drops; ancestry terminates on
   cycle, not on a silent count of 64.
3. **diri PTY engine transplanted** as `crates/pty` (pty, output log, headless
   VT emulation via alacritty_terminal, 20 agent manifests as embedded data,
   status reducer, redaction). `EngineKind` opened from a 2-variant enum to a
   validated manifest id. `PtyAdapter` in `crates/engines` drives any
   terminal-only agent through the same `EngineAdapter` contract — 18 engines
   register in production, cursor/droid/hermes/opencode/grok/kimi/pi verified
   ready on this machine.
4. **PTY agents are answerable and confined.** `run.answer` types the
   manifest's own approve/deny keystrokes ("1" for Claude Code, "y" for
   aider); prompt + Approve/Deny buttons render above the composer. Terminal
   agents run INSIDE `sandbox-exec -p <profile>` with per-session writable
   roots, from-scratch env (a secret-leak bug I introduced and fixed same
   session — see commit `79b699e`), rlimits, and fail-closed refusal when no
   confinement exists.
5. **Parallel verified attempts — the product bet.** `run.attempts` fans one
   objective across up to 8 engines/models, each in its own worktree on its
   own `ah/run-*` branch, all judged by the SAME check. `/attempts <objective>`
   in the composer races every ready engine. Comparison strip above the
   composer shows engine·model, files changed, verdict — an attempt whose
   check has not run reads "finished, not judged", NEVER as a pass. That
   distinction is the point of the whole feature.
6. **A pile of honesty fixes**, each with a regression test: the status line
   was written to a string only rendered while disconnected (the real cause of
   "I click and nothing happens"); `+` now creates a real dated draft and
   reuses an abandoned one; effort validated against the provider catalog at
   run.start; sandbox canaries run at launch and onboarding reports them;
   routing modes explain themselves; Parallel/Timeline hidden when identical;
   History out of the toolbar; Settings/Worktrees/History centred at 720px;
   model+effort persist per engine; reading an old run no longer clobbers a
   deliberate model pick; each transcript turn names its model; execution pane
   collapses when empty (needed the SEAM TARGET fixed, not just the mount —
   see `735c497`); model picker is one row per model with effort chips on the
   selected row (was 104 rows).

## Invariants added this session (on top of HANDOFF.md §3)

- A thread's turns share ONE worktree, keyed on the thread root.
- The handoff brief never claims work is on disk when the tree is fresh.
- A terminal agent with no sandbox confinement is REFUSED, never run loose.
  Only `allow_unconfined_for_tests()` bypasses it, deliberately ugly.
- An unjudged attempt is never rendered as a pass.
- Manifest ids are validated (`EngineKind::from_str`): no path chars,
  whitespace, or shell metacharacters, enforced at the deserializer too.
- A manifest naming a built-in by id or alias is not registered twice;
  an agent with no binary is not offered.
- `run.set_objective` is draft-only; a started run's objective is immutable.

## The user's open asks, in their priority order

1. **Settings as a full page** — opens over the whole window, has a back
   button, section navigation (dropdown), and search within it. bb's
   `SettingsView` is the named reference. Ours is currently a centred 720px
   overlay (`utility_overlay` in `crates/ui-gpui/src/lib.rs`,
   `surface_geometry`).
2. **Onboarding as a proper first-run flow** — real start screens, one page,
   for new users. Currently `crates/ui-gpui/src/onboarding.rs` renders a card
   inside the coordinator transcript. The card works (verified rendering in
   the real app); the user wants it to be the ENTIRE first-run surface.
3. **"If a repo is not chosen just make one when prompting"** — the user said
   this twice. I only improved the refusal message. UNRESOLVED DESIGN
   QUESTION I deliberately did not guess: does this mean `git init` a new
   directory automatically? Ask, or propose something explicit (e.g. a
   "Create a new repository…" option in the chooser dialog) — do not silently
   init directories on their disk.
4. **Transition animations** — user asked whether diri or bb has them worth
   taking. diri does: `crates/diri-app/src/seam.rs` (critically-damped seam
   motion — already partially ported into our `motion.rs`), overlay
   entry/exit fades, and tab transitions. bb has almost none (one 0.14s
   background ease). So diri is the source, and it's Apache-2.0 GPUI —
   transplantable. Our overlays already fade in; missing are exits,
   tab-switch motion, and the settings-page push.

## Known gaps and honest debt

- **`+` new-run popover has never been seen rendering.** The draft path is
  proven (SQLite rows, dated "New run · Draft" rows visible in screenshots).
  The popover itself I could not verify: `osascript` System Events clicks DO
  NOT REACH GPUI windows (they silently no-op — this invalidated several of
  my earlier claims until I caught it). A CGEvent-based clicker at
  `/tmp/click` (source `/tmp/click.swift`) does deliver real clicks; use it,
  not System Events. Also useful: the accessibility tree reads fine via
  `entire contents of window 1` when the app is frontmost.
- **`/attempts` verified through the daemon socket, not by hand in the GUI.**
- **A `project.add` fired on first launch with nothing clicked** (status line
  showed "request: invalid params: choose a folder inside a Git repository"
  on a fresh install). I never found the trigger — worth investigating.
- **No holder process.** Deliberate: a run is a bounded turn whose work
  survives in a preserved worktree; the recovery message now says exactly
  what survives. Rationale in commit `03731f8`. Don't build one without a
  new reason.
- Codex/Claude structured adapters do not implement `answer()` — returns
  `Unsupported` by design; their protocols have no out-of-band answer.
- Old blocked runs exist in the user's BACKUP data only; fresh db is empty.

## Working agreements observed this session (keep them)

- The user tests the packaged app, so finish every change with
  `./scripts/package.sh` and report the exact `.app` path.
- Gate before every commit: fmt, clippy `-D warnings` all targets all
  features, full workspace tests. Never report green without running.
- One commit per coherent change; the message says WHY. Update `NOTICE` when
  transplanting from diri.
- Screenshot the real running app (`screencapture -o -x`, downscale with
  `sips`) and READ the image before claiming a UI change works. Several real
  bugs were only ever visible that way.
- The user writes fast and loose ("jsut makd it") — read for intent, state
  your interpretation, flag genuinely destructive ambiguity (like the repo
  auto-create) instead of guessing.
- When the user asks "should we do X", answer with a recommendation and
  reasons, not a survey. They accepted "no PTY-as-coordinator" and "no
  holder" when argued concretely.

## First moves

1. `cargo test --workspace --all-targets` — confirm 576 before touching
   anything.
2. Launch `target/dist/AutoHarness.app`, look at the fresh-install state.
3. Take ask #1 (settings page) or #2 (onboarding flow); both are pure
   `crates/ui-gpui` work with bb as the design reference.
4. Resolve ask #3 with the user before implementing.
