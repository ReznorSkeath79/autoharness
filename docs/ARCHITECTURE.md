# Architecture

How AutoHarness is put together, and why each part is shaped the way it is.
[README.md](../README.md) has the diagrams and the crate map; this is the
reasoning behind them.

## Three processes, one owner of side effects

| Process | Binary | Owns |
| --- | --- | --- |
| Desktop shell | `autoharness` | Rendering, input, local view state |
| Daemon | `autoharnessd` | SQLite, git worktrees, engine processes, sandbox |
| Update helper | `autoharness-updater` | Atomic swap, rollback, relaunch |

The shell never opens the database and never spawns an engine. It speaks
JSON-RPC 2.0 over a Unix socket under
`~/Library/Application Support/dev.autoharness.app/`, mode `0600`, with a
peer-UID check and a Keychain-held client token. One owner of side effects
means one place to audit, and it means the shell can crash, be killed, or be
replaced by an update without a run noticing.

The shell starts the daemon if it is not already running. Killing the app does
not kill the work.

## Persist before broadcast

Every event is written to the append-only `events` table before it is sent to
any subscriber. There is no path that broadcasts something the ledger does not
already contain.

That ordering is what makes reconnection ordinary rather than special: a client
calls `events.subscribe` with `since_sequence`, receives replay, then live
events on the same channel. A shell that was closed for an hour and a shell
that never disconnected converge on the same state by the same code path.

Event rows carry `seq` (global, monotonic), `run_id`, `run_seq` (per-run
ordering), `timestamp_ms`, `kind`, and a JSON `payload`. Envelopes carry
`protocol_version` (currently `1`) so a version skew is a structured error, not
a misparse.

## State machines return errors

`RunState` and `NodeState` are explicit tables in `crates/core`. The legal set
is written out as one `matches!` pattern, and `transition` returns
`Result<_, TransitionError>`. An illegal transition is a value you can log,
persist, and show — never a panic that takes the daemon down mid-run.

The full run table is drawn in the README. Two edges are worth naming:

- `Draft → Blocked` exists so a missing engine, an unavailable sandbox, or a
  worktree that cannot be created stops the run *before* it starts, with
  diagnostics attached.
- `Blocked → AwaitingApproval` is how a recovery path re-enters approval when
  the recovery produced a newly compiled plan. Recovering is not a licence to
  skip the gate.

## The router falls down, never up

`crates/core/src/router.rs` is pure: no IO, no engine call. The daemon gathers
`TaskFacts` — measured repository and objective facts, not model opinions —
optionally asks an engine for a `GraphProposal`, and calls `route`.

The bias is one-directional and deliberate. A low-confidence proposal becomes
`BoundedLoop`, never `Swarm`. An uncertain graph is worse than an uncertain
loop because a graph commits several workers to a plan nobody validated.
`Swarm` and `DynamicDag` additionally require human approval before anything
executes.

Provider, model, and reasoning effort are **run data**: validated, persisted,
replayable fields on the run. A queued run cannot silently inherit a selector
the user changed afterwards, and planning, direct execution, resume, and every
graph node reuse the exact selection that was recorded.

## Isolation by construction

Every editing run gets its own git worktree and its own branch. The user's
checked-out branch is never written to. A worktree is reclaimed only when it is
clean and commit-free; anything else is left on disk for the user to inspect.

In a graph, each node gets its own worktree and branch, so parallel workers
cannot corrupt each other. The integration node merges the verified branches
into a staging worktree and reruns the checks there — verification happens
after the merge, not only before it.

Failure is contained rather than propagated: a failed node marks its dependents
unreachable instead of letting them run against work that was never produced.

## Threads: a follow-up is a new run

A follow-up turn is a new run carrying `parent_run_id`, not a mutation of the
previous one. That keeps the ledger append-only and makes every turn
independently replayable.

It also creates one hard requirement: everything a conversation accumulates
must be keyed on the **thread**, not the turn.

- **The worktree.** Keyed per turn, every follow-up branched fresh off the base
  commit and the previous turn's edits were simply gone.
- **The provider home.** Each engine session runs under a fake `HOME` at
  `data_dir/sessions/<key>/home`, and the provider writes its resumable
  transcript inside it. Keyed per turn, `--resume <id>` pointed at a session
  filed in a directory it could not see; the turn died before producing a
  single token and reported only "turn failed".

Both now key on `thread_worktree_key`, which walks `parent_run_id` to the
root. `SessionDirs` carries the key it was created from, so a caller cannot
hand an engine one home and a session key naming a different one.

The sidebar lists thread **roots**, so the transcript walks the chain
*downward* from the root — follow-ups are descendants, and a walk that only
visited ancestors showed the opening turn and hid every reply.

Nodes are the exception that proves the rule: a node's home is keyed
`<run_id>-<node_id>`, plus `-attempt-<n>` on a retry, because a retry genuinely
must not resume the attempt that failed.

## Two confinement layers

`crates/daemon/src/sandbox/`:

1. **Engine control processes** (`codex app-server`, `claude`) run with a
   sanitized environment and rely on the engine's own native sandbox for its
   tool subprocesses — codex `workspace-write`, claude `acceptEdits` plus
   disallowed network tools.
2. **Daemon-owned worker commands** (shell, build, verification, integration)
   run through generated Seatbelt profiles: sanitized environment, own process
   group, resource limits, proxy-only networking.

The sanitized environment is built from scratch rather than filtered: a
controlled `PATH`, the fake `HOME` and `TMPDIR`, no `SSH_AUTH_SOCK`,
`GIT_CONFIG_NOSYSTEM=1`, and `GIT_CONFIG_GLOBAL` pinned at an empty file so git
never reads the user's config or credentials.

Two things are deliberately let through, and only for engine control processes:
`.claude.json` is **copied** (never linked, so a run cannot mutate the user's
configuration), and `Library/Keychains` is symlinked because macOS resolves the
keychain search list relative to `$HOME` — without it the CLI cannot see the
login the user already performed and reports `loggedIn: false`. Per-item
Keychain ACLs still apply, so the engine gets exactly the access its own CLI
has. Worker commands get neither, and their Seatbelt profile denies the
resolved path regardless.

## Fail closed

No sandbox, no proxy, or a failed startup canary means `run.start` is refused
with structured diagnostics. There is no unsandboxed fallback path to fall
into. See [SECURITY.md](../SECURITY.md).

## Where the data lives

Everything is under `~/Library/Application Support/dev.autoharness.app/`:
the SQLite ledger (WAL), per-session fake homes under `sessions/`, worktrees,
and the socket. `app.export` returns the whole database as JSON.
`app.purge_project` erases a project and everything derived from it,
irreversibly.
