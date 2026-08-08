# basedpythondebugger

a debugger for python and [basedpython](https://github.com/KotlinIsland/basedpython), written in rust

- **PEP 669 native** — `sys.monitoring` callbacks run in rust, not python. a
    line with no breakpoint on it costs nothing, because the location is
    `DISABLE`d the first time it is seen
- **speaks DAP** — the debug adapter protocol, the same protocol vs code,
    pycharm, neovim and the rest already know how to drive. `bpd` is a debug
    adapter, so an editor needs a launch configuration, not a bespoke plugin
- **and MCP, at parity** — ai agents get the same session through an interface
    shaped for them rather than for a ui. both are thin adapters over one
    session core, and a capability exists in both or in neither
- **basedpython aware** — breakpoints in `.by` source, frames reported against
    `.by` source, through a verified source map
- **django templates** — breakpoints in template files, template frames in the
    stack
- **cpython 3.13+, no compromises** — no `sys.settrace` path, no compatibility
    shims, no capability fallbacks

## status

early. nothing is installable yet. see [ROADMAP.md](ROADMAP.md) for what is
being built and in what order, and `docs/development/` for the design

what exists today:

```sh
cargo run --bin bpd -- doctor
```

reports whether an interpreter can be debugged, and refuses loudly when it
cannot

## documentation

design docs live in `docs/`, built with [zensical](https://zensical.org)
