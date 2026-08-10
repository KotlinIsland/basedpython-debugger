# architecture

this is a design document. most of what it describes is not built yet — see
`ROADMAP.md` for what is. it exists so that what does get built is the thing
that was designed

## the shape

```text
  editor                          ai agent
    │ DAP                            │ MCP
    ▼                                ▼
  ┌────────────────────────────────────┐
  │  bpd_dap        │       bpd_mcp    │   adapters, no logic
  ├────────────────────────────────────┤
  │            bpd_core                │   the vocabulary, and Request/Response
  │            bpd_sourcemap          │   locations in, locations out
  ├────────────────────────────────────┤
  │            bpd_engine              │   owns targets, answers requests
  └───────────────┬────────────────────┘
                  │ bpd_protocol, over a socket
  ┌───────────────▼────────────────────┐
  │  bpd_agent, in the debuggee        │   a cdylib, PEP 669 callbacks in rust
  │  cpython 3.13+                     │
  └────────────────────────────────────┘
```

the engine is out of process. the agent is a native extension module loaded
into the debuggee. they talk over a length prefixed socket, and the agent never
runs python code to service the protocol

the dependency direction is acyclic and is the whole point of the layering:
`bpd_protocol` depends on `bpd_core`; `bpd_engine` and `bpd_agent` depend on
both; the adapters depend on `bpd_core` alone. an adapter that could reach
`bpd_engine` or `bpd_protocol` would be an adapter shaped by how the agent
happens to report something, and how the agent reports something would become
what a DAP client sees

## why the agent is native

PEP 669 lets a tool register a callback per event, and lets a callback return
`sys.monitoring.DISABLE` to say "never fire this event at this location again".
that single return value is the whole performance story, and it only pays off
if the callback itself is cheap

a python callback costs a python frame, an argument tuple, and an interpreter
re-entry on every event. registering a **native** function instead means the
interpreter calls straight into rust. combined with `DISABLE`, the steady state
for a program with three breakpoints in it is:

- the code objects that contain a breakpoint have `LINE` events enabled
    *locally*, via `sys.monitoring.set_local_events`. every other code object in
    the program has no instrumentation at all
- inside an instrumented code object, every line that is not a breakpoint line
    returns `DISABLE` the first time it is reached, and is never reported again

so the cost is bounded by the number of *distinct lines executed once* in the
handful of functions that hold breakpoints, not by the number of lines
executed. that is not an optimisation applied to a tracing debugger. it is a
different machine

**this is now measured rather than argued.** a loop whose six-line body runs
three million times, with a breakpoint held on a line inside the same function
that the program never reaches, runs in 165 ms against 164 ms bare — eighteen
million line locations, six of them ever reported. debugpy, given the same
program and the same breakpoint, takes 10 285 ms. the figures, the machine, and
what *else* they turned up are in [what bpd costs](overhead.md)

two things that page says are worth carrying here, because they qualify this
section rather than confirm it:

- **the native callback is not what separates `bpd` from debugpy today.**
    debugpy's pydevd has used `sys.monitoring` with a cython callback since
    cpython 3.12, so on every interpreter `bpd` supports both debuggers are
    native on this path. with no breakpoints set, both cost a program under 2%.
    what separates them is `DISABLE`, and it only shows once a breakpoint exists
- **the event path is not where a session's cost is.** attaching used to cost
    140–180 ms before the program's first statement, and about 120 ms of that
    was the interpreter loading a freshly written copy of the agent's shared
    object. nothing in this design document had a number for that, and it was
    larger than everything this section is about. it is now a content-addressed
    cache and the cost is paid once per build of the agent rather than once per
    launch — see [launching](launching.md)

### the callback does not get a frame

a PEP 669 callback is handed the code object and an instruction offset. it is
**not** handed a frame. materialising one costs `sys._getframe()` and a real
frame object

this is load bearing: the agent only materialises a frame when it has already
decided it needs one — a breakpoint line was hit, or a condition has to be
evaluated. the common case, "this event is not interesting", is answered from
the code object identity and the offset alone, without ever touching frame
state

any design that reaches for the frame first has thrown the win away

