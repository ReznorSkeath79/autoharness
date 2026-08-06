# AutoHarness Production Completion

## Decision

Finish AutoHarness as a durable macOS run cockpit, not as a visual prototype. The existing
true-black GPUI shell stays; the daemon and SQLite ledger remain authoritative. This completion
pass closes the paths that still lose intent across a restart, fail to recover a preserved run,
or expose a control without an end-to-end implementation.

The approved cockpit image remains the default-window visual acceptance reference. Diri supplies
interaction and motion quality, while AutoHarness keeps its own nouns, safety boundaries, and
evidence model.

## Definition of complete

A local production build is complete when a user can:

1. register a repository, start multiple objectives, see excess work wait in order, reorder or
   cancel waiting work, and quit/relaunch without losing the queue;
2. steer an active run, with every follow-up persisted before delivery and retried after daemon
   restart rather than disappearing into an in-memory channel;
3. recover a blocked run only through its indexed daemon-owned worktree and the same provider
   session, with mismatches failing closed;
4. discover Codex and Claude history read-only, adopt an eligible session, and retain source
   provenance in the event ledger;
5. review streamed activity, ANSI check output, durations, diffs, artifacts, budgets, worktrees,
   and usage after an app restart;
6. receive replay-safe in-app, sound, Notification Center, and menu-bar attention signals;
7. operate every visible action by pointer and keyboard with stable accessibility roles, labels,
   focus order, modal Escape behavior, and reduced-motion support;
8. check a pinned release feed, verify a downloaded package and replacement bundle, install via a
   small signed helper only when no run is active, roll back a failed swap, and relaunch;
9. launch the packaged app beside its daemon/helper with one reconciled authentication token; and
10. pass unit, integration, package, and real UI smoke tests without relying on fixture-only state.

The signed-update acceptance test additionally requires the owner's Developer ID identity, Team
ID, notarization credentials, HTTPS release origin, and signed feed. The application must expose
that missing configuration as a blocked state; it must never weaken verification to simulate a
successful production update.

## Durable queue model

SQLite gains an ordered `queue_items` table. Each row has a stable identifier, kind
(`objective` or `steering`), optional run/project identifiers, bounded payload, state, explicit
position, timestamps, and failure detail. Valid states are `pending`, `dispatching`, `completed`,
`failed`, and `cancelled`.

- `run.enqueue` creates the run and objective queue row in one transaction. The daemon scheduler
  dispatches the oldest pending objective while active-run count is below the persisted limit.
- `chat.send` stores the chat message and steering queue row before notifying an active runner.
  A runner claims rows at safe turn boundaries. An acknowledgement completes the row.
- `queue.list`, `queue.move`, and `queue.cancel` are typed RPCs. Reordering applies only to pending
  rows. Cancellation is idempotent and cannot cancel a row already handed to an engine.
- Terminal run transitions complete the matching objective row and immediately schedule the next
  one. Startup reconciliation resets abandoned `dispatching` rows to `pending` and kicks the
  scheduler.
- Every queue mutation emits a persisted event before subscribers see it. The UI projects the
  queue from RPC results plus replayed events and never invents queue state optimistically after a
  rejected mutation.

The maximum concurrently active objectives is a persisted setting distinct from graph-worker
width. The default is two and is clamped to a conservative range.

## Recovery and graph controls

Starting a draft run creates a new worktree. Resuming a blocked run calls a separate
`resume_existing` path that accepts only a live worktree-index row owned by that run, beneath the
configured storage root, whose repository, branch, and Git top-level match the record. It never
creates a replacement silently. Dirty state is allowed because it is the work being recovered.

The daemon records `run.recovered` before resuming the provider session and any pending steering.
If a provider session cannot be reconciled, the run remains blocked with actionable evidence.

`node.retry` and `node.cancel` become typed, authoritative controls. They reject unknown or
non-controllable nodes, increment attempts, and only move states allowed by the core state
machine. A retry re-executes the node and the reachable dependent subgraph in isolated worktrees;
cancellation stops a pending/running node and marks downstream nodes unreachable. No UI-only
state change is accepted as success.

## Motion and accessibility

Motion is restrained and operational:

- overlays enter in 160 ms with opacity `0.76 -> 1` using quint-out;
- inspector/workbench tabs use a 190 ms directional 8 px slide plus opacity `0.70 -> 1`;
- sidebar, execution, and inspector seams settle over 260 ms with a monotonic critically damped
  curve and no overshoot;
- queue insertion and status changes use only opacity and translation; and
- `reduce_motion` bypasses spatial animation while preserving focus and status feedback.

GPUI's accessibility tree uses stable IDs and semantic roles for the application root, toolbar,
navigation lists, rows, tabs, text input, dialogs, status/banner content, and destructive
confirmations. Tab and Shift-Tab follow visual order. Escape closes the topmost transient surface
and never quits the app.

## Native macOS attention

The existing pure attention reducer remains the policy source. Platform adapters add effects:

- `UNUserNotificationCenter` requests permission only from a bundled app, posts typed summaries,
  and degrades to the in-app banner when denied;
- alert sound uses a fixed system sound path and never executes user-supplied commands;
- an `NSStatusItem` shows the highest-priority rollup and unseen count, with menu items that focus
  the relevant run or open AutoHarness; and
- replayed and duplicate ledger events never trigger OS effects.

Platform adapters are injectable so reducer and delivery behavior remain deterministic in tests.

## Authenticated packaged lifecycle

UI and daemon reconcile one token across Keychain and the mode-0600 fallback file. File and
Keychain disagreement is repaired deterministically under an interprocess creation lock before
either side connects. A daemon spawned by the packaged UI writes a bounded local log rather than
discarding its failure reason. The app locates companion binaries only from its own bundle or an
explicit development override.

The updater fetches only a compile-time pinned HTTPS feed and same-origin release assets. It
verifies SHA-256 before extraction, then bundle identifier, version, Developer ID Team ID,
`codesign --strict`, and Gatekeeper. A bundled helper waits for the UI to exit, performs an atomic
sibling swap with rollback, and relaunches through `/usr/bin/open`. It refuses symlinks, an active
run set, a destination outside `/Applications` or the current bundle parent, and any verification
token mismatch.

## Intentional exclusions

- Arbitrary shell/PTY ownership and raw terminal input remain excluded. AutoHarness provides a
  structured execution console, captured ANSI check output, diffs, and artifacts instead.
- Push, pull-request creation, deployment, publication, and other remote writes remain excluded.
- Provider auto-switching remains prohibited; a continuation stays on the original engine unless
  the user deliberately starts a new run.
- Unsigned self-install, redirect-following update downloads, force-removing worktrees, and
  unindexed recovery are prohibited.

## Verification strategy

Each slice is test-driven and committed independently. Completion evidence is:

- store migration, ordering, idempotency, restart-reconciliation, and purge tests;
- protocol round-trips for every new typed RPC;
- daemon integration tests for queue saturation, restart delivery, cancellation, blocked recovery,
  and node controls;
- pure UI tests for projection, focus routing, reduced motion, attention, and installer gates;
- `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets -- -D warnings`;
- `cargo test --workspace`;
- packaged universal-binary smoke with UI/daemon authentication and relaunch; and
- Computer Use dogfood of create, queue, steer, cancel, recover, history, settings, worktrees,
  attention, and update-blocked flows in a clean repository.

