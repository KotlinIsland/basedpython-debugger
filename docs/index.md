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
    **pycharm** is proven too, by a test that downloads a real pycharm and stops
    a program on a breakpoint through the plugin in `editors/intellij/`
- **and MCP, at parity** — ai agents get the same session through an interface
    shaped for them rather than for a ui. both are thin adapters over the same
    session core, and a capability is reached by both **wherever the protocol
    can carry it** — where one genuinely cannot, that is a written entry naming
    the front end and the reason, rather than a quiet gap
- **django templates** — breakpoints in template files and template frames in
    the stack, not the `django/template/base.py` frames underneath them.
    `runserver` works **without** `--noreload`: the reloader serves from a child
    process, and with `debugChildren` on, bpd debugs that child as a session of
    its own and a template breakpoint binds and fires there
- **basedpython aware** — `.by` breakpoints bind to the generated line and report
    both locations, and a `.by` frame says where the interpreter really is. the map
    is verified against a hash of both artefacts before it resolves anything, and a
    line it cannot place errors rather than falling back to the raw number
- **cpython 3.13+** — no `sys.settrace` path, no shims, no fallback ladders

## status

early, and **nothing is published yet**: [the release path](development/releasing.md)
is armed rather than run. a `v*` tag builds a wheel per platform, installs each
one and debugs five interpreters through it, and uploads to pypi once a person
approves — and until the first tag is pushed there is nothing there to install.
the design lives under [development](development/architecture.md), and the order
of work is in `ROADMAP.md`

what exists today is five subcommands — `bpd doctor`, which reports whether an
interpreter can be debugged; `bpd launch`, which runs a program with the agent
attached and stops it before its first statement; `bpd dap` and `bpd mcp`, the
two front ends; and `bpd cache`, which shows what the agent staging cache holds:

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