## breakpoint binding

a breakpoint is a user request against a *source location*. binding it means
finding the code object and line that will actually run

this is where debuggers quietly lie, so it is spelled out:

- a module's code object is a tree. a line inside a lambda, a nested function, a
    class body or a generator expression belongs to a **different** code object
    than the module's. binding walks `co_consts` recursively — anything less
    silently fails to bind breakpoints inside them. (list, dict and set
    comprehensions are the exception since cpython 3.12 and PEP 709: they are
    inlined into the function that contains them. the recursion is still what
    reaches that function)
- the executable lines of a code object come from `co_lines()`. a breakpoint on
    a blank line, a comment, or a `pass` that the compiler elided has no
    location. it moves to the next executable line **and the response says it
    moved**, with the line it moved to
- a breakpoint in a module that has not been imported yet cannot bind. it is
    reported **unbound**, and it binds later when the module is imported. it is
    never reported as bound-and-set
- a file that is not loaded by the interpreter at all — a typo in a path, a
    stale editor buffer — binds nothing, and says so

the rule: `bpd` never reports a breakpoint as set unless there is a code object
and an offset behind it

## stepping

stepping keeps as much of itself local as the interpreter allows: `LINE` and
`PY_RETURN` go on the code object of the frame being stepped in, and the events
are cleared when the step completes, so the program returns to its
uninstrumented steady state between stops

**two of the events this design wanted are not local events at all.**
`set_local_events` refuses `PY_UNWIND`, `RAISE`, `RERAISE`,
`EXCEPTION_HANDLED` and `PY_THROW`, so a step that has to know its frame was
left by an exception arms `PY_UNWIND` for the whole program; and `PY_START` was
never going to be local, because "some frame, somewhere, was entered" is what a
step in waits for

the traps, and what became of them:

- `sys.monitoring.restart_events()` re-enables **every** location that was
    disabled, process wide. it is the correct tool when a breakpoint is added to
    an already-running program — and it turns out to be the correct tool for a
    step too, because a line the step needs may have disabled itself on an
    earlier pass and PEP 669 has no per-location undo
- generators and coroutines re-enter the same code object, and so does a
    recursive call. what a step follows is therefore a **frame**, held by a
    strong reference, because the address of a freed frame is handed straight
    back to the next one

the whole of it is [stepping](stepping.md)

## the session core, and adapter parity

`bpd_core` holds two things and nothing else:

- the **vocabulary** that describes a debugged program — stops, breakpoints,
    frames, scopes, values, threads, the exceptions it raises and the refusals
    it earns
- the **capability surface**, as `Request` and `Response`: everything a client
    can ask of a session

it has no idea what DAP or MCP are. `bpd_dap` and `bpd_mcp` translate. neither
may contain a capability the other does not have, and neither may contain logic
— if an adapter needs to decide something, that decision belongs in the core
where both get it

### why the surface is data

a capability used to be a *method* on the engine's debuggee, and rust cannot
enumerate methods. the parity rule — no capability in one adapter and not the
other — was therefore a policy someone had to remember rather than something CI
could check, and the parity test this page promised could not be written at all

so the surface is an enum. `bpd_engine` answers it in one place, with a match
that has **no catch-all arm**: a variant added to `Request` is a compile error
in the engine rather than a request nothing answers. the ergonomic methods on
`Debuggee` build a request and come through that match, so there is a single
implementation of every capability rather than one per front end

the same enum then serves three consumers, which is the sign it is the right
shape — DAP requests map onto it, MCP tools map onto it, and the parity test
enumerates it

**it does not serve a fourth, and this page used to say it did.** the debug
script was going to be "a *tree* of it", and it cannot be: a `Request` names a
stop and a frame by **absolute id**, and the stop a step will run at does not
exist when the script is written. so `bpd_core::script::Step` is a vocabulary of
its own, relative to the stop the script is at, which the engine turns into
requests as it walks the tree. the script itself is one more `Request` variant,
and the parity test enumerates that — see [the debug script](scripts.md)

