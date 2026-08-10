# basedpythondebugger

a debugger for python and [basedpython](https://github.com/KotlinIsland/basedpython), written in rust

- **PEP 669 native** — `sys.monitoring` callbacks run in rust, not python, and a
    line with no breakpoint on it is `DISABLE`d the first time it is seen.
    measured: a loop that runs eighteen million line locations runs within 1% of
    its bare time with a breakpoint held in the same function, and 63× faster
    than debugpy doing the same. attaching costs about 55 ms before the program
    starts — it was 150 ms until the agent was staged into a content-addressed
    cache instead of a fresh copy per launch. one machine, ten runs a
    figure, written down in
    [what bpd costs](docs/development/overhead.md)
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
cargo run --bin bpd -- launch --python python3.14 script.py
```

`doctor` reports whether an interpreter can be debugged and refuses loudly when
it cannot. `launch` runs a program with the agent attached, holds it before its
first statement, and lets it go — producing a run indistinguishable from a bare
one, which is checked rather than claimed

## documentation

design docs live in `docs/`, built with [zensical](https://zensical.org)

`skills/bpd/SKILL.md` is for ai clients that read skills — a client feature and
no part of MCP, so copy or symlink it where yours looks for one. what it says is
not load bearing: a client without skills gets everything that matters from the
tool descriptions and the errors, which is where
[the MCP adapter](docs/development/mcp.md) keeps the semantics
