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

- [x] `bpd` runs a script, a module (`-m`), and a package the same way the
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

- [x] step over, step in, step out, continue, and pause
- [x] stepping is correct across generators, coroutines, comprehensions,
        exception unwinding, and re-entrant calls
- [x] with several threads running, a stop holds **one** thread and the others
        keep running, on every build, and the stop names the thread it holds
- [x] stop-the-world is available as an explicit mode, and a thread parked in a
        C call is reported as running in native code rather than counted as held
- [x] a stop that blocks other threads because the held thread owns a lock is
        detected and reported, not left looking like `bpd` hanging

**state**

- [x] the stack, with each frame's source location, for every **held** thread
- [x] a thread that is not held has no stack reported for it, because reading
        one off a running thread describes a moment that has gone. what it gets
        instead is a census saying whether it moved, labelled as the sample it is
- [x] locals, globals and closure variables read from the scope that was asked
        for
- [x] a local can be **written**, and the write is visible to the program
- [x] object graph expansion with an explicit budget, and an explicit statement
        of what was left out when the budget is hit
- [x] expression evaluation in a chosen frame, where a failure returns the
        exception

**the two front ends**

- [x] a DAP adapter a real client drives end to end — a breakpoint, the stack,
        a variable read and written, a step, a resume. driven by a test and by
        `nvim-dap`, which names the executable in its own configuration
- [ ] the registration stub vs code needs to name the executable at all. it
        resolves a configuration's `type` through an extension contributing a
        `debuggers` entry and offers no way to point at a binary, so without one
        no vs code user can run `bpd` however complete the adapter is.
        **built and not ticked**: `editors/vscode/` contributes the type, the
        launch attributes and a `PATH` lookup for the binary, and
        `crates/bpd_dap/tests/vscode.rs` fails if its schema and
        `bpd_dap::Configuration` disagree — but nobody has installed it in vs
        code and started a session, and this project does not tick a box on a
        thing it has not seen work. see
        [the vs code extension](docs/development/vscode.md)
- [x] an MCP server exposing the same session
- [x] a parity test that enumerates the capabilities in `bpd_core` and fails if
        either adapter is missing one. the rule is enforced by CI, not by review

**evidence**

- [x] a benchmark in the tree comparing a run under `bpd` with no breakpoints
        against the same program run bare, and with breakpoints against debugpy.
        `crates/bpd/benches/overhead.rs`, reported in
        [what bpd costs](docs/development/overhead.md) — which also records what
        it contradicted, since two of the claims it was written to check turned
        out to be wrong
- [ ] integration tests that spawn real interpreters across the version and
        build matrix

### explicitly not in the MVP

basedpython source maps, django templates, attach, restart frame, hot module
reloading, and editor integration beyond a launch configuration

that last exclusion was written before anyone checked what vs code needs, and it
ruled out the thing that makes a launch configuration work at all. **the
registration stub is in scope**: an extension that contributes a `debuggers`
entry naming the executable and nothing else. what stays out is everything
built *on top* of that — panels, custom views, an inline value renderer, any UI
of our own

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

### M3 — stepping, frames and values · done

execution control and state, per the criteria above

the stopped-state half is frames, the stack, scopes, values and evaluation — see
[reading a stopped program](docs/development/state.md)

**execution control is built.** step over, step in and step out, a pause that
reaches a program with nothing held, and the two exception breakpoints. what a
step follows is a **frame** rather than a code object, because a recursive call,
a second generator of the same function and a coroutine awaited from two places
all re-enter the same code object — and it holds the frame rather than its
address, because cpython hands a freed frame's address straight back to the next
one

a `yield` is a suspension rather than a return, so a step over an `await` lands
on the next line of the same coroutine instead of somewhere in the event loop.
arming a step costs a `restart_events()`, because a line it has to be offered
may have disabled itself on an earlier pass and PEP 669 has no per-location undo

"break on raised" and "break on uncaught" are different questions and are
answered at different moments. cpython raises its `RAISE` event again in every
frame an exception propagates into, so one exception is one stop, reported where
it was raised; and whether anything will catch it is only knowable at the unwind
out of the outermost frame, so that is where it is answered rather than
predicted. see [stepping](docs/development/stepping.md)

**the thread model is built.** a stop holds **one thread**, and every other
thread keeps running. gdb calls this non-stop mode and DAP exposes it as
`supportsSingleThreadExecutionRequests`; it is the mode that lets a live server
keep serving while one handler is inspected

it applies on **every build**, not only free-threaded ones. the agent releases
the GIL for the duration of a stop, so a gil build behaves the same way rather
than freezing the process by accident — this project does not ship a capability
ladder, and "threads keep running, except on the interpreter most people have"
is one

