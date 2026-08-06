# AutoHarness Diri Experience Transplant

## Decision

AutoHarness will adopt diri's operational completeness and interaction quality while preserving
AutoHarness's existing domain model and security architecture. The result must feel as dense,
quiet, immediate, and complete as diri without copying its branding or pretending that a run is
a PTY session.

The existing daemon remains authoritative. Every editing action still happens in an isolated
AutoHarness worktree, every event is persisted before broadcast, sandbox failure remains closed,
and V1 still performs no push, pull-request creation, merge to the user's branch, deployment,
publication, or other external write.

## Acceptance reference

`docs/reference/cockpit-acceptance.png` is the **exact** acceptance reference for the default
window, and `docs/reference/COCKPIT.md` states what it fixes. A change that makes the default
window disagree with that image is a regression, not a variation. The image supersedes any
earlier reference asset in this directory.

Two rules from the image survive into every surface it does not show: the surface stays black,
and no type falls below 11 px.

## Product outcome

The current fixed three-pane viewer becomes a retained, keyboard-first run cockpit:

- A true-black, resizable sidebar groups repositories and nested run threads.
- A coordinator surface keeps conversation and the objective composer primary.
- A collapsible execution workbench shows the route, graph, activity, checks, and recovery state.
- A collapsible inspector shows changes, artifacts, worktree details, engine, budget, and usage.
- History, worktrees, settings, quick open, command palette, switcher, overview, notifications,
  sounds, and the menu-bar rollup become first-class surfaces instead of hidden commands.
- Existing Codex and Claude transcripts can be discovered read-only and resumed as new
  AutoHarness runs with an auditable handoff.

Success means a user can find prior work, start or continue a run, leave the app, return when it
needs attention, review evidence, and safely clean preserved worktrees without remembering a
slash-command vocabulary.

## Reuse policy

Each diri subsystem is classified before porting:

1. **Transplant** domain-free GPUI or standard-library code, preserving its tests and Apache-2.0
   attribution. Examples: layout math, seams, fuzzy matching, query editing, quick-open ranking,
   switcher state, notification transition logic, sound synthesis, and shared visual components.
2. **Adapt** UI and state code that uses diri nouns. Sessions become AutoHarness run threads;
   agents become engines or graph nodes; hosts are omitted; diri worktrees become daemon-owned
   run/node worktrees; terminal metadata becomes checks, activity, and artifacts.
3. **Replace** an existing AutoHarness implementation when the adapted diri version is complete
   and tested. There will not be parallel legacy and new shells.
4. **Exclude** behavior that violates the product's contracts or lacks a valid data source.

`NOTICE` must continue to identify diri and name every substantially transplanted subsystem.

## Capability mapping

### Adopt in the first complete program

| Diri capability | AutoHarness implementation |
|---|---|
| Retained sidebar, draggable order, archive groups | Repositories with nested run threads, status priority, archive/unarchive |
| Workbench, inspector, animated seams | Resizable coordinator/execution/inspector layout with persisted dimensions |
| Command palette and Quick Open | Search actions, repositories, runs, and indexed local git folders |
| Ctrl-Tab switcher and overview | Run-thread switcher plus searchable status/engine/project overview |
| History discovery | Read-only Codex/Claude transcript scan and “continue in AutoHarness” handoff |
| Worktree sheet | List preserved run/node worktrees; open, inspect, and safely reclaim eligible ones |
| Notifications, sounds, menu bar | Needs-you, destructive blocker, completion, and resource-pressure signals. Delivered as a reducer, an in-app banner, and a toolbar rollup; the operating-system notification, the alert sound, and a true menu-bar extra are recorded as not done in `docs/STATUS.md` |
| Settings | General, execution, resources, notifications, privacy, and updates |
| Usage | Aggregate daemon `engine.usage` events by run, day, month, and engine |
| Inspector and diff | Changed files, unified diff, checks, artifacts, worktree, route, budget, usage |
| Update UI | Signed-release status and install flow owned by the app, not a general daemon HTTP client |
| Resource governor concepts | UI-visible limits and conservative cleanup/retention settings |

### Adapt instead of copying literally

