# basedpythondebugger

a debugger for python and [basedpython](https://github.com/KotlinIsland/basedpython), written in rust

`bpd` is a rewrite of the idea behind debugpy on top of PEP 669, with no
compatibility layer holding it back:

- **PEP 669 native** — `sys.monitoring` callbacks are rust functions. no python
    trace function, no python frame per event. a line with no breakpoint on it
    is `DISABLE`d the first time the interpreter reaches it and is never
    reported again — [measured](development/overhead.md) at within 1% of a bare
    run, on a loop of eighteen million line locations with a breakpoint held in
    the same function. what a session *does* cost is the 55 ms it takes to
    attach, and that is on the same page
- **speaks DAP** — the [debug adapter protocol](https://microsoft.github.io/debug-adapter-protocol/).
    `bpd` is a debug adapter, so an editor needs a launch configuration rather
    than a bespoke plugin. **vs code** is proven, by a test that drives a real
    session through the extension; **neovim** drives it through `nvim-dap`.
    **pycharm is a hard requirement and is not done** — its own python
    debugging is pydevd, and DAP reaches it through a plugin rather than
    natively
- **and MCP, at parity** — ai agents get the same session through an interface
    shaped for them rather than for a ui. both are thin adapters over the same
    session core, and a capability exists in both or in neither
- **django templates** — breakpoints in template files and template frames in
    the stack, not the `django/template/base.py` frames underneath them. under
    `runserver` the reloader serves from a **child** process the debugger is not
    in, so that wants `--noreload` until `bpd` follows a child — and `bpd` says
    so rather than leaving a breakpoint looking broken
- **basedpython aware — not built yet.** the thing this project is named for is
    the one bullet here that does not exist: `.by` breakpoints and `.by` frames
    need the transpiler to emit a source map with provenance and a hash of both
    artefacts, and that work is upstream. this project's rule is that a feature
    is built or it does not exist, so it is labelled rather than listed beside
    the ones that are
- **cpython 3.13+** — no `sys.settrace` path, no shims, no fallback ladders

## status

early. nothing is installable yet. the design lives under
[development](development/architecture.md), and the order of work is in
`ROADMAP.md`

what exists today is `bpd doctor`, which reports whether an interpreter can be
debugged, and `bpd launch`, which runs a program with the agent attached and
stops it before its first statement:

```sh
cargo run --bin bpd -- doctor python3.14
cargo run --bin bpd -- launch --python python3.14 script.py
```

see [launching a debuggee](development/launching.md)

## why not just use debugpy

not for the reason this page used to give. debugpy vendors pydevd, and pydevd
has used `sys.monitoring` since cpython 3.12 — with a cython-compiled callback,
not a python one. on every interpreter `bpd` supports, debugpy is a PEP 669
debugger too, and with no breakpoints set it costs a program as little as `bpd`
does

what it still carries is a decade of having to work on interpreters where the
only interface was `sys.settrace`, and the shape that left behind shows up the
moment a breakpoint exists. [measured](development/overhead.md): a breakpoint on
a line in a hot function — a line the program **never reaches** — makes that
function run 63× slower under debugpy and not measurably slower under `bpd`,
because `DISABLE` is how the lines around a breakpoint stop being reported and a
design that predates it has nowhere to put that

`bpd` starts from the assumption that PEP 669 is all there is, and that dropping
support for the interpreters where it is not is a feature rather than a cost.
that assumption changes the architecture, not just the implementation — see
[architecture](development/architecture.md)

starting a session costs something in both, and that is on the same page too:
about 55 ms before the program's first statement under `bpd`, about 1.1 s under
debugpy
