# AutoHarness Production Completion Implementation Plan

> Execute task-by-task with `superpowers:executing-plans`. Every behavioral task starts with a
> failing focused test, then the smallest implementation, then focused and workspace verification.

**Goal:** Close every local end-to-end gap in the approved AutoHarness cockpit while preserving
the daemon-ledger safety model and exact true-black acceptance reference.

**Architecture:** SQLite owns durable intent; the daemon owns scheduling/recovery; `client.rs`
owns JSON-RPC projection; GPUI views render typed state and dispatch typed commands; macOS and
update effects sit behind narrow tested adapters.

**Baseline:** `cargo test --workspace` passes 379 tests at the start of this plan.

## Task 1: Durable objective and steering queues

**Files:** `crates/protocol/src/lib.rs`, `crates/store/src/lib.rs`,
`crates/daemon/src/lib.rs`, `crates/daemon/src/runner.rs`,
`crates/ui-gpui/src/client.rs`, new `crates/ui-gpui/src/queue.rs`, shell/navigation modules.

- [ ] Add typed queue kinds, states, rows, enqueue/list/move/cancel parameters and results, plus
  `max_active_runs` settings compatibility tests.
- [ ] Add migration v9 and store tests for atomic run+queue creation, stable ordering, pending-only
  moves/cancels, claim/complete, restart reset, and project purge.
- [ ] Replace `run.create -> run.start` UI submission with `run.enqueue`; retain direct RPCs for
  compatibility and tests.
- [ ] Persist steering before delivery, pass queue IDs to the runner, acknowledge safe-boundary
  delivery, and reload pending steering on restart/recovery.
- [ ] Add the daemon scheduler and integration tests covering capacity, terminal dispatch,
  cancellation, restart, duplicate request IDs, and event-before-broadcast order.
- [ ] Add the queue surface, toolbar count, compact composer strip, reorder/cancel actions, empty and
  error states, keyboard activation, and client projection tests.

## Task 2: Diri-quality motion with reduced-motion safety

**Files:** new `crates/ui-gpui/src/motion.rs`, `crates/ui-gpui/src/lib.rs`, `layout.rs`,
`components.rs`, `inspector.rs`, `workbench.rs`, `queue.rs`, `theme.rs`.

- [ ] Add pure curve/timing tests for 160 ms overlay, 190 ms tab, and monotonic 260 ms seam tokens.
- [ ] Animate overlay and tab entry with quint-out opacity/translation.
- [ ] Animate pane seams and queue insertion without animating layout-dependent paint properties.
- [ ] Route GPUI `reduce_motion` to an instant/no-spatial path and test both policies.
- [ ] Capture and compare the deterministic populated preview to
  `docs/reference/cockpit-acceptance.png` after the motion settles.

## Task 3: Packaged authentication and daemon lifecycle

**Files:** daemon auth/config module, `crates/ui-gpui/src/client.rs`, app/daemon binaries,
`scripts/package.sh`, integration tests.

- [ ] Write fake-backend tests for Keychain/file mismatch, one-backend failure, concurrent creation,
  and exact mode-0600 fallback permissions.
- [ ] Reconcile one token under an interprocess lock and repair stale backend copies.
- [ ] Log packaged daemon startup/exit failures to the AutoHarness data directory and surface a
  bounded diagnostic in the UI.
- [ ] Package companion binaries at fixed bundle paths and smoke a clean HOME/data directory:
  launch UI, authenticate, register repo, quit UI, confirm daemon remains healthy, relaunch.

## Task 4: Fail-closed blocked recovery and node controls

**Files:** `crates/daemon/src/worktree.rs`, `scheduler.rs`, `runner.rs`, `lib.rs`, core state tests,
store attempt queries, protocol types, UI control rendering.

- [ ] Add recovery tests for owned path success and path/root/repo/branch/removed-row rejection.
- [ ] Implement `resume_existing`, resume the same provider session, reload steering, and emit
  `run.recovered` before execution resumes.
- [ ] Add typed `node.retry`/`node.cancel` contracts and state/attempt tests.
- [ ] Add graph control channels/cancellation, dependent-subgraph retry, and daemon integration
  tests proving an actual node executes or stops.
- [ ] Expose retry/cancel only for daemon-reported controllable states and test keyboard/pointer
  parity.

## Task 5: Native attention, sound, and menu-bar rollup

**Files:** `crates/ui-gpui/src/notify.rs`, new `menubar.rs`, `attention.rs`, shell/client,
`crates/ui-gpui/Cargo.toml`, package metadata.

- [ ] Add injected platform tests for permission state, denial fallback, duplicate/replay
  suppression, sound preference, and priority rollup.
- [ ] Implement Notification Center request/post via `objc2-user-notifications` with no user-derived
  selector or executable input.
- [ ] Play a fixed system alert via `/usr/bin/afplay` and degrade silently if unavailable.
- [ ] Own/update an `NSStatusItem` on the main thread with unseen count and focus/open actions.
- [ ] Dogfood foreground/background completion, needs-you, denied-permission, and replay paths.

## Task 6: Signed update fetch, installation, rollback, and relaunch

**Files:** `crates/ui-gpui/src/update.rs`, new `bins/updater`, UI settings/update surface,
`scripts/package.sh`, release-feed tooling/tests.

- [ ] Add tests for pinned feed/asset origins, no redirects, digest mismatch, Team ID and bundle
  mismatch, active-run gate, staging paths, helper token, swap failure, and rollback.
- [ ] Fetch with fixed `/usr/bin/curl` arguments, verify the archive before extraction, and inspect a
  single app bundle in a private staging directory.
- [ ] Build the updater helper that re-verifies inputs, waits for the parent, atomically swaps,
  rolls back, and relaunches.
- [ ] Make Team ID/feed origin compile-time release inputs; render missing inputs as blocked rather
  than silently disabling verification.
- [ ] Package/sign/notarize app, daemon, and helper consistently and generate a signed-feed template.

## Task 7: Accessibility, focus, and recovery UX

**Files:** shell, toolbar, sidebar, coordinator, query editor, inspector, workbench, overlays,
queue, settings/history/worktrees, accessibility tests.

- [ ] Add an application role and stable IDs/roles/labels/descriptions for every interactive or
  status-bearing element.
- [ ] Bind Tab/Shift-Tab to visual focus order and make Enter/Space activate utility, settings,
  queue, graph, and confirmation actions.
- [ ] Make Escape close the topmost overlay/banner/switcher only; preserve Cmd-Q as the quit path.
- [ ] Add visible focus treatment, accessible error text, and reduced-motion-compatible status
  updates.
- [ ] Exercise the Accessibility Inspector and keyboard-only clean-workspace smoke path.

## Task 8: Quality gates, truthful docs, and final dogfood

**Files:** warning sites in engine adapters, `AGENTS.md`, `PLAN.md`, `docs/STATUS.md`, `NOTICE`,
reference screenshots, dogfood report.

- [ ] Clear all strict Clippy warnings without allowances that hide product code issues.
- [ ] Remove stale statements that call implemented settings/history/worktree/artifact behavior
  missing; document intentional PTY and external-write exclusions.
- [ ] Run formatting, strict Clippy, all workspace tests, package smoke, and signed-build checks when
  credentials are present.
- [ ] Use Computer Use against a clean repository to exercise create/queue/steer/cancel/recover,
  history adoption, settings persistence, worktree reclaim preview/confirm, attention, and update
  blocked/available states.
- [ ] Record exact evidence and any external credential acceptance dependency in `docs/STATUS.md`;
  do not call an unexercised integration complete.

