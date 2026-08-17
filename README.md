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
- **basedpython aware** — `.by` breakpoints bind to the generated line and
    report both locations, and a `.by` frame carries where the interpreter really
    is. the map is verified against a hash of **both** artefacts before it
    resolves anything, and a line it cannot place is an error rather than a
    fallback to the raw number. the bullet this replaces said the feature did not
    exist, which stopped being true when `by run` began emitting the map
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

## contributing

`ROADMAP.md` says a milestone is finished when the standard in this section
holds for it. that sentence pointed at a section which did not exist, so the
standard a contributor is held to had no committed statement at all — this is it

### a debugger is a measuring instrument

everything a person or an agent believes about a running program comes through
it. a type checker that is wrong produces a diagnostic somebody can argue with;
a debugger that is wrong produces a false **belief** about reality, and they act
on it. there is no downstream check that catches it

so the bar is not "works on the happy path". it is: **if `bpd` reports it, it is
true, and if it cannot know, it says so.** a wrong answer is worse than an error

this project will not ship a value that is probably right, a breakpoint that
silently did not bind, a step that silently landed elsewhere, or a line number
from a source map nobody verified

### no placeholders

a feature is fully implemented, tested and documented — or it does not exist.
there is no third state. `todo!` and `unimplemented!` are denied at clippy
level, and so are a function returning a default to stand in for work not done,
a match arm added to quiet the compiler, a parameter accepted and ignored, and
an option parsed and never read

if you need the shape of something before the body exists, leave the type out of
the tree and write the design in `scratch.<topic>.md`. an absent feature is
honest; a hollow one is a lie that compiles

### fail loudly

when the debugger cannot do what was asked it says so immediately, with the
reason and the thing that caused it. it never degrades quietly

| situation                  | response                                             |
| -------------------------- | ---------------------------------------------------- |
| the interpreter is too old | refuse at launch, naming the version and the minimum |
| a breakpoint cannot bind   | report it unbound, with why — never as set           |
| a source map has no entry  | error naming the file and line — never the raw line  |
| an expression fails        | return the exception, not `None`                     |
| an invariant is violated   | panic with a message naming the invariant            |

`assert!`, `unreachable!` and `panic!` are encouraged, with a message saying
what was supposed to hold. never `let _ =` on a `Result`; never a bare
`unwrap()`. `expect()` is allowed and its message justifies why the invariant
holds

**and a bound that bit is reported, never silent.** anything truncated,
capped or discarded carries the count of what went — `bpd_core::Kept` exists so
that is a type rather than a convention, because it was got wrong four separate
times before it was one

### no special casing

a fix that handles the reported input and nothing else is not a fix. find the
rule the input is an instance of, implement the rule, and test the rule. if the
rule genuinely has an exception, the exception gets a comment naming the cpython
behaviour that forces it and a test that fails if cpython changes

### correctness is not negotiable for speed

this project is fast because of its architecture — native PEP 669 callbacks,
`DISABLE` on locations that will never be interesting — not because it skips
checks. an optimisation that changes an observable answer is a bug, not a
trade-off, and every fast path needs a test pinning it to the slow path's answer

### tests

a bug fix starts with a failing test. anything touching interpreter behaviour
needs an integration test that spawns a real interpreter and asserts on real
state — unit tests over mocked frames prove nothing about cpython. performance
claims need a benchmark in the tree, not a number in a commit message

a test whose name promises more than its body checks is worse than no test

### the build

```sh
cargo test
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
prek run --all-files
```

the agent is a cpython extension and is not abi3, so every build compiles it
against one interpreter. `.cargo/config.toml` sets a default; override it with
`PYO3_PYTHON=python3.13 cargo build -p bpd_agent`

### text style

all english is lowercase, with exceptions for proper nouns, acronyms and PEP
numbers, and trailing periods are dropped — in code comments, commit messages
and documentation alike. comment when the implementation looks out of place: a
cpython quirk, an ordering constraint, a deliberate non-obvious choice. do not
narrate what the code already says
