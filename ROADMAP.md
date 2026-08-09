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

- [x] a breakpoint binds through the whole code object tree — inside a
      comprehension, a lambda, a nested function, a class body
- [x] a breakpoint on a non-executable line moves to the next executable line
      and the response says where it moved to
- [x] a breakpoint in a module that is not imported yet is reported **unbound**,
      and binds when the module is imported
- [x] conditions, hit counts and logpoints evaluate in the debuggee, not over
      the wire
- [x] a condition that raises reports the exception, and the breakpoint still
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
user thread exists yet; that stops being true the moment a breakpoint can fire

see [launching a debuggee](docs/development/launching.md)

### M2 — breakpoints · done

a breakpoint set on a source location finds the code object and the offset that
will run, and the program stops there. code objects are discovered through a
global `PY_START`, so one built by `exec` an hour into a run is found the same
way a module's is; binding walks `co_consts`, so a method of a class and a
generator expression inside it both bind; a location that is not executable
moves and the answer says where to; and a location with nothing behind it is
reported unbound with a reason rather than reported as set

a breakpoint also carries a condition, a hit count and a log message, and all
three are answered inside the debuggee. an expression is compiled when the
breakpoint is set and never on the event path, the common `name <op> literal`
shape is compared natively against the frame's own locals, and an expression
that raises **stops** and hands over the exception rather than being read as
false. a logpoint on a line executed a million times produces a million records
and waits for the debugger none of those times

stop coordination did **not** land with it. a breakpoint stop reports the thread
that hit it and deliberately claims nothing about the others, because holding
them is not implemented and saying otherwise would be the lie this project
exists to avoid

see [breakpoints](docs/development/breakpoints.md)

### M3 — stepping, frames and values

execution control and state, per the criteria above

stop coordination lands here, and **the choice of what a stop means has to be
made with it** rather than after — see non-stop debugging under
[further out](#further-out). building "stop the world" as the only mode would
have to be undone

### M4 — DAP

the adapter, and an editor driving it end to end

### M5 — MCP

the agent interface from
[agent interface](docs/development/agent-interface.md), and the parity test that
makes the invariant structural

this is also where the **debug script** lands: a schema-validated tree of steps
with its own branching, submitted in one call, returning a transcript of what
happened at every one. it collapses an investigation that would be fifty round
trips into a single turn, and it is a capability rather than an MCP feature, so
DAP gets it too

and where **how an agent learns bpd** is answered. only tools are
model-controlled in MCP — resources are the host's choice and prompts are the
user's — so the interface has to explain itself through schemas and errors
first, with documents and canonical investigations layered on top rather than
compensating for them

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

### M10 — hot module reloading and hot code replacement

reload a changed module and keep the process alive: rebind `__code__` on live
function objects, update class dictionaries in place so existing instances see
new methods, and re-run nothing that has already run

a change that **cannot** be applied — a changed signature, a changed class
layout, a module with import side effects, a live frame executing the code being
replaced — is reported as not applicable, with the reason and the name of the
thing that blocked it. it is never applied partially, because a process half way
between two versions of a module produces evidence about neither

**what triggers it is a separate and much earlier piece of work.** noticing that
the file on disk is no longer the code that is running is a correctness feature
in its own right, and cpython gets it wrong today: a traceback is rendered by
`linecache` reading the file *now*, so a file edited since import is shown with
current text against old line numbers. every python debugger inherits that lie.
detecting it costs a hash recorded at bind time, and once it is detected the
client can be offered the replacement — "the source does not match what is
running, replace it?" — through both adapters

## further out

these are not ordered against each other, and none of them is scheduled. what
each entry records is the reason it is worth doing and the part that is hard,
so that whoever picks one up starts from the obstacle rather than finding it

### breakpoint sequences

a breakpoint that does not arm until another one has been hit. "stop in the
handler, but only after the request that set this flag came through"

this is the cheapest item here and it fits what M2 already built: breakpoints
carry conditions and hit counts evaluated in the agent, and a sequence is one
more edge in that table. it is also **cheaper than it looks at run time** — a
breakpoint that is not armed yet has its `LINE` events off entirely, so it costs
nothing until the one before it fires

the design questions are all about resets: whether arming is one-shot or
permanent, whether a sequence is per thread or per process, and whether the hit
counter of a later breakpoint starts before or after it arms. a chain answers
those differently from a DAG, and picking a chain first is likely right

### non-stop debugging

on a free-threaded build, stopping one thread does not stop the others — that is
reported honestly today and treated as a gap. it is also a **feature**, and a
standard one: gdb calls it non-stop mode and DAP has
`supportsSingleThreadExecutionRequests` for it

so the answer to the M1.7 gap is not "build stop coordination", it is "build
both and say which is in force". a GIL build can only really offer stop the
world; a free-threaded build can offer either

what is **not** free is the guarantee. while one thread is stopped and the
others run, everything the stopped frame can see is being mutated underneath
the inspection — so a value read twice can differ, and an expression evaluated
in that frame is racing. the work is not making threads keep running, which
already happens; it is stating exactly what is stable in each mode and refusing
to answer what is not

### async causal stacks

`await` preserves a stack. `create_task`, `ensure_future`, callbacks and
executors sever it, and what is left does not say who is responsible:

```python
async def h():
    raise TypeError("Something broke in h")

async def g():
    asyncio.create_task(h())   # the chain ends here
```

the traceback for that is **one frame** — `h`, and nothing above it — and the
process exits **0**, because an exception in an unretrieved task is reported by
a handler rather than raised. a debugger that says nothing here is agreeing that
the program succeeded

stitching means recording the stack at the moment a task is created and
presenting it above the frames that are actually running. cpython 3.14 added
external asyncio introspection (`python -m asyncio ps`), which is the supported
way into the task tree and fits the 3.14 line this project already draws

the rule that matters: the stitched frames **did not call** the running ones,
they scheduled them. presenting one seamless stack would be a fabricated call
chain, which is the exact lie this project exists to avoid. the join has to be
visible in the stack itself, not explained in a tooltip

### live heaps and retainers

"what is holding this object" — the question `gc.get_referrers` answers badly
and slowly. done natively the walk builds a reverse index from the GC's own
referent graph without allocating millions of python objects to do it, which
matters because the usual tools perturb the heap they are measuring

three honesty constraints decide the design:

- the debugger's own frames and temporaries are retainers, and have to be
    excluded **and** said to be excluded
- objects the GC does not track — ints, strs, anything without GC support —
    never appear in the walk. a retainer report that omits them silently is
    wrong; it has to state its coverage
- a heap snapshot of a running program is racy unless the world is stopped,
    which ties this directly to the mode question above

with PEP 768 attach (M8) this becomes a snapshot of a production process that
was never started under a debugger, which is where it earns most

### record and replay

stepping backwards, and asking what a variable was rather than what it is

full deterministic replay is not available: it would mean capturing every
syscall, thread interleaving, clock read and random source that the interpreter
and its C extensions touch. what is reachable is a **bounded recording** — an
event log with retention, giving "step back" and "what was this before" across a
window, and saying plainly where the window ends

this is the one item that fights the existing architecture. the performance
model is `DISABLE` on everything uninteresting; recording wants everything, and
the two cannot both be on. it is a mode, scoped to a region of a run rather than
a whole session

the trap is the obvious one: a recorder that interpolates state it did not
capture is a debugger inventing history. it reports what it has and refuses what
it does not

## not planned

- cpython 3.12 or older
- alternative implementations
- a `sys.settrace` path
- jinja2 templates, for now
