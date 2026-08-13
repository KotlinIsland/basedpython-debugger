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

**they do not overlap in capability.** `basedpython-pycharm` debugs `.by` and
`bpd` cannot yet, so folding a working `.by` debugger into a plugin that cannot
replace it would trade something for nothing

**but the gap is much smaller than this project recorded.** M6 said the
transpiler had to emit a map with provenance and a hash, as though none existed.
`by run` writes `_by_sourcemap.py` today — indexed by generated line, holding the
`.by` line it came from and `None` where a prelude has no source, which *is* the
provenance — and the language plugin ships source-mapped `.by` debugging on it,
verified end to end. its `docs/debugging.md` records that the identical "blocked
upstream" belief it had held for a long time was false

what is actually missing is the **hash of both artefacts**, and only that. it is
this project's rule that needs it rather than the mapping: a line that came from
a map nobody verified against the thing it maps is the exact failure the contract
refuses. so the upstream ask is one digest

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

the plumbing is duplicated now, and the language plugin hit three things first.
all three are settled for `bpd` now, and the first is kept here because it was
settled the other way round from what this page expected:

- **the console.** `DapXDebugProcess` assumes the adapter owns the debuggee and
    builds a console over its own process handler. `bpd` spawns the interpreter
    itself, so the debuggee is not the adapter's process. `basedpython-pycharm`
    overrode `createXDebugProcess` for this — and **`editors/intellij/` needs no
    override**, which was checked by driving one rather than reasoned about. the
    console the platform builds is over a `DefaultDebugProcessHandler` that has
    no process behind it either way, and what fills it is
    `formatAndPrintOutput`, which prints DAP `output` events into it. so who
    spawned the debuggee never comes into it. the session test asserts the
    program's own stdout and stderr are in that console, and
    [the intellij page](intellij.md#the-console) records what the platform does
- **output events.** debugpy opens every session with two bare events reading
    `ptvsd` and `debugpy`, which land in front of the program's first line, so
    the language plugin drops adapter `output` events on the floor. that
    filtering **is** wrong for `bpd`, and it is now driven rather than guessed
    at: `bpd` sends the program's two streams under `stdout` and `stderr`
    (`crates/bpd_dap/src/adapter.rs`) and sends no `output` event of its own —
    the `{"listening":…}` announcement goes on `bpd dap`'s own stdout, which the
    client reads off the pipe. dropping them here would drop the program
- **how a failure surfaces.** a missing adapter thrown from the wrong place
    reaches the user as an IDE internal error naming `CoroutineScheduler`.
    `bpd`'s plugin raises `ExecutionException` deliberately and
    `testAConfigurationThatCannotFindBpdIsRefused` covers it, so this one is
    already handled — it is listed because it is the trap, not because it is open

what the two plugins do share is one thing neither can fix from a plugin: a
debuggee's stdout is a pipe, and cpython block-buffers a piped stdout, so an
unflushed `print` is not in the console until the program exits. it is the
adapter's to solve or to refuse, not the IDE's

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
