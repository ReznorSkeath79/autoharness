# AutoHarness macOS Implementation Plan

## Summary

Build a greenfield, Apache-2.0, macOS-only coding-agent control center inspired by `jcode` but sharing no runtime code.

The product accepts one objective through coordinator chat, automatically chooses direct execution, a bounded loop, a swarm, or a dynamic DAG, and orchestrates installed Codex or Claude CLI sessions. It uses an all-Rust GPU interface, a persistent daemon, isolated Git worktrees, progress-aware loop detection, verified project memory, and human-promoted policy evolution.

V1 succeeds when a developer can install one notarized app, open a repository, select Codex or Claude, enter an objective, approve complex graphs, watch work survive UI restarts, and review a verified local integration commit without any remote write.

## Architecture and contracts

### Workspace structure

Create a Rust 2024 workspace with a deliberately small initial crate surface:

- `autoharness-core`: domain types, run/node state machines, routing contracts, budgets, artifacts, memory, and policy versions.
- `autoharness-protocol`: versioned client-daemon request, response, and event envelopes.
- `autoharness-store`: SQLite migrations, append-only event ledger, snapshots, replay, project memory, and policy history.
- `autoharness-engines`: shared adapter contract plus Codex App Server and Claude stream-JSON implementations.
- `autoharness-daemon`: socket server, router, DAG scheduler, process supervision, worktrees, sandbox broker, verification, recovery, and evolution lab.
- `autoharness-ui-gpui`: retained GPUI shell with semantic accessibility, coordinator chat, graph, captured ANSI check output, classified diffs, and timeline/review widgets.
- `autoharness` and `autoharnessd`: desktop and daemon binaries shipped in one signed application bundle.

Use Tokio for daemon concurrency, Rusqlite with WAL mode, Serde for protocol types, and tracing for structured local logs.

### Process and persistence model

- Install `autoharnessd` as an unprivileged per-user LaunchAgent. Closing or crashing the UI must not stop active work.
- Communicate over a Unix-domain socket under Application Support with mode `0600`, peer-UID validation, and a Keychain-held random client token.
- Use JSON-RPC 2.0 over length-delimited JSON frames. Every emitted event carries `protocol_version`, monotonic `sequence`, `run_id`, timestamp, type, and payload.
- The daemon writes events before broadcasting them. Reconnecting clients subscribe from their last sequence and receive replay followed by live events.
- SQLite is authoritative for projects, runs, chat, graph versions, node attempts, artifacts, checkpoints, memory facts, policy versions, and evolution evaluations.
- Snapshot long runs periodically, but retain the append-only event ledger for audit and recovery.

### Public domain types

Define and version these core contracts:

- `EngineKind`: `Codex` or `Claude`.
- `ExecutionShape`: `Direct`, `BoundedLoop`, `Swarm`, or `DynamicDag`.
- `RunState`: `Draft`, `AwaitingApproval`, `Running`, `Paused`, `Blocked`, `Succeeded`, `Failed`, or `Cancelled`.
- `NodeState`: `Pending`, `Ready`, `Running`, `Verifying`, `Succeeded`, `Failed`, `Blocked`, or `Cancelled`.
- `RouteDecision`: selected shape, confidence, reasons, alternatives, proposed graph, budgets, and policy version.
- `GraphProposal`: typed nodes, dependency edges, roles, file scopes, acceptance checks, and integration strategy.
- `Budget`: wall time, turns, tool calls, retries, concurrent workers, and maximum graph growth.
- `Artifact`: findings, evidence references, changed files, commit, checks, open questions, and omissions.
- `Checkpoint`: resumable node summary with evidence and last known-good repository state.
- `MemoryFact`: typed statement, evidence event IDs, confidence, supersession link, and project scope.
- `PolicyVersion`: immutable router thresholds, approved graph templates, detector thresholds, and recovery budgets.

Minimum RPC methods:

- `project.add`, `project.list`, `project.remove`
- `run.create`, `run.start`, `run.approve`, `run.pause`, `run.resume`, `run.cancel`, `run.get`
- `chat.send`, `chat.interrupt`
- `node.retry`, `node.cancel`
- `events.subscribe`
- `policy.list`, `policy.promote`, `policy.rollback`
- `memory.list`, `memory.forget`

### Agent engine adapters

- Codex uses its structured App Server interface; Claude uses bidirectional `stream-json`.
- The user selects one primary engine when creating a run. Every node uses that engine in V1.
- Persist provider session/thread IDs and resume them when safe.
- Normalize provider-specific output into shared events for text, tool activity, files, checks, questions, usage, completion, failure, and session identity.
- Never parse interactive terminal escape sequences as the control protocol.
- Do not automatically switch engines. An unavailable or terminally stuck engine blocks the run and offers a new derived run on the other engine from the latest typed checkpoint.
- Detect installation, version, authentication, and required structured modes before accepting a run.

### macOS sandbox and authority

Distribute outside the Mac App Store as a hardened, signed, and notarized universal application targeting macOS 14 or newer.

Use a layered boundary:

