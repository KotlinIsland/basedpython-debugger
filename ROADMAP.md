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
- [x] an unsupported interpreter is refused before anything is launched, by
        name. the refusal names the interpreter as given, the version it found
        and the minimum, and it comes from `bpd_engine::launch::start`, which
        every front end goes through — so a DAP client gets the same sentence,
        and `a_client_is_refused_the_same_interpreter_the_command_line_is` is
        what says so. "before" is the part that is actually tested:
        `crates/bpd/tests/launch_refusal.rs` runs all three launch forms with a
        program that would announce itself, and requires that it did not

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
- [x] the registration stub vs code needs to name the executable at all. it
        resolves a configuration's `type` through an extension contributing a
        `debuggers` entry and offers no way to point at a binary, so without one
        no vs code user can run `bpd` however complete the adapter is.
        `editors/vscode/` contributes the type, the launch attributes and a
        `PATH` lookup for the binary, and `crates/bpd_dap/tests/vscode.rs` fails
        if its schema and `bpd_dap::Configuration` disagree.
        **what ticked it**: `editors/vscode/test/` downloads a pinned vs code
        and starts a session through the extension — a breakpoint set with
        `vscode.debug.addBreakpoints`, a stop observed as the frame vs code
        focuses, the stack and a local read, and a resume that ends with the
        program's own exit. the assertions are the editor's debug state, not
        bpd's output, and CI runs it headless under `xvfb`. driving it also
        found a bug nothing else could: vs code renders an error thrown out of
        `createDebugAdapterDescriptor` itself, so the `showErrorMessage` beside
        the throw was the same sentence in front of the user twice. see
        [the vs code extension](docs/development/vscode.md)
- [x] **intellij drives it.** a hard requirement, not a nice-to-have, and a
        different problem from vs code rather than the same one twice.
        `editors/intellij/` is a plugin registering
        `platform.dap.debugAdapterSupportProvider` — the platform's own general
        DAP layer, which jetbrains' debugpy backend is a *client* of rather than
        the owner of — with an adapter id of its own, plus a run configuration
        type and the same `PATH` rule the vs code extension carries.
        **what ticked it**: `editors/intellij/src/test/` downloads a real
        pycharm and starts a session through the plugin — a breakpoint set with
        the IDE's own `XBreakpointManager` on the python plugin's line
        breakpoint type, a stop observed as `XDebuggerManager` holding a paused
        session, the frame the IDE focused and a local read out of its
        variables view, and a resume that ends with the program's own exit. the
        assertions are the IDE's debug state, not bpd's output, and CI runs it
        headless — the platform test framework needs no display. see
        [the intellij plugin](docs/development/intellij.md)
        - **the layer is `@ApiStatus.Experimental`, not `Internal`**, its module
        descriptor says `visibility="public"`, and both its extension points
        are `dynamic="true"`. that was the open question and it is closed
        - **but it is not in every IDE, and that is the real constraint**:
        `intellij.platform.dap` ships in the unified PyCharm and in IDEA
        Ultimate, and **not** in PyCharm Community or IDEA Community, at
        2026.2.1. the marketplace lists jetbrains' own DAP plugin as
        `PYCHARM_COMMUNITY` compatible anyway, which its own `since-build`
        supports and the product layout does not
        - **`supportsSingleThreadExecutionRequests` is honoured.**
        `DapDebugSessionImpl.resume` reads it off the `initialize` response
        and sets `ContinueArguments.singleThread` from it, so a non-stop bpd
        session resumes in pycharm the way it does in vs code
        - **`startDebugging` is not advertised** by the layer's default
        `createInitializeParams`, and `DapClient` does not implement lsp4j's
        reverse request. so `debug children` is refused there by name, with
        the adapter's own sentence
        - worth reading twice: jetbrains' own stated reason for their debugpy
        plugin is **"lower debugging overhead — especially on Python 3.12+
        thanks to PEP 669 monitoring hooks"**. that is this project's thesis,
        arrived at independently, and it means the comparison in
        [what bpd costs](docs/development/overhead.md) is the comparison
        that matters inside pycharm too, not just against a command-line
        debugpy — per-location `DISABLE` against per-code-object
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

**intellij needed a plugin**, and the exclusion is written narrowly for it
rather than bent. vs code needs an extension because it resolves a
configuration's `type` through one; a jetbrains IDE resolves a debug session
through a run configuration type and a `ProgramRunner`, and neither can be named
from a settings file. so the minimum that makes `bpd` reachable there is in
scope on the same grounds the vs code stub is — a run configuration, an adapter
registration and a `PATH` lookup — and everything above that minimum is still
out: no panels, no tool window, no actions, no inline values

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

