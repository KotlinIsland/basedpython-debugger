# the DAP adapter

DAP is how an editor plugs into a debugger — vs code, pycharm and neovim all
speak it. `bpd dap` is the whole of what `bpd` says to one, over stdin and
stdout with `Content-Length` framing

it is a **translation and nothing else**. a DAP request becomes a
`bpd_core::Request`, the answer is rendered, and no decision about the program
is made on the way. `bpd_dap` depends on `bpd_core` alone; the `bpd` binary is
where `bpd_engine` is put behind it. an adapter that could reach the engine
would be an adapter shaped by how the agent happens to report something, and
how the agent reports something would become what a DAP client sees

## setting it up in an editor

`bpd dap` speaks the protocol on stdin and stdout, so it is a
[debug adapter executable][adapter]. what an editor needs beyond that differs,
and the difference is worth being blunt about:

- **neovim, through `nvim-dap`** — works today. the adapter is named in the
  configuration itself, so nothing else has to be installed
- **vs code** — needs an extension, because it resolves a configuration's
  `"type"` through one and offers no way to name an adapter executable from
  `launch.json` alone. `editors/vscode/` is that extension and it contributes
  the type, the launch attributes and the lookup for the binary — and nothing
  else, which is where the MVP's line on editor integration lands. it has
  **not** been driven by hand in vs code, and
  [the vs code extension](vscode.md) says exactly what that leaves unverified

`nvim-dap`, which is the one that works:

```lua
local dap = require("dap")

dap.adapters.bpd = {
  type = "executable",
  command = "bpd",
  args = { "dap" },
}

dap.configurations.python = {
  {
    type = "bpd",
    request = "launch",
    name = "debug this file with bpd",
    program = "${file}",
    python = "python3.14",
    stopOnEntry = false,
  },
}
```

set a breakpoint on a line, start the configuration, and the program stops
there. the frame's locals are in the scopes pane, writing one writes it, and
step over / step in / step out are the ordinary `nvim-dap` commands