- Configure Codex and Claude through their own native macOS sandbox controls so model-driven tool subprocesses may write only inside the assigned worktree and session directories.
- Run daemon-owned shell, build, verification, and integration commands through generated Seatbelt profiles using `/usr/bin/sandbox-exec`, following the same general approach used by Codex's Seatbelt implementation (codex-rs/sandboxing/src/seatbelt.rs).
- Treat `sandbox-exec` as a replaceable backend because Apple marks it deprecated in its manual page.
- Launch every worker with a sanitized environment, controlled `PATH`, fake `HOME` and `TMPDIR`, a new process group, resource limits, and no SSH agent, GitHub token, cloud credentials, shell profiles, Docker socket, or user Keychain access.
- Permit the engine control process to reach its model provider. Deny arbitrary tool-process networking.
- Provide brokered read-only web search/fetch and allowlisted package-registry downloads. The proxy permits read operations and records domains; it rejects Git pushes and other external mutations.
- Run startup canaries proving that a worker can write its worktree but cannot read `~/.ssh`, write outside allowed roots, contact arbitrary network destinations, or access Git credentials.
- Fail closed when the engine's sandbox, Seatbelt canaries, proxy, or process supervision is unavailable.
- V1 permits autonomous local reads, edits, builds, tests, commits, retries, graph expansion, and integration. It performs no push, PR, merge, deployment, publication, email, or other external write.

## Product behavior

### Coordinator chat

- Make chat the primary interface. A run has one coordinator conversation with expandable worker threads.
- Display plans, route decisions, tool calls, tests, diffs, blockers, graph changes, memory use, and recovery actions as structured cards.
- Support queued mid-turn steering and immediate interruption. A normal message is injected at the next safe checkpoint; interrupt cancels the current provider turn first.
- Persist chat as event-ledger entries so UI relaunch reconstructs the exact conversation.
- Never request or expose hidden chain-of-thought. Show concise reasoning summaries, decisions, and evidence.

Primary layout:

```text
Projects | Coordinator Chat | Live Graph / Diff / Terminal
```

The product is an agent control center, not an IDE. Provide "Open in…" integration for external editors rather than building file editing, language servers, debugging, or extensions.

### Router and graph compiler

Use deterministic repository/task facts plus one structured planning call to the selected engine.

- `Direct`: one atomic, low-risk task with an explicit check.
- `BoundedLoop`: sequential inspect/change/verify work benefiting from one persistent context.
- `Swarm`: independent parallel branches with a synthesis or integration join.
- `DynamicDag`: dependent, mixed-role, high-risk, or recursively decomposable work.

Direct and bounded-loop routes start automatically. Swarms and dynamic DAGs enter `AwaitingApproval` and show their nodes, dependencies, worktrees, concurrency, budgets, and verification before starting.

Compile model proposals through Rust validation:

- Graph must be acyclic and connected to a final outcome.
- V1 allows at most eight live nodes, four concurrent workers, and depth three.
- Editing nodes require disjoint declared file scopes or separate worktrees.
- Every editing branch must feed a verification or integration node.
- Invalid or low-confidence proposals fall back to a bounded loop rather than running an uncertain graph.

### Worktrees and integration

- Read-only workers may share the base checkout.
- Every editing worker receives its own worktree and branch under daemon-managed storage.
- Capture the base commit, repository status, assignment, worktree path, and expected file scope before starting.
- Agents commit only to their assigned branches.
- A dedicated integration node combines verified branch commits into a staging worktree, resolves conflicts, reruns required checks, and creates the final local integration commit.
- Never alter the user's checked-out branch automatically.
- Cancellation removes only daemon-owned clean worktrees. Dirty or diagnostically useful worktrees are preserved and surfaced for review.

### Loop and stall detection

Record exact and normalized fingerprints for actions, observations, errors, worktree state, checks, sources inspected, artifacts, and handoffs.

High-confidence triggers:

- Identical action and normalized result three times.
- Identical invalid action three times.
- An alternating action pair twice with no progress delta.
- The same error class three times without changed code or checks.
- Six tool actions without workspace, test, source, or artifact progress.
- A node or run exhausting its explicit budget.

Recovery ladder:

1. One constrained nudge prohibiting the repeated action and requiring new evidence.
2. One replan using a critic node and detector evidence.
3. One restart from the latest verified checkpoint.
4. Mark the node blocked and surface the exact evidence to coordinator chat.

Reset detector counters after meaningful progress. Use task-specific progress semantics so productive TDD, research, long builds, and deterministic reproductions are not falsely interrupted.

### Verified project memory

Store only evidence-backed project facts:

- Architecture decisions from user messages.
- Commands and checks that produced recorded results.
- Known failure patterns and successful recoveries.
- Repository conventions, file ownership, and verified tooling facts.
- Superseding facts when the project changes.

Every injected fact must link to event IDs or repository artifacts. Model-extracted candidates without sufficient evidence remain proposals and are not injected automatically. Use SQLite FTS5 in V1; defer embeddings until the corpus proves lexical retrieval insufficient.

### Self-evolution

V1 observes and proposes; it never self-promotes.