- Diri's terminal pane becomes an AutoHarness execution pane. It renders structured engine
  activity, captured ANSI check output, DAG nodes, recovery evidence, and diffs. It does not
  claim to be an interactive terminal.
- Diri's session resume becomes either same-engine AutoHarness thread continuation or a new run
  with a deterministic ledger/history handoff. Provider session identifiers remain engine-bound.
- Diri's worktree removal becomes a daemon RPC with exact eligibility diagnostics. The UI never
  shells out to `git` for product-owned cleanup.
- Diri's artifact and pull-request concepts become local evidence and detected references.
  AutoHarness may display a URL already present in evidence but will not create or mutate remote
  resources.
- Diri's updater fetch runs in the UI process against a pinned signed feed. It cannot weaken the
  worker proxy or add general daemon egress.

### Intentionally excluded

- Arbitrary shell sessions, PTY ownership, terminal-grid protocol, and raw terminal input.
- Remote hosts, port forwarding, agent migration, and companion/mobile access.
- Diri's MCP tools that let agents spawn unconstrained sessions.
- Cursor, Gemini, generic shell, and manifest-defined agents until they implement the
  AutoHarness `EngineAdapter` contract and pass the same sandbox/authentication tests.
- Push, PR creation, deployment, publication, and any other external-write action.

These exclusions are product-boundary decisions, not temporary UI omissions.

## Architecture

### UI composition

`crates/ui-gpui` is split into focused retained entities instead of one immediate `Shell`:

- `app_shell`: window, top toolbar, global shortcuts, overlays, and focus routing.
- `sidebar`: repository/run projection, reorder state, archive sections, status priority.
- `coordinator`: transcript and objective/steering composer.
- `workbench`: pure split-layout state and the execution/graph/activity surfaces.
- `inspector`: changes, checks, artifacts, worktree, engine, budget, and usage tabs.
- `navigation`: command palette, Quick Open, and search editor.
- `switcher`: Ctrl-Tab and overview state machines.
- `surfaces`: history, worktrees, and settings sheets.
- `attention`: notifications, sounds, and menu-bar snapshots.
- `usage`: ledger-event aggregation and display formatting.

`client.rs` remains the socket boundary. It projects replay plus live events into immutable view
snapshots and emits typed commands. Visual components do not speak JSON-RPC directly.

### Daemon and protocol additions

The daemon gains only capabilities that must be authoritative:

- `run.archive` / `run.unarchive` and archived timestamps.
- `worktree.list` and `worktree.reclaim`, returning explicit clean/dirty/commit/ownership reasons.
- A bounded history-adoption command that creates a new AutoHarness run and records its source.
- Settings RPCs for daemon-owned resource and retention limits.
- Structured usage summaries only if replaying bounded usage events in the client becomes too
  expensive; client-side projection is preferred initially.

Every new mutation uses the existing request-id deduplication path, writes its event before
broadcast, and has replay tests.

### History data flow

The app scans the user's known Codex and Claude transcript roots read-only with strict byte,
entry, and directory caps. Results are metadata only: provider, source identifier, timestamp,
repository hint, and a sanitized title/first prompt. Choosing one creates a new AutoHarness run;
the original transcript is never modified. The daemon records the source and prepends a bounded,
deterministic handoff that tells the selected engine to verify inherited claims.

### Worktree cleanup flow

The daemon enumerates only branches and directories it owns. A reclaim preview reports:

- owning run/node and terminal state;
- dirty status;
- commits not integrated into the owning result;
- whether any process still uses the directory;
- the exact files/branch that would be removed.

Reclaim is enabled only when the existing invariant permits removal. Ineligible worktrees remain
visible with the blocking reason. No force-delete control is added.

## Visual system

Approved visual reference: [true-black AutoHarness cockpit](assets/autoharness-diri-black-reference.png).

The app is dark-only and genuinely black:

- Window and primary content: `#000000`.
- Raised persistent surfaces: `#050505` and `#0A0A0A` only when hierarchy requires them.
- Dividers and selection fills: white at low alpha; no independent grey palette.
- Text: one white foreground at semantic alpha levels.
- Semantic color is reserved for attention and engine identity: amber needs-you, red destructive
  blocker, green newly complete, restrained clay Claude, and restrained teal Codex.