stop-the-world is available as an explicit mode, because a coherent view of a
data structure needs it, and it names the threads it could not stop rather than
counting a thread parked in a C call as held

what it costs is not hidden:

- **the held thread still holds its locks.** the import lock is the one cpython
    makes knowable, because its machinery runs in python frames, and a stop
    inside one says which module it is holding. cpython exposes **no owner for
    any other lock**, so that is not claimed — what is reported instead is the
    other end of it: a census saying which threads got nowhere between two
    samples, and where
- **a read is a sample, not a snapshot.** every read carries the mode it was
    taken in. the one thing that is a snapshot in either mode is the held
    thread's own stack, which cannot be torn down while it is held
- **the program can end while a thread is held.** it cannot exit — finalization
    joins non-daemon threads — so the agent says which threads are still held
    rather than leaving a process that looks like a hang
- **concurrent stops queue.** a second thread reaching a breakpoint reports its
    own stop immediately, and resume names the threads it means

the state half has landed too: frames and frame identity, the stack, the four
scopes a frame really has, writing a variable the program then goes on to use,
values, object graph expansion under a budget that says what it left out, and
expression evaluation. so has execution control — stepping, pausing and the
exception breakpoints. see [the stopped state](docs/development/state.md),
[stepping](docs/development/stepping.md) and
[threads](docs/development/threads.md)

### M4 — DAP · the adapter is built, the extension is unverified

`bpd dap` speaks the debug adapter protocol on stdin and stdout. a breakpoint is
set, hit, the frame's locals read, a local written, and a step taken — proved
end to end against a real interpreter in `crates/bpd/tests/dap.rs`, where the
write is checked by the **program's own output** rather than by reading it back

`bpd_dap` depends on `bpd_core` alone. the session arrives through traits the
adapter defines, and `crates/bpd/src/dap.rs` is where `bpd_engine` is put behind
them — so nothing about how the agent reports something can shape what a DAP
client sees

what it advertises is only what is implemented, and the absences have reasons
rather than being oversights: DAP's `hitCondition` is a string whose meaning is
a per-client convention, and bpd refuses one rather than guessing which was
meant. `supportsSingleThreadExecutionRequests` is on, because a stop holds one
thread and a client told otherwise would render a stop that never happened

**the registration vs code needs is built, and has not been driven.**
`editors/vscode/` contributes `"type": "bpd"`, the launch attributes, and a
lookup that finds `bpd` on `PATH` or says which of two things to do about it —
and nothing else, no panel and no view. its schema cannot drift from
`bpd_dap::Configuration` without `cargo test` failing. what has **not** happened
is anyone installing it and starting a session, so the criterion stays unticked;
[the vs code extension](docs/development/vscode.md) lists what that leaves
unverified. `nvim-dap` names the executable in the configuration itself and has
worked since the adapter was built

the whole of it is [the DAP adapter](docs/development/dap.md)

**the move it needed had already landed.** `bpd_core` is what the contract says
it is:
the domain vocabulary, and the capability surface as a `Request`/`Response`
pair naming everything a client can ask of a session. `bpd_protocol` depends on
it and keeps only what is genuinely wire — framing, the handshake, the token,
the env names, the agent envelopes. `bpd_engine` keeps processes and the
connection, and answers a request in one match with no catch-all arm, so a
capability added to `Request` is a compile error rather than a silent gap

what forced it was not tidiness: a capability used to be a *method*, rust
cannot enumerate methods, and the parity test this repository promises could
not be written at all. as data, the same enum serves three consumers — DAP maps
onto it, MCP tools map onto it, and the parity test enumerates it. `bpd_dap` and
`bpd_mcp` depend on `bpd_core` alone

it was going to serve a fourth: the debug script was to be a *tree* of
`Request`, and when M5.7 was built that turned out to be impossible — a
`Request` names a stop and a frame by absolute id, and the stop a step will run
at does not exist when the script is written. the script is one more variant,
and its steps are a vocabulary of their own

there is no conversion layer between the domain and the wire, and that is
deliberate — the reasoning is in
[architecture](docs/development/architecture.md)

### M5 — MCP · the server, the control tools, the debug script and the parity test are built

`bpd mcp` speaks the model context protocol on stdin and stdout. `bpd_mcp`
depends on `bpd_core` alone, and `crates/bpd/src/mcp.rs` is where `bpd_engine`
is put behind it — the same shape as the DAP adapter, so nothing about how the
agent reports something can shape what an ai agent sees

**every control tool returns the stop it produced.** `continue_`, `step_over`,
`step_in`, `step_out`, `wait` and `pause` each block until the program stops
again and return why it stopped, which thread, and the top of its stack. the
server writes nothing that is not the answer to something that was asked, so
there is no event stream to correlate — stepping five times costs five tool
calls, counted against a real interpreter in `crates/bpd/tests/mcp.rs`

