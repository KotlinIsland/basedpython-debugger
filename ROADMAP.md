# roadmap

what is being built, in what order, and how we know when each part is finished

this is the high level view. the per-task breakdown lives in
`scratch.roadmap.md`, which is a working note and is not committed

## what "finished" means anywhere on this page

a milestone is finished when the standard in the contributing section of
[README.md](README.md) holds for it: tests that fail without the change, clean
clippy and hooks, no placeholder of any kind, every new failure mode reporting a
cause a user can act on, and a docs page wired into the nav

a milestone is **not** finished because the happy path works

## the MVP

the MVP is the point at which `bpd` is worth using instead of `pdb` or debugpy
for ordinary python, on a supported interpreter. it deliberately excludes
basedpython, django, attach, and both of the headline roadmap features — those
are what the architecture is *for*, but none of them matter if stopping on a
breakpoint is not already solid

### MVP criteria

**launching**

- [ ] `bpd` runs a script, a module (`-m`), and a package the same way the
      interpreter would: same `__main__` identity, same `sys.argv`, same
      `sys.path[0]`, same exit code, same stdout and stderr interleaving
- [ ] it runs on cpython 3.13 and 3.14, on linux, macos and windows, on gil and
      free-threaded builds
- [ ] an unsupported interpreter is refused before anything is launched, by name

**breakpoints**

- [ ] a breakpoint binds through the whole code object tree — inside a
      comprehension, a lambda, a nested function, a class body
- [ ] a breakpoint on a non-executable line moves to the next executable line
      and the response says where it moved to
- [ ] a breakpoint in a module that is not imported yet is reported **unbound**,
      and binds when the module is imported
- [ ] conditions, hit counts and logpoints evaluate in the debuggee, not over
      the wire
- [ ] a condition that raises reports the exception, and the breakpoint still
      stops — it never silently behaves as false

**execution control**

- [ ] step over, step in, step out, continue, and pause
- [ ] stepping is correct across generators, coroutines, comprehensions,
      exception unwinding, and re-entrant calls
- [ ] with several threads running, stopping one stops the rest, and the stop
      reports which thread caused it

**state**

- [ ] the stack, with each frame's source location, on every thread
- [ ] locals, globals and closure variables read from the scope that was asked
      for
- [ ] a local can be **written**, and the write is visible to the program
- [ ] object graph expansion with an explicit budget, and an explicit statement
      of what was left out when the budget is hit
- [ ] expression evaluation in a chosen frame, where a failure returns the
      exception

**the two front ends**

- [ ] a DAP adapter that vs code drives end to end from a launch configuration
- [ ] an MCP server exposing the same session
- [ ] a parity test that enumerates the capabilities in `bpd_core` and fails if
      either adapter is missing one. the rule is enforced by CI, not by review

**evidence**

- [ ] a benchmark in the tree comparing a run under `bpd` with no breakpoints
      against the same program run bare, and with breakpoints against debugpy
- [ ] integration tests that spawn real interpreters across the version and
      build matrix

### explicitly not in the MVP

basedpython source maps, django templates, attach, restart frame, hot module
reloading, and any editor integration beyond a launch configuration

## before the MVP

### M0 — foundations · done

the workspace, the lint and hook configuration, CI across the interpreter
matrix, the docs site, and `bpd doctor`

### M1 — a process that stops · done

`bpd launch` starts an interpreter with the agent attached, holds the program
before its first statement, resumes it, and exits with the program's own code

running under `bpd` is indistinguishable from running without it, and that is
checked rather than claimed: the same program runs twice, once bare and once
debugged, and the launch record, both output streams and the exit code are
compared. syntax errors, uncaught tracebacks and an unopenable script are all
reported in the interpreter's own words

what is **not** done here: `-m` and `-c` launch forms, and stop coordination
strong enough for a breakpoint. an entry stop holds the whole program because no
user thread exists yet; that stops being true the moment a breakpoint can fire,
and the real coordination lands with M2

see [launching a debuggee](docs/development/launching.md)

### M2 — breakpoints

binding, conditions, hit counts and logpoints, per the criteria above

### M3 — stepping, frames and values

execution control and state, per the criteria above

### M4 — DAP

the adapter, and an editor driving it end to end

### M5 — MCP

the agent interface from
[agent interface](docs/development/agent-interface.md), and the parity test that
makes the invariant structural

**the MVP is M0 through M5**

## after the MVP

### M6 — basedpython

`.by` breakpoints and `.by` frames through a verified source map

this milestone has an **upstream dependency**: the basedpython transpiler has to
emit a source map with provenance for generated lines and a hash of both
artefacts. that work is in basedpython, not here. until it lands, `bpd` debugs
the generated python and says that is what it is doing

### M7 — django templates

template frames, template context and template expression evaluation, per
[django templates](docs/development/django-templates.md), with a loud refusal
when the template engine is not in debug mode

### M8 — attach

PEP 768, implemented as the wire protocol in rust rather than through
`sys.remote_exec`, so no matching local interpreter is needed. 3.14 or newer,
and a refusal below that

### M9 — reset stack frame to here

two operations, separate because their obstacles are different

**restart frame** — discard the frames above a chosen frame and re-enter it from
the top with its original arguments. the honest limitation, which the ui states
rather than buries: side effects already performed are not undone

**set next statement** — move execution to another line inside the current frame.
cpython historically permits assigning to `frame.f_lineno` only from inside a
trace function, and `bpd` does not install one, so whether this is reachable at
all under PEP 669 is the first thing to establish. if it needs a cpython change,
the answer is to propose one upstream

both refuse loudly rather than approximating: a jump into a different block,
into or out of a `try`, or across a `with` is either correct or rejected

### M10 — hot module reloading

reload a changed module and keep the process alive: rebind `__code__` on live
function objects, update class dictionaries in place so existing instances see
new methods, and re-run nothing that has already run

a change that **cannot** be applied — a changed signature, a changed class
layout, a module with import side effects, a live frame executing the code being
replaced — is reported as not applicable, with the reason and the name of the
thing that blocked it. it is never applied partially, because a process half way
between two versions of a module produces evidence about neither

## not planned

- cpython 3.12 or older
- alternative implementations
- a `sys.settrace` path
- jinja2 templates, for now