**both adapters are built and the test is written.** `bpd_dap::reach_of` and
`bpd_mcp::reach_of` each match `Request` with no catch-all arm, so a capability
added to the core is a compile error in both until someone says how each front
end reaches it; `crates/bpd/tests/parity.rs` then compares the two answers. a
capability enum alone would not have been enough — a front end can implement
every variant and still not offer a capability carried in a *field*, so
`bpd_core::parity::Facet` enumerates those too, and a front end whose protocol
genuinely cannot carry one says so with the reason rather than leaving a gap.
the whole of it is [the DAP adapter](dap.md) and
[the MCP adapter](mcp.md)

### one definition, not two

there is **no conversion layer** between the domain types and the wire. the
messages in `bpd_protocol` carry `bpd_core`'s types directly

two models are worth it when the wire has to stay stable independently of the
domain. here the agent and the engine are built and shipped together and the
handshake refuses a mismatch outright, so a protocol change costs nothing that
has not already been accepted. a mapping layer is not free either: every
mapping is a place a field can be dropped, which is exactly the quiet wrongness
this project bans. two models would buy a stability nobody needs at the price
of a seam nobody wants

the serde derives on core types are not a wire format in the sense that matters.
the rule is that `bpd_core` knows nothing about **DAP or MCP**, and it does not

what is *not* one definition is the request set. `bpd_core::Request` is what a
client asks of a session; `bpd_protocol::message::FromEngine` is what the
session asks of the agent. they differ where the session does something the
agent has no single request for — running the program is a resume followed by a
wait — and they share the vocabulary rather than mapping between two copies of
it

### where a failure lives

the split is what the failure *describes*:

- a failure that describes the **program** is a `bpd_core::Error` — an
    interpreter that cannot be debugged, a request made with nothing held, a
    request that names one stop while several are, a refusal the agent gave a
    reason for. a front end that depends on `bpd_core` alone still has to render
    every one of them
- a failure that describes **`bpd`'s own machinery** is a `bpd_engine::Error` —
    a socket, a spawned process, an agent artifact that could not be found, an
    interval that does not fit the wire. the engine carries a core error
    transparently rather than restating it

this is the mechanism behind the promise that an agent can do everything a
human can. it is not a policy anyone has to remember; it falls out of there
being one implementation

## attaching

attaching to a running process is PEP 768. cpython 3.14 exposes
`sys.remote_exec(pid, script)`, and documents the underlying [attachment
protocol](https://docs.python.org/3.14/howto/remote_debugging.html) — read the
target's debug offsets, write a script path into its control section, set the
pending flag, and the interpreter picks it up at its next safe point

`bpd` implements that protocol directly in rust rather than shelling out to
`sys.remote_exec`. the python level API requires the *calling* interpreter to
match the target's major and minor version, which would mean shipping or
locating a matching python just to start a debug session. the wire protocol has
no such requirement

on 3.13, `bpd attach` refuses. there is no ptrace fallback, no `gdb` injection,
no signal handler trick. those techniques work until they corrupt a process,
and a debugger that can corrupt the program it is measuring is not a debugger

## threads and free-threading

every thread hits callbacks independently, and **a stop holds one of them**. the
rest of the program keeps running: gdb's non-stop mode, and `bpd`'s default

the agent **releases the GIL for the duration of a stop**, so that is true on a
gil-enabled build as well as a free-threaded one. otherwise the GIL would be the
thing deciding the threading behaviour, and "threads keep running, except on the
interpreter most people have" is a capability ladder. stop-the-world is a mode
that is asked for explicitly, and it names the threads it could not stop

the registry of held threads, their mailboxes and the parking are native, behind
mutexes and condition variables that do not assume a GIL. the control connection
is read by a rust thread that never takes one, because every answer has to be
computed on the python thread the question is about

free-threaded builds are a target, not a variant to be tested later. anything in
the agent that would only be correct under the GIL is a bug on every build,
because the GIL was never the guarantee it looked like

the whole of it, and what it costs, is [threads](threads.md)

## the python layer

there is as little of it as possible. where one is genuinely required — an
import hook, a django integration point, anything that must be a python object
to be installed somewhere — it is written in **basedpython** and lives under
`python/`, transpiled as part of the build

it never sits on an event path
