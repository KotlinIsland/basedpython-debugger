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
  │            bpd_core                │   sessions, breakpoints, stepping
  │   bpd_sourcemap    bpd_django      │   locations in, locations out
  ├────────────────────────────────────┤
  │            bpd_engine              │   owns targets, drives the transport
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

- a module's code object is a tree. a line inside a comprehension, a lambda, a
    nested function or a class body belongs to a **different** code object than
    the module's. binding walks `co_consts` recursively — anything less silently
    fails to bind breakpoints inside comprehensions
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

stepping is expressed in terms of local events on the code object of the frame
being stepped in, not global instrumentation:

| step | events enabled |
| --- | --- |
| step over | `LINE` on the current code object, `PY_RETURN` and `PY_UNWIND` |
| step in | the above, plus `PY_START` |
| step out | `PY_RETURN` and `PY_UNWIND` on the current code object only |

the events are cleared when the step completes, so the program returns to its
uninstrumented steady state between stops

two traps worth knowing before touching this code:

- `sys.monitoring.restart_events()` re-enables **every** location that was
    disabled, process wide. it is the correct tool when a breakpoint is added to
    an already-running program, and the wrong tool for anything per-frame
- generators and coroutines re-enter the same code object. `PY_RESUME` and
    `PY_YIELD` distinguish a resumption from a fresh call, and a step that
    ignores them steps into the wrong instance of a frame

## the session core, and adapter parity

`bpd_core` holds the session: targets, threads, frames, breakpoints, the stop
state machine, evaluation. it has no idea what DAP or MCP are

`bpd_dap` and `bpd_mcp` translate. neither may contain a capability that the
other does not have, and neither may contain logic — if an adapter needs to
decide something, that decision belongs in the core where both get it

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

every thread hits callbacks independently. the stop coordination — "one thread
hit a breakpoint, now suspend the others" — lives in the agent, in native code,
behind a mutex and a condition variable that do not assume a GIL

free-threaded builds are a target, not a variant to be tested later. anything in
the agent that would only be correct under the GIL is a bug on every build,
because the GIL was never the guarantee it looked like

## the python layer

there is as little of it as possible. where one is genuinely required — an
import hook, a django integration point, anything that must be a python object
to be installed somewhere — it is written in **basedpython** and lives under
`python/`, transpiled as part of the build

it never sits on an event path