**every one of them requires a deadline**, and what a timeout may say turned out
to be narrower than the design assumed. everything the agent inside the debuggee
answers, it answers on a thread it is *holding* — including the thread census —
so a program with nothing held cannot be sampled at all. a timeout therefore
reports that the program is still running and carries **no location of any
kind**, and names what can be done instead: keep waiting, or pause and get a
real stop. a sampled stack presented as a stopped one is the debugger reporting
a state the program was not in, and a sample that cannot be taken is not one

**the parity rule is now a build failure.** `crates/bpd/tests/parity.rs`
enumerates `bpd_core`'s capabilities and compares what the two adapters claim
about each. enumerating the request variants was not enough on its own: a front
end can implement every one of them and still not offer a capability carried in
a *field*, which is exactly what DAP's hit condition is — so those are
enumerated beside them, and a protocol that genuinely cannot carry one says so
with the reason, in a list the test checks by hand

what is **not** built: MCP resources and prompts, which are the layers that must
not be written to compensate for an interface that does not explain itself

the whole of it is [the MCP adapter](docs/development/mcp.md)

**the declarative state query is built, and so is snapshot and diff.** one call
says what is wanted about a stop — frames, the scopes of each, expressions
evaluated in a frame, the source around each line, under one byte budget — and
is answered with it. it is composed of the requests the tree walk is made of, so
the two cannot disagree about a value; what it removes is the round trips

the answer is kept under a content addressed id, and a second call says what
**changed** between two of them rather than handing back both. a snapshot does
not go stale — it is a reading already taken rather than a promise to take one,
which is what DAP's variable reference is and why that one does. a value a bound
cut short in either snapshot is reported as *not compared*, because "unchanged"
is a claim and half a value is not evidence for it

source is only ever shown when it can be **proved**: the debuggee compiles the
file and requires the frame's own code object in what comes out, line table
included. a file edited since the interpreter read it says so rather than showing
a line one off, which is the bug every traceback's `linecache` has. the whole of
it is [the state query](docs/development/queries.md)

**the debug script is built.** a schema-validated tree of steps with its own
branching — `step_over`, `run_to`, `eval`, `stack`, `log`, `if`, `while`,
`finish` — submitted in one call and answered with a transcript of what happened
at every one. it is a capability of `bpd_core`, so DAP has it too, through the
custom request `bpd/runScript`

the steps run in the engine; only the predicates reach the debuggee, through the
machinery a breakpoint condition already uses. the **transcript is the return
value** rather than the final state, because a client given only where a script
ended cannot tell why. a budget is required on three axes and exhausting one
returns the transcript so far labelled partial; a step that fails halts the
script; every loop carries a bound, so a script that cannot be shown to
terminate does not deserialise

`run_to` lands here rather than among the control tools, and that was M5.2's
finding: as a tool it is either a decision an adapter makes about the program or
a capability DAP has no request for, and its one-shot breakpoint cannot be
removed from a program that timed out still running. as a *step* the engine owns
the whole composition — including arming a pause to take its own breakpoint off
a program that is still running, which the transcript says it did

the whole of it is [the debug script](docs/development/scripts.md)

and **how an agent learns bpd**, in order: tool schemas, then errors, then
resources and prompts. the first two are what shipped, which is the order the
design asks for — a document is not a reliable teaching surface and a tool
result is

**the MVP is M0 through M5**

## after the MVP

### M6 — basedpython

`.by` breakpoints and `.by` frames through a verified source map

this milestone has an **upstream dependency**: the basedpython transpiler has to
emit a source map with provenance for generated lines and a hash of both
artefacts. that work is in basedpython, not here. until it lands, `bpd` debugs
the generated python and says that is what it is doing

### M7 — django templates · done

template frames, template context and template expression evaluation, per
[django templates](docs/development/django-templates.md)

there is **no debug-mode refusal**. the design expected one, and measuring
django found nothing to refuse: `Parser.extend_nodelist` sets `node.token` and
`node.origin` unconditionally, and the engine's `debug` option only chooses a
lexer that additionally records a character position bpd never reads. the test
suite runs the whole fixture with the option on and off and requires the same
answer

what is left out is named on that page. the largest is that stepping from a
template frame steps the python underneath it rather than node to node

### M7a — the process that serves is not the process launched

**this is a hole in M7 rather than a milestone after it**, which is why it sits
here and not at the end

