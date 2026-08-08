# basedpythondebugger

a debugger for python and [basedpython](https://github.com/KotlinIsland/basedpython), written in rust

`bpd` is a rewrite of the idea behind debugpy on top of PEP 669, with no
compatibility layer holding it back:

- **PEP 669 native** — `sys.monitoring` callbacks are rust functions. no python
    trace function, no python frame per event. a line with no breakpoint on it
    is `DISABLE`d the first time the interpreter reaches it, and never costs
    anything again
- **speaks DAP** — the [debug adapter
    protocol](https://microsoft.github.io/debug-adapter-protocol/), the same
    protocol vs code, pycharm, neovim and the rest already know how to drive.
    `bpd` is a debug adapter, so an editor needs a launch configuration rather
    than a bespoke plugin
- **and MCP, at parity** — ai agents get the same session through an interface
    shaped for them rather than for a ui. both are thin adapters over the same
    session core, and a capability exists in both or in neither
- **basedpython aware** — set a breakpoint in `.by` source, get frames back in
    `.by` source, through a source map that is verified rather than assumed
- **django templates** — breakpoints in template files and template frames in
    the stack, not the `django/template/base.py` frames underneath them
- **cpython 3.13+** — no `sys.settrace` path, no shims, no fallback ladders

## status

early. nothing is installable yet. the design lives under
[development](development/architecture.md), and the order of work is in
`ROADMAP.md`

what exists today is `bpd doctor`, which reports whether an interpreter can be
debugged:

```sh
cargo run --bin bpd -- doctor python3.14
```

## why not just use debugpy

debugpy predates PEP 669 and carries a decade of compatibility with it. it
still has to work on interpreters where the only interface is `sys.settrace`,
which means a python callback on every line of every frame that is being
traced, and a large body of cython and frame-evaluation machinery to claw that
cost back

`bpd` starts from the assumption that PEP 669 exists, and that dropping support
for the interpreters where it does not is a feature rather than a cost. that
assumption changes the architecture, not just the implementation — see
[architecture](development/architecture.md)
