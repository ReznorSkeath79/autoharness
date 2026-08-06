# The cockpit acceptance reference

`cockpit-acceptance.png` in this directory is the **exact** acceptance
reference for the AutoHarness shell. It is not a mood board and it is not an
aspiration. A change to the default window that makes it disagree with the
image is a regression, and the reviewer rejects it.

## What the image fixes

The image fixes the default window: no overlay open, no palette, no switcher,
one run selected, all three panes open.

| Region | What must be true |
|---|---|
| Surface | Fully black. Every pane, every seam, every card. |
| Toolbar | Route mode, engine chips, run state, then Overview, History, Worktrees, search, palette, notifications, settings. |
| Sidebar | Repositories, then Runs with status text on the right and a per-run count, then an older-runs group, then the worktree root. |
| Coordinator | Message list, a plan summary strip, then the composer with repository/mention controls, the model + reasoning selector, the selected-repository breadcrumb, and send. |
| Execution | The DAG with its node count, exact dependency flow, selectable node details, a Parallel view / Timeline switch, and four activity rows. |
| Inspector | Changes, Diff, Checks, Artifacts, Worktree, Engine, Budget, and Usage — in that order, and Usage must be reachable. |
| Type | Nothing below 11 px. |

## How to check it

```sh
cargo run -p autoharness-ui-gpui --example shell_preview
```

The preview builds its state in memory. It needs no daemon, no provider
account, and no client token, so the comparison is the same on every machine.
Open the model control to verify the animated capability picker; click a DAG
node to verify its dependency, scope, and check detail strip. These transient
states supplement the default-window image rather than changing its black
surface contract.

## What the image does not fix

The image shows one state. It does not constrain the overlays, because they
are not open in it. Overlay content is fixed by the tests in
`crates/ui-gpui/src/lib.rs` instead, which is the right place for it: a row
that exists is a row a test names.

Two rules survive from the image into every overlay:

- The surface stays black.
- Type stays at 11 px or above.
