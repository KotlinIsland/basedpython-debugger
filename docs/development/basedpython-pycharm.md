# bpd and the basedpython pycharm plugin

there are two plugins in this ecosystem registering the same two platform
extension points, and this page is the agreement between them

- **`basedpython-pycharm`** — the language plugin. `.by` file type, the `by`
    language server, the `buff` formatter, run configurations, and **`.by`
    debugging**, built on `intellij.platform.dap` and driving **debugpy**
    against the transpiled python
- **`editors/intellij/` in this repository** — makes `bpd` reachable as a debug
    backend for **python**. it knows nothing about `.by`

both register `platform.dap.debugAdapterSupportProvider` and
`platform.dap.launchArgumentsProvider`. that is not a conflict — the platform
routes on the adapter id and the run configuration type — but it is duplicated
work, and it should end somewhere

## why they are not merged today

**they do not overlap in capability.** `basedpython-pycharm` debugs `.by`;
`bpd` cannot, because that needs the transpiler to emit a source map with
provenance and a hash of both artefacts — M6 on the roadmap, and upstream work.
folding a working `.by` debugger into a plugin that cannot yet replace it would
trade something for nothing

and the scopes differ. `bpd` is a python debugger. a person debugging ordinary
python has no reason to install a basedpython language plugin to get one, and a
merge would make that the only way

## where this is going

the end state is not "bpd's plugin absorbed into the language plugin". it is
**`basedpython-pycharm` switching its adapter from debugpy to `bpd`** once M6
lands. debugging the transpiled python is the fallback this project's roadmap
already describes as temporary — `bpd` exists to replace it with `.by`
breakpoints and `.by` frames through a verified map, which is the whole reason
the source mapping rule is "total or absent, no identity fallback"

when that happens the two plugins have one adapter between them, and whether
they are one artefact or two is a packaging question rather than an
architectural one

## what to share before then

the plumbing is duplicated now, and the language plugin is **ahead** on three
things it hit first. they are recorded here because `bpd`'s plugin has not
solved them and in two cases has not proved it does not need to:

- **the console.** `DapXDebugProcess` assumes the adapter owns the debuggee and
    builds a console over its own process handler. `bpd` spawns the interpreter
    itself, so the debuggee is not the adapter's process. `basedpython-pycharm`
    overrode `createXDebugProcess` for this; `editors/intellij/` does not, and
    **nothing asserts that the debugged program's own output reaches the IDE
    console**. that is the first thing to check
- **output events.** debugpy opens every session with two bare events reading
    `ptvsd` and `debugpy`, which land in front of the program's first line, so
    the language plugin drops adapter `output` events on the floor. `bpd` sends
    them with proper `stdout` and `stderr` categories
    (`crates/bpd_dap/src/adapter.rs`), so the same filtering is probably wrong
    for `bpd` — **probably**, and that is the point: it is untested
- **how a failure surfaces.** a missing adapter thrown from the wrong place
    reaches the user as an IDE internal error naming `CoroutineScheduler`.
    `bpd`'s plugin raises `ExecutionException` deliberately and
    `testAConfigurationThatCannotFindBpdIsRefused` covers it, so this one is
    already handled — it is listed because it is the trap, not because it is open

## the platform, as both plugins found it

established by decompiling the shipped `intellij.platform.dap.jar` rather than
from documentation, and true for both:

- `DebugAdapterSupportProvider` is `@ApiStatus.Experimental`, the module
    descriptor says `visibility="public"`, and both extension points are
    `dynamic="true"`
- **`intellij.platform.dap` is not in the community IDEs.** present in
    `pycharmPY` and IDEA Ultimate 2026.2.1, absent from `pycharmPC` and `IC` —
    so **both** of these plugins are unavailable on community builds
- `configurationType` plus `debugAdapterSupportProvider` is not enough:
    `DapProgramRunner.canRun` also asks `launchArgumentsProvider`, and without
    one the run configuration exists and silently cannot be debugged
- `supportsSingleThreadExecutionRequests` **is** honoured, so `bpd`'s non-stop
    default behaves the same here as in vs code
- `startDebugging`, `gotoTargets` and `restartFrame` are **not** implemented by
    the layer, so child debugging, set next statement and restart frame are all
    unreachable from a jetbrains IDE regardless of which adapter is behind it

that last line is the one that decides how much converging is worth: it is a
platform gap rather than an adapter gap, and neither plugin can close it