### M4 — DAP · the adapter is built, and vs code has driven it

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

**the registration vs code needs is built, and vs code has driven it.**
`editors/vscode/` contributes `"type": "bpd"`, the launch attributes, and a
lookup that finds `bpd` on `PATH` or says which of two things to do about it —
and nothing else, no panel and no view. its schema cannot drift from
`bpd_dap::Configuration` without `cargo test` failing, and `editors/vscode/test/`
starts a real session inside a real vs code, so "it loads, it activates, it
stops on a breakpoint" is a test rather than a hope;
[the vs code extension](docs/development/vscode.md) says what that still leaves
uncovered. `nvim-dap` names the executable in the configuration itself and has
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

this was recorded as **blocked upstream** — "the transpiler has to emit a source
map with provenance for generated lines and a hash of both artefacts". **most of
that is wrong, and the sibling repository had already found it out.**

`by run` writes `_by_sourcemap.py` beside the transpiled output today:

```python
SOURCEMAP = {
    "/tmp/.tmpXXXX/demo.py": ("/abs/path/demo.by", [None, 0, 1, 2, 3, ...]),
}
```

indexed by **generated** line, holding the `.by` line it came from — and `None`
where the transpiler emitted a prelude with no source. that is the map, and the
`None` **is** the provenance. `basedpython-pycharm` ships source-mapped `.by`
debugging on it through debugpy's `setPydevdSourceMap`, verified end to end, and
its `docs/debugging.md` says in as many words that the same "blocked upstream"
belief it had recorded for a long time was false

what is genuinely missing is the **hash of both artefacts**, and only that. it is
what this project's rule needs rather than what a map needs: a source map that
was not verified against the thing it maps is exactly the line number
[the contract](README.md) refuses to report. so the upstream ask is one digest,
not a mapping format

until it lands `bpd` debugs the generated python and says that is what it is
doing — which is now a smaller gap than this entry claimed, and a much smaller
ask. see [bpd and the basedpython pycharm plugin](docs/development/basedpython-pycharm.md)

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

**the second step is built too**: a forked child no longer writes into the
session its parent owns. it inherited the agent's monitoring state and both
descriptors of the control connection, and two processes putting length-prefixed
frames into one socket desynchronise it — so the child gives all of it up in
`os.register_at_fork(after_in_child=…)` before it runs a line. that closed a bug
reachable without any of the rest of this, and a second one nobody had looked
for: a child holding those descriptors kept the session open after its parent
exited, which is the shape of every worker pool and reloader

closing it turned up a parity violation and closed that as well. the agent's
reader thread made every debuggee multi-threaded, and cpython warns on
`os.fork()` in a multi-threaded process — which a program **records in its own
data**, not merely on stderr, so `warnings.catch_warnings(record=True)` returned
a different list under `bpd` than without it. the reader now stands down across
the fork and goes back afterwards

what is left is the propagation. the hook is not the hard part and not the
answer either: an audit hook *can* rewrite a spawn — appending to the argument
list the `subprocess.Popen` event carries puts the argument in the child, on
every supported release, and the caller's own list is untouched because what is
audited is already a copy. it is still not the mechanism, because it works by
where cpython raises the event relative to reading the list back, which nothing
documents. so the way into an **exec'd** child remains its environment, which is
the one channel the parity guarantee keeps clean

a **forked** child needs none of that, because it inherits memory: it already
holds everything it would need to reconnect. that is the cheapest real
propagation available and it is the next step

the session id two debuggees require is built, ahead of the propagation rather
than after it, because nothing above it can be built without one: every id an
agent mints — a stop's number, and the frame and snapshot ids on it — counts
from one in its own process, so two agents name two different things the same.
the engine mints a `SessionId`, a stop says which session it is of, a request may
name one, and one that names none is the only session there is — refused rather
than picked when there is more than one. there is still one session; see
[sessions](docs/development/sessions.md)