- Candidate policies may change bounded routing thresholds, approved graph-template selection, detector thresholds, retry allocation, and coordinator prompt fragments.
- Candidates may not change sandbox rules, capabilities, external-write policy, event retention, evaluators, maximum budgets, approval requirements, or promotion logic.
- Evaluate candidates through historical replay, seeded regression scenarios, safety canaries, and route-decision comparison.
- Show expected success, interruption, latency, and graph-growth changes in the app.
- Promotion requires explicit user action and creates a signed local policy version with one-click rollback.

## Implementation sequence

1. **Foundation:** Scaffold the workspace, core state machines, versioned protocol, SQLite ledger, daemon lifecycle, socket authentication, replay, and a minimal Winit/Vello/Parley window.
2. **Engine bridge:** Implement fake-engine contract tests, then Codex and Claude adapters with installation/authentication checks, session persistence, normalized streaming, pause, interrupt, and cancellation.
3. **Sandbox:** Implement the macOS broker, environment isolation, engine-native sandbox configuration, Seatbelt runner, read-only proxy, process groups, resource limits, and fail-closed canaries.
4. **Direct chat:** Ship project selection, engine selection, coordinator chat, one direct worker, streaming cards, terminal activity, local commits, restart recovery, and cancellation.
5. **Router and loops:** Add task profiling, route decisions, bounded loops, budgets, normalized trajectory events, detector warnings, and the recovery ladder.
6. **Graphs and swarms:** Add graph compilation, complex-route approval, scheduler, worktree workers, expandable worker threads, verification gates, and integration staging.
7. **Review surfaces:** Add GPU graph rendering, a virtualized event timeline, captured ANSI check output, classified unified diffs, checks, artifacts, and "Open in…" actions. Raw PTY ownership is intentionally excluded because providers use structured protocols.
8. **Memory and evolution:** Add verified memory retrieval, policy-version storage, replay evaluation, candidate comparison, manual promotion, and rollback.
9. **Release hardening:** Add accessibility, crash recovery, database repair/export, privacy controls, local diagnostics, universal signed/notarized builds, DMG distribution, and an update checker linking to signed GitHub releases.

Each phase must end in working software with focused commits; later phases may not bypass unfinished sandbox, persistence, or adapter contract tests.

## Test plan and acceptance criteria

### Required automated coverage

- Domain transition tests for every legal and illegal run/node state change.
- Protocol compatibility, authentication, framing, event ordering, cursor replay, and duplicate-request tests.
- SQLite migration, WAL recovery, snapshot/replay, corruption reporting, and concurrent reader/writer tests.
- Contract fixtures proving Codex and Claude produce equivalent normalized events.
- Sandbox canaries proving allowed worktree operations succeed and forbidden filesystem, credential, network, socket, and external-write attempts fail.
- Graph validation for cycles, orphan nodes, excessive growth, scope collisions, missing verification, and budget overflow.
- Loop-detector positive fixtures plus false-positive fixtures for TDD, research, flaky infrastructure, and long builds.
- Worktree creation, cancellation, dirty preservation, conflict handling, integration, and base-branch immutability tests.
- UI snapshot tests for chat, graph, diff, terminal, recovery, approval, empty, loading, and failure states.
- Semantic accessibility-tree and keyboard-navigation tests.
- Evolution tests proving candidates cannot modify invariants and promoted policies can be rolled back exactly.

### End-to-end scenarios

1. Add a repository, select Codex, submit a simple objective, automatically run direct mode, verify, and produce a local commit.
2. Submit a complex objective, inspect and approve a swarm, run two isolated editing workers, integrate them, rerun checks, and review the final diff.
3. Kill and relaunch the UI during a run; confirm the daemon continues and the client replays without duplicate or missing events.
4. Force a repeated failing action; confirm nudge, replan, checkpoint restart, and final blocked evidence occur in order.
5. Attempt to read SSH credentials, write outside the worktree, push Git changes, or use arbitrary network egress; confirm fail-closed denial and visible audit events.
6. Cancel a swarm; confirm the full process tree stops, clean worktrees are removed, dirty worktrees are preserved, and the base checkout is unchanged.
7. Promote and roll back an evolution candidate; confirm historical runs, sandbox policy, and capability boundaries remain unchanged.

### Release acceptance

- One notarized universal DMG installs and launches on clean macOS 14+ test users.
- Codex and Claude setup diagnostics identify missing, outdated, or unauthenticated installations without crashing.
- Active runs survive client restarts and daemon restart reconciliation never reports stale work as successful.
- No V1 workflow performs an external write.
- Every final result includes its engine, policy version, graph version, commits, checks, unresolved blockers, and sandbox/audit evidence.

## Assumptions and defaults

- Working product name: **AutoHarness**; development bundle identifier: `dev.autoharness.app`.
- License: Apache-2.0.
- No cloud service, accounts, remote telemetry, mobile client, Windows/Linux support, direct model API, full IDE, extension marketplace, or automatic policy promotion in V1.
- User data remains local under Application Support; secrets stay in Keychain.
- Codex and Claude are installed and authenticated separately by the user.
- `jcode`, Codex, Claude Code, Conductor, Melty, and other harnesses are references only; AutoHarness remains an independent implementation.
