# Security

AutoHarness runs coding agents against your repositories on your machine. The
design assumption is that an agent may do something wrong — not maliciously,
just wrong — and that the blast radius has to be bounded by construction rather
than by the agent behaving.

## Threat model

**In scope.** A confused or looping agent editing the wrong files, running the
wrong command, exfiltrating repository contents over the network, corrupting a
parallel worker's work, or destroying uncommitted work in the user's checkout.

**Out of scope.** A hostile local user with your UID — the daemon socket is
`0600` with a peer-UID check, which stops other accounts, not you. Also out of
scope: the security of the Codex and Claude CLIs themselves, and anything a
signed and notarized build cannot help with on an unsigned local build.

## Boundaries

**Process authority.** The desktop shell has none. It cannot open the database
or spawn an engine; it speaks JSON-RPC over a Unix socket
(`0600`, peer-UID checked, Keychain-held client token). The daemon owns every
side effect, so there is one place to audit.

**Two confinement layers.** Engine control processes run with a sanitized
environment and their own native sandbox for tool subprocesses (codex
`workspace-write`, claude `acceptEdits` plus disallowed network tools).
Daemon-owned worker commands — shell, build, verification, integration — run
under generated Seatbelt profiles with their own process group, resource
limits, and proxy-only networking.

**Environment built from scratch, not filtered.** A controlled `PATH`, a fake
`HOME` and `TMPDIR` per session, no `SSH_AUTH_SOCK`, `GIT_CONFIG_NOSYSTEM=1`,
and `GIT_CONFIG_GLOBAL` pinned at an empty file so git cannot reach your config
or credential helpers. A denylist would leak whatever it forgot; this leaks
only what is listed.

**Credentials.** No credential is ever copied into a session home. Engine
control processes get a symlink to `Library/Keychains`, because macOS resolves
the keychain search list relative to `$HOME` and without it the CLI cannot see
the login you already performed. Per-item Keychain ACLs still apply, so the
engine gets exactly the access its own CLI has when you run it yourself.
Daemon-owned worker commands never get the link, and their Seatbelt profile
denies the resolved path regardless.

**Your branch.** Editing runs work in their own git worktree on their own
branch. The checked-out branch is never written to. A worktree is reclaimed
only when it is clean and commit-free — anything else is left on disk for you.

**Network.** Worker commands reach the network only through a read-only
brokered proxy, and every fetch is audited.

**No external writes.** No push, no pull request, no deploy, anywhere. This is
a product boundary, not a default to be configured away.

## Fail closed

No sandbox, no proxy, or a failed startup canary means `run.start` is refused
with structured diagnostics. There is no unsandboxed fallback. A build that
cannot confine an agent does not run one.

The same applies to updates: without a compile-time Team ID and an HTTPS feed
URL, the build stays fully usable but deliberately exposes no self-install
action. Update archives are SHA-256 pinned and the swap is atomic with
rollback.

## Your data

Everything is local, under `~/Library/Application Support/dev.autoharness.app/`.
Nothing is sent anywhere except to the provider CLIs you already installed and
logged into.

- `app.export` returns the entire database as JSON.
- `app.purge_project` erases a project and everything derived from it. This is
  irreversible and intended to be.

## Unsigned builds

`./scripts/package.sh` without `SIGN_IDENTITY` produces an unsigned bundle for
local testing. It is not notarized and Gatekeeper will refuse it on another
Mac. Do not distribute it.

## Reporting

This is a private repository. Report a suspected vulnerability by opening an
issue on it. Do not open a public disclosure elsewhere first.