the same configuration body is what a vs code `launch.json` entry holds:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "bpd",
      "request": "launch",
      "name": "debug this file with bpd",
      "program": "${file}",
      "python": "python3.14",
      "stopOnEntry": false
    }
  ]
}
```

[adapter]: https://microsoft.github.io/debug-adapter-protocol/overview

## what a configuration can say

every field here is read. one that were parsed and ignored would be a setting a
user could see accepted and never get

| field | default | what it does |
| --- | --- | --- |
| `program` | required | the script to run |
| `args` | `[]` | arguments for the program, exactly as it receives them |
| `python` | `python3` | the interpreter, resolved on `PATH` like any command |
| `stopOnEntry` | `false` | stay stopped before the first statement |
| `stopTheWorld` | `false` | hold every thread that can be held, for each stop |
| `variables` | see below | how much of a value to read |
| `threadSettleMs` | `50` | how far apart the two samples a thread census compares |

`bpd` holds **every** program before its first statement — that is how a
breakpoint binds against a real interpreter rather than against a guess about
one. `stopOnEntry` decides whether the client is told about it, not whether it
happens

### `variables`

DAP has nowhere to carry the bounds on a value read, and they are the difference
between an object graph that opens and one that reports it ran out of budget.
every one of them is a field of `bpd_core::Detail`:

```json
"variables": {
  "depth": 3,
  "children": 100,
  "text": 1024,
  "budget": 8192,
  "attributes": true,
  "repr": false
}
```

- `budget`, `children` and `text` are the ones that bite. when one does, the
  value's own line in the variables pane says which, how much was there, and
  which of these to raise. that message is only useful to someone who has
  somewhere to raise it, which is what this exists for
- `repr` is **off** by default because `__repr__` is arbitrary user code. bpd
  cannot interrupt it once it has started, so a `__repr__` that hangs hangs the
  debuggee
- `depth` applies where a value is read once and kept — an evaluated
  expression, and the value a write left behind. the variables *tree* is a
  different thing: opening a node re-reads its scope one level deeper, so how
  far it goes is however far the client has opened it

## where DAP's model and bpd's differ

none of these is papered over, because papering over one is how a debugger
reports something that is not true

### a stop holds one thread

DAP defaults to whole-program stops. bpd's default is that a stop holds one
thread and everything else keeps running, so the adapter reports
`supportsSingleThreadExecutionRequests` and sends `allThreadsStopped: false`. a
client that did not know would render a stop that never happened, and label
every read with a mode it was not taken in

with `stopTheWorld` set, every stop asks the agent to hold the rest of the
program, and `allThreadsStopped` is true — **only when it really was**. a thread
parked in a C call has released the GIL and reaches no monitoring event, so
nothing can hold it; when there is one, the client gets an `output` event naming
it and `allThreadsStopped` stays false. see [threads](threads.md)

### a reference is not a frame id

DAP hands a client an opaque number and expects it back later, and that number
looks the same before and after a resume — so a stale one gets answered.
`bpd_core::FrameId` carries the stop it was minted at, and the adapter keeps
that: the entries of a stop are forgotten when its thread is resumed, and asking
again is refused with "ask for the stack again" rather than resolved against
whatever is at that index now

### a hit condition is a string with no agreed meaning

DAP carries one as free text, and `>5`, `=5`, `%5` and a bare `5` are read
differently by different debuggers. `bpd_core::HitCondition` is deliberately not
that string. so `supportsHitConditionalBreakpoints` is **not** advertised, and a
client that sends one anyway is refused with the reason — a debugger that
guessed which convention was meant is a debugger that stops on the wrong pass

### writing is a name of a frame's scope

`setVariable` works on a scope. asking to write something *inside* a value is
refused: that means running the program's own `__setattr__` or `__setitem__`,
which is the program rather than the debugger

### a mapping is not a set of names

a dict key is an object. a **whole** string key is a name and is used as one;
anything else — a number, a tuple, a string the read had to cut — gets a
positional name and its key is listed beside it, so nothing about the entry goes
unsaid

## what is advertised, and what is not

only capabilities that are implemented. a capability reported and not
implemented is a placeholder with a wire format: the client offers the feature
and the user gets an error at the moment they need it

advertised: `supportsConfigurationDoneRequest`,
`supportsConditionalBreakpoints`, `supportsLogPoints`, `supportsSetVariable`,
`supportsSingleThreadExecutionRequests`, `supportsExceptionInfoRequest`, and the
`raised` / `uncaught` exception filters

not advertised, and why:

| capability | why not |
| --- | --- |
| `supportsHitConditionalBreakpoints` | the string has no agreed meaning — above |
| `supportsEvaluateForHovers` | an evaluation runs the program's own code, and running it because a mouse passed over an identifier is the debugger changing the program by accident |
| `supportsDelayedStackTraceLoading` | `Request::Stack` bounds a walk from the top and cannot start part way down. a client paging from the middle would be answered from a walk that started at the top anyway |
| `supportsTerminateRequest` | there is no *graceful* end. `disconnect` ends the debuggee, and a second request that did the same thing under a name promising more would be a promise nothing keeps |
| `supportsSetExpression`, `supportsFunctionBreakpoints`, `supportsDataBreakpoints`, `supportsStepBack`, `supportsRestartRequest` | no capability behind them exists |

`attach` is refused by name: attaching is PEP 768, it needs cpython 3.14, and
`bpd` refuses rather than injecting by another route

a client that asks for 0-based lines or columns, or for sources as URIs, is
refused too. bpd reports the line numbers cpython reports, and renumbering them
is not something a debugger may get wrong quietly

## how the session runs

everything the agent answers, it answers on a thread it is **holding**. that one
fact shapes the whole adapter:

- the main thread owns the session. when nothing is held it is blocked waiting
  for the program, which is where a `stopped` event comes from
- a reader thread owns the client's input and an interrupt handle. `pause`,
  `disconnect` and `terminate` are the three things a *running* program can be
  asked, and the reader answers them without going through the session
- everything else that arrives while the program is running is queued and
  answered when it next stops. that is the model rather than a shortcut: the
  agent cannot bind a breakpoint or read a frame without a python thread to do
  it on. **to change breakpoints in a program that is running, pause it first**

a second thread stopping while a first is held arrives on the connection rather
than as the answer to anything, so the adapter compares what the session is
holding against what it has told the client after every request — and a stop
nobody asked about gets its own `stopped` event

### the program's own output

the adapter's stdout **is** the protocol, so the debuggee cannot share it: one
`print` in the middle of a message and every message after it is unreadable. the
program is launched with pipes and each line becomes an `output` event, `stdout`
and `stderr` categorised separately

## the parity rule, both sided

no capability may exist in one adapter and not the other, and the test of that
arrived with [the MCP adapter](mcp.md): `crates/bpd/tests/parity.rs`

what bites here is the half about DAP. `bpd_dap::reach_of` matches
`bpd_core::Request` with **no catch-all arm**, so a capability added to the core
does not compile until someone says how DAP gets at it, and
`bpd_dap::reach_of_facet` does the same for the capabilities carried *inside* a
request — a front end can implement every variant and still not offer a hit
condition. `crates/bpd_dap/tests/coverage.rs` then drives the adapter through a
whole DAP conversation and asserts that every capability the table claims is
reachable was really asked for, so an entry that reads well and is not true
fails

**one capability is out of DAP's reach**, and it is the hit condition — for the
reason below, which is DAP's rather than bpd's. it is the only entry in the
parity test's hand written list of justified exceptions, and it is only
acceptable there because MCP does carry it. a capability neither front end can
reach fails that test outright

one capability is reachable only as its parts. `Request::Run` is a resume
followed by a wait, and DAP needs those separately: a `continue` has to be
answered before the program stops again. the adapter resumes, answers, and then
waits

### one capability is reached through an extension

`Request::RunScript` — [the debug script](scripts.md) — is a capability of the
core, so the parity rule does not let it be an agent's alone. DAP has no request
of its own for a whole investigation and will not grow one, so it is a **custom
request**, which the protocol provides for and which every DAP client can send:

```json
{ "command": "bpd/runScript",
  "arguments": { "threadId": 1, "steps": [ … ], "budget": { … } } }
```

it takes a `threadId` like every other DAP request that is about one thread, and
the whole transcript comes back in the response body. that is the answer rather
than the final state, for the same reason it is for an agent: a client given only
where a script ended cannot tell why. a script that leaves a thread held at a new
stop produces a `stopped` event for it, by the same reconciliation every other
request goes through

nothing advertises it. DAP has no capability flag for a custom request, and a
client that does not know about it never sends one

## what is not built

- **`attach`**, which is PEP 768 and needs 3.14
- **variable paging**. `start`, `count` and `filter` are refused rather than
  ignored: a value is read with a stated bound on how many children come back,
  and the answer says when the bound bit
- **`restart`**, **function breakpoints**, **data breakpoints**, **goto**,
  **step back**, and **`setExpression`**

what is built but **unverified in the editor it is for**: the vs code extension.
its schema is pinned to `bpd_dap::Configuration` by a test that fails if either
side moves, and nobody has yet installed it and started a session — which is a
different sentence from "it works", and [that page](vscode.md) is the one that
says so
