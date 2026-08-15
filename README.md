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
- **speaks DAP** — the debug adapter protocol. `bpd` is a debug adapter, so an
    editor needs a launch configuration rather than a bespoke plugin. **vs
    code** is proven: `editors/vscode/` registers the type and a test drives a
    real session through it. **neovim** drives it through `nvim-dap`, which
    names the executable itself. **pycharm** is proven too: `editors/intellij/`
    is a plugin on the platform's own DAP extension point, and a test downloads
    a real pycharm and stops a program on a breakpoint in it — the layer it
    needs is in the unified pycharm and in IDEA Ultimate, and not in the
    community builds
- **and MCP, at parity** — ai agents get the same session through an interface
    shaped for them rather than for a ui. both are thin adapters over one
    session core, and a capability is reached by both wherever the protocol can
    carry it — a test fails when one reaches something the other does not. where
    a protocol genuinely cannot, that is a written entry naming which and why,
    because the gap this rule is about is the silent one
- **django templates** — breakpoints in template files, template frames in the
    stack, and `runserver` **without `--noreload`**: the reloader serves from a
    child process, so `bpd` debugs that child as a session of its own and a
    template breakpoint binds and fires there. asked for rather than assumed — a
    debugged child stops, so nothing turns it on for you — and with it off the
    child is still *reported*, which is what turns an unbound breakpoint into a
    reason
- **basedpython aware — not built yet.** it is the thing this project is named
    for and it is the one bullet here describing something that does not exist:
    `.by` breakpoints and `.by` frames need the transpiler to emit a source map
    with provenance and a hash of both artefacts, and that work is upstream. the
    rule this project holds itself to is that a feature is built or it does not
    exist, and a list that quietly mixed the two would break it on the first
    line
- **cpython 3.13+, no compromises** — no `sys.settrace` path, no compatibility
    shims, no capability fallbacks

## status

early. nothing is installable yet. see [ROADMAP.md](ROADMAP.md) for what is
being built and in what order, and `docs/development/` for the design

what exists today:

```sh
cargo run --bin bpd -- doctor
cargo run --bin bpd -- launch --python python3.14 script.py
cargo run --bin bpd -- launch --debug-children manage.py runserver
cargo run --bin bpd -- cache
```

`doctor` reports whether an interpreter can be debugged and refuses loudly when
it cannot. `launch` runs a program with the agent attached, holds it before its
first statement, and lets it go — producing a run indistinguishable from a bare
one, which is checked rather than claimed. `--debug-children` makes each child a
session of its own, held before it runs anything, which is what a `runserver`
needs. `cache` says what the two staging caches are holding and clears them when
asked — nothing prunes either on its own, and
[the staging caches](docs/development/caches.md) says why

## documentation

design docs live in `docs/`, built with [zensical](https://zensical.org)

`skills/bpd/SKILL.md` is for ai clients that read skills — a client feature and
no part of MCP, so copy or symlink it where yours looks for one. what it says is
not load bearing: a client without skills gets everything that matters from the
tool descriptions and the errors, which is where
[the MCP adapter](docs/development/mcp.md) keeps the semantics
