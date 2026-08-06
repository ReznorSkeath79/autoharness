# Implementation status

What is built, what is verified locally, and what is blocked on something
outside the code. Updated on the `feature/diri-experience` branch.

## Verified locally

These run and pass on a developer machine with `cargo test --workspace`.

| Area | State |
|---|---|
| Exact black cockpit shell | Complete. `docs/reference/COCKPIT.md` is the acceptance reference. |
| Structured run projection | Complete. Per-run messages, activity, checks, files, artifacts, worktree, budget, usage. Background runs keep their own state. |
| Execution targeting | Complete. Native canonical Git-repository selection, token-free provider model catalogs, atomic model/reasoning selection, per-run SQLite persistence, queue idempotency, provider launch wiring, and replayed inspector evidence. |
| Execution graph | Complete. Exact edge flow, provider task objectives, dependency/scope/check metadata, selectable node detail, live status/progress, and typed retry/cancel controls. |
| Navigation and accessibility | Complete. Overview, palette, Ctrl-Tab switcher, visual-order Tab/Shift-Tab traversal, Enter/Space activation, modal focus ownership, stable semantic roles and labels, and VoiceOver actions that dispatch through the same typed paths as pointer input. |
| Provider history | Complete. Read-only metadata indexing, grouping, pagination, explicit adoption, atomic concurrent adoption, purge. |
| Persisted settings | Complete. Eleven fields in SQLite, request-id idempotency, conservative clamping, persist-before-broadcast. |
| Usage summary | Complete. Ledger fold with Codex cumulative and Claude delta semantics. No cost is reported, because none is known. |
| Worktree index and reclaim | Complete. Fail-closed against a SQLite index; ten preconditions; never `--force`. |
| Artifacts and check details | Complete. Bounded evidence references, persisted before broadcast, replayed on restart. |
| Attention | Complete in code. Replay suppression, in-app banner, toolbar and native menu-bar rollups, a status menu that preserves the highest-priority alert and opens AutoHarness, Notification Center delivery with click-to-run routing, and fixed opt-in sound. |
| Signed updates | Complete in code. The UI fetches only its compile-time pinned HTTPS feed, stages privately, verifies the archive and app, and hands an authenticated request to the independent swap-and-rollback helper. |

## Depends on a macOS permission

**Operating-system notifications.** Native delivery is implemented through
GPUI's `UNUserNotificationCenter` adapter. Background ledger events enter a
bounded queue, the main thread asks macOS to deliver them, and clicking a
notification focuses AutoHarness and selects the related run through the same
typed action used by the in-app panel. The native menu-bar item displays the
highest-priority unseen item and count. Replayed items are already seen, so
they do not create a notification, sound, or menu-bar badge.

An unbundled `cargo run` still cannot register with Notification Center;
`MacNotificationSink` reports `Unavailable` there and the in-app banner remains
the fallback. The packaged `.app` has the required bundle identifier and asks
for permission on first native post. If the user denies or revokes permission,
the banner and both in-app attention surfaces still retain the alert.

The opt-in sound uses `/usr/bin/afplay` with one fixed system sound path. It
never invokes a shell, never accepts user input, and suppresses overlap while a
previous alert is playing.

## Depends on signing and a hosted feed

**Production update acceptance.** The complete update path now exists:

1. The release build embeds one fixed HTTPS `.json` feed URL and ten-character
   Developer ID Team ID. Builds without both truthfully disable updates.
2. `/usr/bin/curl` fetches the exact feed without redirects or user config,
   with protocol, time, and size bounds. Assets must be same-origin HTTPS ZIPs.
3. The app creates a private staging directory, verifies the SHA-256 digest,
   rejects symlinks and ambiguous archives, and accepts exactly one
   `AutoHarness.app`.
4. The staged bundle must match the pinned bundle ID, numeric version, Team
   ID, `codesign --verify --deep --strict`, and `spctl --assess`. The currently
   running app must pass the same identity checks.
5. A separately signed helper receives a mode-0600, single-use authenticated
   request, independently repeats the safety checks, refuses while a run is
   active, atomically swaps the bundle, relaunches it, and restores the prior
   bundle if either swap or relaunch fails.

The fetch, staging, verifier, helper, rollback, and refusal paths have local
automated coverage. `scripts/package.sh` builds the universal helper inside the
app, signs inner binaries before the bundle, notarizes and staples the app,
creates the final ZIP and DMG, and emits the SHA-pinned feed record.

A real production update cannot be accepted from this checkout alone. It
still needs the owner's Developer ID signing identity and Team ID, an Apple
notarization profile, and hosting for the pinned feed and ZIP. Without those
external release inputs the Settings row says that updates are unavailable
and never exposes an install action.

## Quality gate

`cargo clippy --workspace --all-targets -- -D warnings` is clean and
`cargo test --workspace --quiet` passes all 438 tests. `scripts/package.sh`
produces universal arm64 + x86_64 app, daemon, and updater binaries plus the
local ZIP and DMG.

The packaged app was also exercised through macOS accessibility against a
fresh Git fixture and isolated app-data home. It launched without a Keychain
dialog, resolved the repository, loaded the provider model/effort catalog,
accepted text through both the native keyboard and accessibility SetValue
paths, and ran a real Claude/high objective. The run created one exact
`ok\n` file, committed it on its isolated branch, persisted the diff and file
artifacts, recorded 4 input + 202 output tokens, and reconstructed the
completed cockpit from the ledger after relaunch. The clean-home acceptance
run also caught and fixed sandbox-home resolution, overlong Unix sockets,
interactive Keychain fallback, and a late-settings execution-label race.

This document does not treat source presence as runtime proof. Production
signing/notarization, a hosted signed feed, and the user's Notification Center
permission remain the external acceptance items described above.

The tree is formatted with rustfmt 1.9.0. An earlier version formatted parts
of it differently; the reformat is its own commit so it does not obscure the
feature commits.