- Type remains 11/13/15 pt with weight, not additional sizes, creating hierarchy.
- Rows remain 28 pt with 8 pt internal rhythm and 248 pt default sidebar width.
- Only needs-you, active work, and panel transitions may animate.
- The generated GPT Images concept is directional: its layout and black hierarchy are useful,
  but code must use real AutoHarness states and measured GPUI layout rather than raster imitation.

## Interaction model

- The top toolbar exposes route, engine, run state, overview/history/worktrees, search, alerts,
  settings, and pane toggles.
- Visible controls replace slash commands, while existing commands remain compatible.
- The composer switches among create, steer, and continue behavior based on the selected thread.
- Complex plans present one explicit review/approve control before execution.
- `Needs you` always outranks working, complete, and idle in sidebar and menu-bar rollups.
- Empty states each provide one direct next action.
- Keyboard navigation follows macOS conventions and every action is reachable without a pointer.

## Error handling

- Offline daemon state keeps history/settings readable and makes reconnect state explicit.
- Failed history parsing skips the entry and records a local diagnostic without blocking the
  surface.
- Worktree cleanup errors preserve the worktree and show structured reasons.
- Notification denial falls back to in-app banners and the menu bar.
- Update feed/install failure cannot prevent normal launch or run control.
- Unknown replay events remain visible as quiet activity rather than crashing a projection.

## Delivery sequence

The request is too large for one safe commit. It will land as coherent, always-green slices:

1. **Black retained shell:** new component boundaries, sidebar, coordinator, resizable workbench,
   inspector, shortcuts, and visual regression fixture.
2. **Navigation:** Quick Open, richer command palette, Ctrl-Tab switcher, and run overview.
3. **History:** capped transcript discovery, source projection, and auditable run adoption.
4. **Worktree operations:** authoritative list/reclaim RPCs and management sheet.
5. **Attention:** in-app banner, macOS notifications, sounds, and menu-bar rollup.
6. **Settings and usage:** persisted preferences, budgets/resources/privacy, usage aggregation.
7. **Review completion:** richer diff/check/artifact inspector and final empty/error states.
8. **Updater and release hardening:** signed feed in the app process, accessibility, packaging,
   and end-to-end validation.

Each slice replaces overlapping legacy code, updates `AGENTS.md` when a convention changes,
updates `NOTICE` for transplanted code, and ends in a focused commit.

## Testing and verification

Every slice must keep or improve the baseline of 233 tests and zero failures. Before completion:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo build --workspace`
- `cargo test --workspace`
- daemon replay/dedup/persist-before-broadcast tests for every new RPC/event;
- pure unit tests ported with transplanted layout, ranking, switcher, history, notification, and
  sound logic;
- UI interaction tests for focus, keyboard navigation, overlays, resizing, and destructive gates;
- screenshots of populated, empty, offline, needs-you, approval, running-DAG, failed-check, and
  completed-diff states inspected at common window sizes;
- a real debug-app walkthrough that adds a repository, creates/continues a run with FakeEngine,
  switches engines through a handoff, reviews changes, and exercises safe worktree cleanup;
- no live provider turn unless explicitly gated with `AUTOHARNESS_LIVE_TESTS=1`.

The existing `unused_mut` warning in `ansi.rs` is removed in the first production slice so the
repository returns to the handoff's zero-warning standard.

## Acceptance criteria

- The running app matches the approved true-black direction and does not resemble a recolored
  copy of a single diri screen.
- All adopted controls perform real typed actions; no decorative or dead UI ships.
- Existing runs replay correctly and active work survives client restart.
- Existing Codex/Claude history is discoverable without modifying provider files.
- Needs-you state reaches in-app, notification, sound, and menu-bar surfaces according to prefs.
- Preserved worktrees are visible and only safely eligible ones can be reclaimed.
- Diffs, checks, artifacts, worktree data, route, engine, budget, and usage are reviewable without
  leaving the app.
- No feature weakens sandbox failure behavior, touches the user's checked-out branch, or performs
  an external write.
- Workspace formatting, clippy, build, tests, and visual walkthrough are green at handoff.