`django.utils.autoreload.restart_with_reloader` calls `subprocess.run(args)` and
the parent does nothing afterwards but wait on the exit code — read in django
6.1. so under the default `runserver`, the **child** serves every request, and
`bpd launch manage.py runserver` attaches the agent to a supervisor that never
renders a template. the same is true of anything using `multiprocessing`, and of
flask's reloader

nothing is reported wrongly: the supervisor never imports the template engine,
so the hook never arms and a template breakpoint is reported **unbound**, which
is the truth. the feature is unreachable, not dishonest — and until this is
built, `--noreload` is the answer, which
[django templates](docs/development/django-templates.md) now says

what makes it hard is that a debuggee must be able to hand a child the same
session without the child's launch going through `bpd` at all, and that the
child is spawned by code `bpd` does not control. debugpy does this with
`subProcess` defaulting to true and a custom `debugpyAttach` DAP event — which
predates the standard `startDebugging` reverse request, and that is the shape to
use rather than debugpy's

**the first step of it is built**: `bpd` notices a python child and reports it,
through a native audit hook, in the CLI, in DAP and in MCP. it does not
propagate anything, and the child runs exactly as it would have — including the
guarantee that a program cannot tell it is being debugged, which is unchanged
and is still tested against a bare run. the reason that is worth having on its
own is that the symptom it removes is a breakpoint reported unbound with no
reason given

the audit events differ by release — `_posixsubprocess.fork_exec` only became
one in 3.14 — so the watch list is chosen from the running interpreter, and the
one thing 3.13 cannot see at all, a `multiprocessing` child started with the
`spawn` or `forkserver` method, is **announced** rather than left as a silence.
see [child processes](docs/development/subprocesses.md)

what is left is the propagation, and the hard part of it is not the hook: an
audit hook can **observe** a spawn and cannot rewrite its arguments, so the only
way into an exec'd child is its environment — which is the one channel the
parity guarantee currently keeps clean

### M8 — attach

PEP 768, implemented as the wire protocol in rust rather than through
`sys.remote_exec`, so **no local interpreter is needed at all**. 3.14 or newer,
and a refusal below that

that difference is the whole of it on the happy path, and worth stating plainly
because the obvious contrast is gone: debugpy 1.8.21 also prefers PEP 768 —
`server/cli.py:455` gates on `hasattr(sys, "remote_exec")` and `:474` calls it.
what it does that this will not is `:480`, "Will reattempt using pydevd", which
shells out to gdb. so the contrast is not "PEP 768 versus injection", it is that
`bpd` needs no python of its own to attach and never falls back

### M9 — reset stack frame to here

two operations, separate because their obstacles are different

**restart frame** — discard the frames above a chosen frame and re-enter it from
the top with its original arguments. the honest limitation, which the ui states
rather than buries: side effects already performed are not undone

**set next statement** — move execution to another line inside the current frame.
this was written down as the open question of the milestone, on the grounds that
cpython permits assigning to `frame.f_lineno` only from inside a trace function
and `bpd` does not install one. **measured, and it is not a question**: from a
`sys.monitoring` LINE callback with `sys.gettrace()` returning `None`, on 3.13,
3.14 and 3.15, a forward jump takes effect, a backward jump takes effect and the
block genuinely re-executes, and an illegal one raises `ValueError: can't jump
into the body of a for loop`. pydevd sets `f_trace` first, which is why reading
it suggests otherwise — that is the `settrace` era's requirement, not this one

so the loud refusal this milestone needs is cpython's own, with a reason already
in it, and what is left to build is the plumbing. two traps found while
establishing it, both about which lines produce an event:

- `co_lines()` reports the `def` line, and **no LINE event is ever delivered for
    it**, so a jump origin taken from `co_lines()` can be a line that never
    arrives
- **no LINE event is delivered for the line jumped *to***. measured on 3.13,
    3.14 and 3.15: jumping back from `C` to `A` in a three-statement body runs
    `A, B, A, B, C` while the events are `A, B, C, B, C`. execution really is at
    the destination and really does run it — the event for it is simply not
    sent. so where the program now is has to be **derived from the jump**, and a
    debugger that waits to be told will report the line after the one it moved to

that second one also means **restart frame is reachable for the topmost frame**
by the same mechanism: jump to the first statement and write the original
arguments back through the PEP 667 proxy. measured working. what it does not
give is the DAP operation's other half — discarding the frames *above* a chosen
one — because nothing in cpython pops a frame from outside it. so the two halves
of this milestone are not equally reachable, and the entry should not imply they
are

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

### async causal stacks

`await` preserves a stack. `create_task`, `ensure_future`, callbacks and
executors sever it, and what is left does not say who is responsible:

```python
async def h():
    raise TypeError("Something broke in h")


async def g():
    asyncio.create_task(h())  # the chain ends here
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
