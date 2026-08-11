# the DAP adapter

DAP is how an editor plugs into a debugger — vs code, pycharm and neovim all
speak it. `bpd dap` is the whole of what `bpd` says to one, with
`Content-Length` framing on stdin and stdout, or on a loopback socket

it is a **translation and nothing else**. a DAP request becomes a
`bpd_core::Request`, the answer is rendered, and no decision about the program
is made on the way. `bpd_dap` depends on `bpd_core` alone; the `bpd` binary is
where `bpd_engine` is put behind it. an adapter that could reach the engine
would be an adapter shaped by how the agent happens to report something, and
how the agent reports something would become what a DAP client sees

## the two transports

DAP defines two, and this speaks both. they end at the same `bpd_dap::serve`: a
transport is where a client's bytes come from, and nothing past that point knows
which one it was

- **stdin and stdout**, with no flag. the adapter is spawned by its client,
    which is what an editor does
- **a loopback socket**, with `--listen`, for a client that did **not** spawn the
    adapter. a script, a tooling integration, and the second session a
    `startDebugging` reverse request has a client start

a transport is not a capability of `bpd_core`, so this adds nothing to the parity
table and nothing to MCP. `bpd_core::Request` is unchanged, `surface()` is
unchanged, and `reach_of` answers the same for every capability whichever socket
the bytes arrived on. "an agent can do everything a human can" is about what can
be **asked**; how the asking arrives is below that line — see
[the parity rule](#the-parity-rule-both-sided)

```sh
bpd dap --listen 0
{"listening":{"header":"x-bpd-token","host":"127.0.0.1","port":51241,"token":"5b1c…"}}
```

one line of json on stdout, before anything is accepted, and then stdout is
never written to again. `--listen` is the one transport where stdout is not the
protocol, which is what makes reporting the port possible at all

`0` binds a port the operating system chooses and the line says which one it
got. that is the point of it: a client that had to pick a number would be racing
every other program on the machine for it, and a client that had to guess when
the adapter was ready would be racing the adapter

one client at a time. a session **is** the connection — that is DAP's own model,
and it is why a second debuggee is a second session rather than a second field on
a request. a second connection is **answered**, with an `output` event saying so
and then a close, rather than queued: a queue is indistinguishable from a hang
at the other end

### why a socket needs a token and stdin does not

a pipe has exactly one writer and whoever spawned the adapter chose it. a
listening socket has whoever gets there first, and **a DAP message runs the
debuggee's own code** — a breakpoint condition is an expression evaluated in the
program. reaching this port is code execution as whoever started `bpd`. so:

- **loopback, and only loopback.** `--listen` takes a *port*. there is no
    address to widen, no flag that would offer one, and no default that something
    else could change: a wildcard bind is not expressible rather than merely
    defaulted away from
- **a token, checked before anything on the connection is acted on.** loopback is
    not a trust boundary. every local user reaches it, and so does every container
    sharing the network namespace. the sharper case is a browser tab: a page can
    `fetch` a `text/plain` POST at `127.0.0.1` with no preflight, and this framing
    is HTTP shaped enough that a request line with a colon in its path parses as
    an ordinary header — so the body after it would be a whole DAP message. what
    the page cannot do is obtain the token, because the same origin policy stops
    it reading anything back

a client presents it as one header line on its first message, alongside the
`Content-Length` it was going to send anyway:

```text
Content-Length: 76
X-Bpd-Token: 5b1c…

{"seq":1,"type":"request","command":"initialize","arguments":{}}
```

it authenticates the *connection*, so it goes on the first message and only the
first. the check is constant time across the token's bytes, and a refusal never
quotes back what was presented — something on loopback is guessing, and telling
it how close it got would be absurd

a connection that fails to authenticate is told why and closed, and **the
listener carries on waiting**. that is deliberate: if a bad connection ended the
adapter, or took the one client slot, then anything that could reach the port
could stop the session someone was starting. a connection that presents nothing
at all is dropped after ten seconds for the same reason. every refusal is
printed on the adapter's stderr, naming the peer — a refusal nobody sees is a
session that mysteriously never starts

what this does **not** defend against is a local process that connects, stalls,
and repeats: authentication is serialised, so a client can be kept waiting ten
seconds at a time. that is a denial of service against a session the person is
watching start, with the reason on stderr each time, and it is not code
execution

### is this `bpd_protocol`'s handshake?

it is the same *problem* — a loopback listener whose peer gets to run code — and
a deliberately different answer, because the peer is different

`bpd_protocol` connects `bpd` to `bpd`. both ends are the same build, so the
handshake can be magic bytes, an exact protocol version and 32 raw bytes, and a
peer that is not this session's agent is refused before it sends a frame. here
the peer is a **third party** that speaks DAP and nothing else, so the token
rides in the framing the transport already has and a client adds one header line
rather than learning a second protocol. the sizes match on purpose: 32 bytes of
the operating system's randomness, hex encoded

the two tokens are separate values with separate lifetimes. this one
authenticates a client to the adapter; the agent's authenticates the debuggee to
the engine. one shared between them would make a DAP client that has this token
able to open the debuggee's control plane directly

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

| field            | default   | what it does                                           |
| ---------------- | --------- | ------------------------------------------------------ |
| `program`        | required  | the script to run                                      |
| `args`           | `[]`      | arguments for the program, exactly as it receives them |
| `python`         | `python3` | the interpreter, resolved on `PATH` like any command   |
| `stopOnEntry`    | `false`   | stay stopped before the first statement                |
| `stopTheWorld`   | `false`   | hold every thread that can be held, for each stop      |
| `variables`      | see below | how much of a value to read                            |
| `threadSettleMs` | `50`      | how far apart the two samples a thread census compares |

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

### a session is the connection

a `bpd_core::Request` may name the session it is for, and a DAP request has no
field for one — because a DAP session *is* the connection. the spec's answer to
a second debuggee is the `startDebugging` reverse request, which has the client
start a whole second session of its own, and a request inside that session needs
no id for the same reason one inside this session does not

so the adapter addresses every request it makes, and the client never writes a
session id: a request that is about a stop goes to the session that stop was
reported from — the `Stop` carries it — and one that is about the program names
none, which the engine answers with the only session there is. that is
`Facet::Session` in the parity table, as `Reach::OnItsOwn`, and
`crates/bpd_dap/tests/coverage.rs` checks the claim against what really arrived.
see [sessions](sessions.md)

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
`supportsSingleThreadExecutionRequests`, `supportsExceptionInfoRequest`,
`supportsGotoTargetsRequest`, `supportsRestartFrame`, and the
`raised` / `uncaught` exception filters

`gotoTargets` and `goto` are [set next statement](jumps.md), and `restartFrame`
is restart frame. a target is minted only for a location in the file a held
thread is **executing**, and the `stopped` event that follows either of them
carries reason `goto` or `restart` — the thread was never resumed, and the client
has to re-read the stack to see where it is

not advertised, and why:

| capability                                                                                                                      | why not                                                                                                                                                                  |
| ------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `supportsHitConditionalBreakpoints`                                                                                             | the string has no agreed meaning — above                                                                                                                                 |
| `supportsEvaluateForHovers`                                                                                                     | an evaluation runs the program's own code, and running it because a mouse passed over an identifier is the debugger changing the program by accident                     |
| `supportsDelayedStackTraceLoading`                                                                                              | `Request::Stack` bounds a walk from the top and cannot start part way down. a client paging from the middle would be answered from a walk that started at the top anyway |
| `supportsTerminateRequest`                                                                                                      | there is no *graceful* end. `disconnect` ends the debuggee, and a second request that did the same thing under a name promising more would be a promise nothing keeps    |
| `supportsSetExpression`, `supportsFunctionBreakpoints`, `supportsDataBreakpoints`, `supportsStepBack`, `supportsRestartRequest` | no capability behind them exists                                                                                                                                         |

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
and `stderr` categorised separately. that is unconditional rather than a thing
`--listen` relaxes — under it stdout carries the line saying where the adapter
bound, and a `print` landing in the middle of *that* is a client that cannot
find the port

what `bpd` itself has to say goes on the **`console`** category instead, and the
one thing that currently does is a python child the program started. `console`
and not `stdout`, because the program did not write it and a client that showed
it among the program's own output would be putting words in the debuggee's
mouth. it carries no `source` and no `line` either — see
[child processes](subprocesses.md)

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

### three capabilities are reached through an extension

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

`Request::Query` and `Request::Diff` — [the state query](queries.md) — are the
other two, for the same reason. DAP's own way of reading state is the tree walk
above and it keeps it; these are the whole of a stop in one request, and the
difference between two of those answers:

```json
{ "command": "bpd/state",
  "arguments": { "threadId": 1,
                 "query": { "frames": 2, "scopes": ["local"], "source": 2 } } }
{ "command": "bpd/diff", "arguments": { "before": "2:719f…", "after": "3:bf52…" } }
```

`bpd/diff` names no thread and touches no program: both states were read when
they were read. "what changed between these two stops" is a thing a person wants
as much as an agent, and no editor offers it — which is the parity rule earning
its keep rather than being satisfied

nothing advertises any of the three. DAP has no capability flag for a custom
request, and a client that does not know about one never sends it

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