the **transport** a second session would need is built too, and it is a complete
feature on its own: `bpd dap --listen` speaks the protocol on a loopback socket
instead of stdin and stdout, which is what a client that did not spawn the
adapter needs. `startDebugging` only does the signalling — the client it asks to
start a second session has to be able to reach a second adapter, and this is how.
none of the child half is in it: one session, one connection, exactly what
`bpd dap` already does, reachable a second way. what it does add is an attack
surface, since reaching that port is running code as whoever started `bpd`, so it
binds loopback with no address to widen and refuses a connection that cannot
present the session token it printed when it bound. see
[the two transports](docs/development/dap.md#the-two-transports)

**the reconnect itself is five pieces**, and the first two are built

1. ~~**the agent's shared statics made fork-replaceable.**~~ done. the writing
    end, the reader and the held-thread table are cells behind an atomic pointer
    and a forked child replaces all three with a store apiece, taking none of
    them. the stop counter came out of the table into an atomic of its own,
    because a child has to **keep** it: resetting to one reissues numbers the
    parent has already reported. what had been keeping the hazard out of reach
    was the GIL on a gil build and — measured on 3.14t — `os.fork()`'s
    stop-the-world on a free-threaded one, neither of which is a property of
    this agent
1. ~~**the engine holding two sessions.**~~ done. the listener a debuggee
    attached on is kept open, an agent that presents its token becomes a second
    session of it, and the wait polls the connection and the listener rather
    than blocking on one. a program bpd did not start ends as `Running::Ended` —
    over, with no exit status, because bpd is not its parent and never learns
    one — and `terminate` on it is refused by name. with two open, a request
    that names none is refused by `only_session`
1. ~~**the child reconnecting**~~ done, opt-in, decided at fork time from a flag
    the engine set earlier
1. ~~**MCP's `sessions` tool and a session argument**~~ done
1. ~~**DAP's mediator and multi-connection listener**~~ done

**the last piece is built too**: a child that **`exec`s**. `subprocess`, and
`multiprocessing` with the `spawn` and `forkserver` start methods, reach a fresh
interpreter with none of this process's memory in it, so the agent has to be
found — through `PYTHONPATH` ending in a directory of its own holding an
eleven-line `sitecustomize`, appended so it cannot shadow anything, with a
per-debuggee token that is **not** the session's. it is the same `debugChildren`
switch: one question a user asks, two ways a child comes into being

that is the one thing in bpd a program can notice, and the rule is now written
into `crates/bpd/tests/launch_parity.rs` in both directions. the **off** case is
byte-identical to what it always was — neither assertion that compares the whole
environment and the whole `sys.path` against a bare run moved. the **on** case is
an enumerated list of four names with a reason each, and a fifth fails

so `runserver` works without `--noreload`, and
[django templates](docs/development/django-templates.md) says so on the strength
of `a_breakpoint_in_a_template_the_reloaders_child_renders_is_hit_in_the_child` —
a fixture in `restart_with_reloader`'s own shape against a real django, with the
breakpoint bound in the child's own template engine mid-render

what is deliberately not in it: a token rotated per child, which
`subprocess` fixing the environment block before the audit event is raised makes
unreachable without the undocumented rewrite this design rules out; and windows,
where `debugChildren` is refused because there is no `fork` for the other half of
it

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

**measured, and it moves this milestone**: on macOS PEP 768 needs privileges
this project cannot assume. `sys.remote_exec` on 3.14 refuses with

```text
PermissionError: Cannot get task port for PID 69506 (kern_return_t: 5).
This typically requires running as root or having the
'com.apple.system-task-ports' entitlement.
```

and it refuses the same way when the caller is the target's **own parent**, so
it is a blanket `task_for_pid` restriction rather than anything about process
relationship. writing the protocol in rust does not change that: the restriction
is the kernel's, and it applies to whoever reads the memory

so attach is a **linux-first** feature, and the honest consequence is that the
one machine this project has been developed on cannot run it without `sudo`
— which means nobody here can watch it work, which is the bar this page sets.
what has to come with M8 is therefore a refusal naming the entitlement and the
remedy, and a decision about whether an unverifiable-on-macOS milestone is
started before the cross-platform work it depends on

### M9 — reset stack frame to here · done

two operations, separate because their obstacles are different, per
[set next statement, and restart frame](docs/development/jumps.md)

**restart frame** — re-enter the frame a thread is executing from the top. it
re-enters with **what its parameters hold now**, not with the arguments the call
was made with: nothing captured those, and capturing them would mean copying
every argument of every call in the process on the event path. the honest
limitations, which the ui states rather than buries: side effects already
performed are not undone, and the frames above the chosen one are not discarded

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

that second one also means **restart frame is reachable for the frame a thread
is executing** by the same mechanism: move to the first line of its code object.
measured working — and the destination is the line of that code object's first
*instruction* that carries one, which is not `co_firstlineno`: a module's first
instruction has line `0` and a closure's has no line at all

a generator, coroutine or async generator frame is refused a restart. the first
instruction of such a code object is the `RESUME` its driver sends into rather
than the top of the body, and moving there **ends** the frame — measured on 3.13,
3.14 and 3.15, the next `next()` raises `StopIteration` and nothing was yielded.
set next statement to a line of the body works there and is what the refusal
names

what it does not give is the DAP operation's other half — discarding the frames
*above* a chosen one. that was checked rather than assumed: there is no public C
API that pops a frame, `frame.clear()` answers `RuntimeError: cannot clear an
executing frame`, and pydevd does not implement it either — `supportsRestartFrame`
is `False` in the capabilities it advertises, and the only `restartFrame` in the
whole vendored tree is the DAP schema's own definition

the only remaining route is making each intervening frame **return**, and the
honest ways to do that all run `finally` and `except` blocks on the way out —
which is a different operation from discarding a frame and has to be described as
one. so the two halves of this milestone are not equally reachable, and the entry
should not imply they are

what cpython refuses it refuses loudly, and that refusal is passed through with
its reason intact. what it does **not** refuse had to be measured rather than
assumed, and two of those are the ones a user has to be told about:

- **a jump does not run the cleanup of a block it leaves.** jumping out of a
    `with` does not call `__exit__` and jumping out of a `try` does not run its
    `finally` — cpython accepts both. `bpd` does not undo them and does not
    pretend otherwise
- **a jump binds every unbound local of the frame to `None`**, warning
    `RuntimeWarning: assigning None to 2 unbound locals`. that is a change to the
    program the debugger caused, so the names are read back out of the frame and
    reported

and one thing cpython accepts that `bpd` refuses: a move in a frame that is
**suspended in a call**. it is accepted, and the frame then runs on with a value
stack that no longer matches where it is — a function jumped in this way returned
a value it never computed. only the frame a thread is executing can move

### M10 — hot code replacement · done

replace the code a live process is running with the code that is now on disk,
without restarting it: `function.__code__` is rebound on every function object
in the **process** — found through the heap rather than through the module's
namespace, so a closure a factory handed out, the original a decorator captured
and a function in somebody's registry are all reached — and nothing else is
touched. the top level is not re-run, no name is bound or unbound, and no object
is created

**classes came with it rather than after it.** a method *is* a function object in
the class dictionary, so every instance that already exists sees the new one at
once, with nothing written to any dictionary; and "a changed class layout" turned
out not to be a check of its own but the body rule applied to a class body, which
is the only other code object cpython leaves unoptimized

the rule that makes it total: the file is compiled and the tree compared against
the tree that is running, and it is applicable exactly when **every difference is
inside the body of a function that exists in both and takes the same arguments**.
that is the same machinery `Source` already uses to refuse to show source it
cannot prove, inverted. it is never applied partially, and a refusal carries every
reason it had rather than the first

two things had to be measured rather than assumed, and both changed the design:

- **a class body carries its own source line.** since 3.13 cpython stores
    `__firstlineno__` in every class body — `LOAD_CONST` of the line on 3.13, and
    `LOAD_SMALL_INT` of it on 3.14, 3.15 and 3.14t — so a class that merely moved
    down the file is different code. left in the comparison it refuses every edit
    above a class as a changed class layout, which is most edits. that one
    instruction is masked, and a test fails if cpython changes
- **replacing `__code__` under a live frame is safe**, on 3.13, 3.14, 3.15 and
    3.14t. the frame keeps its own reference to its code object, runs the old code
    to completion, and the next call gets the new one. this milestone was written
    expecting the opposite — the neighbouring `f_lineno` assignment on a suspended
    frame really does abort the interpreter — and it does not generalise. so the
    refusal for a live frame stands, and its reason is **not** crash prevention:
    between the assignment and that frame returning the process is running two
    versions of one function, and a stack whose frames behave two different ways
    is evidence about neither. a suspended generator, coroutine or async generator
    counts as such a frame, because it will run that code the moment anything
    sends into it

what it costs is stated rather than hidden: adding a function, deleting one or
changing a module-level constant is a change to the module body, and applying one
would mean re-running the top level — which is running the program a second time,
not reloading it. those are refused

**what triggers it was already built.** the source around a frame is compiled and
the frame's own code object has to be in what comes out, so `not_the_same_code`
is what tells a client the file on disk is no longer what is running and that a
replacement is worth offering

see [hot code replacement](docs/development/hot-code-replacement.md)

what is **not** built: applying a replacement while a frame is running the code
and reporting which frames are still on the old version. it is more useful and a
strictly weaker guarantee, and it was left out deliberately — "never half a
process" is the rule this milestone commits to

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
