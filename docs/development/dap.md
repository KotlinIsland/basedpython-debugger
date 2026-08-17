# the DAP adapter

DAP is how an editor plugs into a debugger. `bpd dap` is the whole of what
`bpd` says to one, with `Content-Length` framing on stdin and stdout, or on a
loopback socket

which editors that reaches is not uniform, and this page used to say "vs code,
pycharm and neovim all speak it" as though it were. **vs code** and **neovim**
do, and both have driven `bpd`. **pycharm's own python debugging is pydevd**,
and DAP arrives there through a plugin rather than natively — which is what
[the intellij plugin](intellij.md) is, and it drives `bpd` too

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

**one listener, one token, any number of sessions.** a session *is* the
connection — that is DAP's own model, and it is why a second debuggee is a
second session rather than a second field on a request. so a second connection
that presents this listener's token is served beside the first, over the same
`Debuggee`

it used to be turned away with an `output` event saying the adapter was busy and
that "a second session is a second `bpd dap --listen`". that was right while one
listener meant one session and is wrong now: `startDebugging` asks a client to
open exactly this connection, and the adapter would have been refusing the thing
it had just asked for. a token per child was the alternative and is not better —
the connection is asked for by *this* adapter, to a client that has already
presented this token, so a second token would be a second lifetime to get wrong
for a boundary that is already drawn

what the refusal was protecting still holds, and it is now what the tests are
about. anything on this machine can open a socket to a loopback port, and one
that connects and then says nothing must not be able to take a slot or hold up
the session a `startDebugging` asked for — so the wait for a token happens on
the connection's **own** thread, never on the one that accepts. a connection
that presents nothing is dropped on its deadline and the listener carries on

every session the first client opened goes when the first client does. a
debuggee whose original client has gone is a program with nothing watching it,
and the adapter does not outlive it

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
    else, which is where the MVP's line on editor integration lands. a real vs
    code drives a real session through it in `editors/vscode/test/`, and
    [the vs code extension](vscode.md) says what that covers and what it does
    not

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
| `debugChildren`  | `false`   | debug a child the program **forks** — see below        |
| `console`        | see below | where the program runs, and what its streams are       |
| `variables`      | see below | how much of a value to read                            |
| `threadSettleMs` | `50`      | how far apart the two samples a thread census compares |

`bpd` holds **every** program before its first statement — that is how a
breakpoint binds against a real interpreter rather than against a guess about
one. `stopOnEntry` decides whether the client is told about it, not whether it
happens

### `debugChildren`

off by default, and deliberately not debugpy's default of on. a debugged fork
**stops**, at the line that forked, and a setting that produced stopped
processes without being asked for would be a debugger that hangs programs by
default

it is refused up front — at `launch`, before anything has forked — unless two
things are true, because the alternative is discovering them when a child is
already held:

- the client advertised **`supportsStartDebuggingRequest`** in `initialize`.
    that is a *client* capability, not one bpd advertises, and it is one of the
    two this adapter reads — `supportsRunInTerminalRequest` being the other.
    DAP's only way to hand a second program to a client is to ask it to start a
    second session, so a client that cannot be asked would leave the child held
    with nothing able to reach it
- the adapter is **reachable by a second connection**, which means
    `bpd dap --listen`. on stdio the second session `startDebugging` asks for
    would be another `bpd dap` process, with an engine of its own that this
    debuggee is not in

a client that gets the refusal has the honest outcome rather than a hang: take
`debugChildren` out and the fork is reported on the `console` category and the
child runs undebugged, which is what bpd does without it

when a child does join, the adapter sends `startDebugging` with `request:
"attach"` and a configuration written out of the one this connection was
launched with — so the child's session carries the same `stopTheWorld`, the same
`variables` bounds and the same `debugChildren`, which is what makes a fork of a
fork debugged too. two fields are bpd's own, because DAP has none for either:

```json
{
  "bpdSession": 2,
  "bpdConnect": {
    "host": "127.0.0.1",
    "port": 4711,
    "header": "X-Bpd-Token",
    "token": "…"
  }
}
```

the client connects to that endpoint with that header and sends `attach` with
that `bpdSession`. it is **not** PEP 768 attaching and nothing is injected into
anything: the process is already there and already held, and what the second
connection takes up is a session the engine already holds. an `attach` that does
not name `bpdSession` is still refused with the PEP 768 reason

### `console`

where the program runs, which decides what its standard streams **are**:

| value                          | what the program gets                                                                                                 |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------- |
| `internalConsole`, the default | pipes, forwarded as `output` events, and `/dev/null` for stdin. `isatty()` is `False` and `input()` raises `EOFError` |
| `integratedTerminal`           | a real terminal, inside the client                                                                                    |
| `externalTerminal`             | a real terminal, opened outside it                                                                                    |

either terminal is the **`runInTerminal`** reverse request: the client is handed
the argument vector and the environment `bpd` would have spawned, and starts it
in a terminal it owns. the agent then connects back from there exactly as it
does from a process `bpd` started, so it is the last step of a launch that
differs and nothing before it

it is the only way a debuggee under this adapter has a terminal, and the only
way one **reads input**. a debug console is not a terminal — it delivers no
keystrokes, has no size, and shows a cursor escape literally — so putting a
pseudo-terminal in front of one would be `isatty()` claiming something that is
not true. that reasoning, and everything that follows from `bpd` not being the
parent of a process the client started, is in
[launching a debuggee](launching.md#runinterminal-the-client-owns-the-terminal-and-starts-the-program).
in short: the program's output goes to the terminal and **not** to the client as
`output` events, there is no exit code to report, and `disconnect` says so
rather than claiming to have ended the program

it is refused at `launch` unless the client advertised
**`supportsRunInTerminalRequest`** in `initialize` — the second and last client
capability this adapter reads. the refusal names it and says what the program
gets without it, for the reason `debugChildren`'s does: a client that cannot be
asked to start the program would leave the launch waiting for an agent nothing
was going to start

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

### `pending` and `failed` are the core's distinction, not the adapter's

an unverified breakpoint carries DAP's `reason`, and it is the only thing that
tells a client whether to keep hoping: `pending` is "not bound yet, may bind
later", `failed` is "this will not bind". a client is entitled to act on the
second one — to stop waiting, or to mark it differently

which refusals are temporary is a fact about the refusal, so it is asked of
`bpd_core::Unbound::will_bind_later` rather than decided here. the adapter used
to match the one variant it knew about, and that was right for every breakpoint
the source mapping did not touch. a translated one does not arrive in that
shape: `Unbound::InGeneratedPython` says **where bpd looked**, with the ordinary
reason one level down — so a `.by` breakpoint waiting for its module reported
`failed` while the identical `.py` breakpoint in the same session reported
`pending`, and both bound on import a moment later

the message beside it was already right, and said so in words — *"it will bind
if that file is imported later"* — next to a code that said the opposite. that
is the shape of the bug worth remembering: the adapter reproduced a judgement
the core owns instead of asking for it

`Unmappable` inside the same wrapper stays `failed`, because the map could not
place the line at all and nothing arriving later changes that

### a session is the connection

a `bpd_core::Request` may name the session it is for, and a DAP request has no
field for one — because a DAP session *is* the connection. the spec's answer to
a second debuggee is the `startDebugging` reverse request, which has the client
start a whole second session of its own, and a request inside that session needs
no id for the same reason one inside this session does not

so the adapter addresses every request it makes, and the client never writes a
session id: a request that is about a stop goes to the session that stop was
reported from — the `Stop` carries it — and one that is about the program is the
session **this connection** serves. which session that is comes from the launch,
for the first connection, and from the `bpdSession` in the `startDebugging`
configuration for every later one. that is `Facet::Session` in the parity table,
as `Reach::OnItsOwn`, and `crates/bpd_dap/tests/coverage.rs` checks the claim
against what really arrived. see [sessions](sessions.md)

each connection sees **its own** session's stops and no others. another
connection's stops are another program's threads, and a client shown one would
be shown a thread it can neither walk nor resume

#### what a second connection costs

two connections serve two sessions of one `Debuggee`, and the engine is one
object. a wait that blocked in it until the program stopped would hold it for as
long as the program ran, and the other connection could not ask anything —
including the resume a held child is waiting for

so the adapter's wait carries a **deadline** and is sliced. a slice that passes
reports nothing: the client's `continue` was answered before the wait began, so
there is nothing outstanding for a timeout to be the answer to, and the loop
waits again. it changes what is held rather than what is done — the engine
already polls its listener from inside a wait, which is how a session that
arrives while the program runs arrives at all

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

a launch that asked for a terminal is the one thing that changes it, and it
changes it by taking the program's output away from `bpd` altogether: there are
no pipes to read, the client owns the stream, and **no** `output` event carries
a line of the program's. what `bpd` says about the program still does

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
reason below, which is DAP's rather than bpd's. it is acceptable only because
MCP does carry it: a capability neither front end can reach fails that test
outright

it is one of the two entries in the parity test's hand written list of justified
exceptions, and the other one goes the other way. **a terminal for the debuggee
is out of MCP's reach**: `runInTerminal` works by asking a client that owns a
terminal to make one, and an MCP client is an agent that reads the program's
output out of a tool's answer — there is no terminal in that picture, and a
pseudo-terminal the server opened and called one would be `isatty()` answering
`True` about a thing that is not. the list is short and is meant to stay short;
what it forbids is the gap nobody wrote down

one capability is reachable only as its parts. `Request::Run` is a resume
followed by a wait, and DAP needs those separately: a `continue` has to be
answered before the program stops again. the adapter resumes, answers, and then
waits

### and about what the debugger says

the rule is about both directions, and the other one used to rest on somebody
remembering. `bpd_core::Reporting` has no default bodies, so an implementation
of it has to exist — and an empty one satisfies that. an adapter could take a
logpoint's record, or a forked child sitting there **held**, and drop it

`bpd_dap::carriage_of` is the table for it, matching `bpd_core::Told` with no
catch-all arm like the others. everything DAP says is `Carried::Pushed`,
because DAP has an event stream and a client is not obliged to ask anything
again — a fact kept back for a request that never comes is a fact nobody is
told:

| what the debugger says                            | how a DAP client is told                                       |
| ------------------------------------------------- | -------------------------------------------------------------- |
| a logpoint's record                               | an `output` event on `stdout`, with the source and line        |
| a pause armed while the program ran               | an `output` event on `console`, naming the threads             |
| a child the program started                       | an `output` event on `console`, with no `source` and no `line` |
| a way of starting a child this interpreter hides  | an `output` event on `important`                               |
| a debugged fork joining                           | the `startDebugging` reverse request                           |
| a thread stopping                                 | a `stopped` event                                              |
| the program exiting                               | an `exited` event with the code, then `terminated`             |
| the program ending with threads still held        | an `output` event naming them, then the `stopped` events       |
| the program being over with no exit bpd can read  | `terminated` and deliberately **no** `exited`                  |
| a deadline passing with the program still running | nothing — see below                                            |

**one of them reaches no DAP client at all.** DAP answers a `continue` *before*
the program stops again, so a deadline that passes has nothing outstanding to
be the answer to, and there is no event for "still running" — the client was
told the program was running when its `continue` was answered and has had no
`stopped` since. the adapter's own wait carries a deadline only so one
connection cannot block the other sessions of the same debuggee. it is the sole
entry in the parity test's hand written list for this direction, and it is
acceptable only because MCP, whose control tools return the stop they produced,
has to carry it and does

**saying it is reached is not the same as reaching it.** so the fake session in
`crates/bpd_dap/tests/coverage.rs` says one of everything, the conversation is
run to each of the two ways a program can end, and the test reads the
transcript for what would prove each one arrived. an adapter that emptied one of
those methods passes every other test in the file and fails that one

### four capabilities are reached through an extension

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

`Request::ReplaceCode` — [hot code replacement](hot-code-replacement.md) — is
the fourth. DAP's own `restart` is the opposite thing: it throws the process away
and starts another, and the whole point of a replacement is that the process
stays. it names no thread either, because it is about the process:

```json
{
  "command": "bpd/replaceCode",
  "arguments": { "file": "/srv/app/handlers.py" }
}
```

the whole answer comes back in the response body — an editor given only "yes"
cannot show what is now different about the process, and one given only "no"
cannot show which of the user's edits to undo — and each refusal is additionally
written to the `output` stream, in the `important` category, because the person
who has to change something is looking at the debug console rather than at a
response body

nothing advertises any of the four. DAP has no capability flag for a custom
request, and a client that does not know about one never sends it

### and two extra fields, on requests DAP does have

`evenUnderALiveFrame` on `bpd/replaceCode` is one — it trades the guarantee that
the process never runs two versions of one function for a report of every frame
that will, so it is asked for by name and a value that is not a boolean is
refused rather than read as truthy

the other is `after` on a `setBreakpoints` breakpoint —
[a breakpoint that waits for another one](breakpoints.md#a-breakpoint-that-waits-for-another-one):

```json
{
  "command": "setBreakpoints",
  "arguments": {
    "source": { "path": "/srv/app/handlers.py" },
    "breakpoints": [
      { "line": 12 },
      { "line": 88, "after": { "path": "/srv/app/handlers.py", "line": 12 } }
    ]
  }
}
```

**it names a file and a line rather than an id**, and that is not a style choice.
this adapter mints breakpoint ids and re-mints them on every `setBreakpoints` for
that file, so an id a client read off an earlier response has already gone stale.
a file and a line are what the client actually knows, and they are resolved to
whatever id the predecessor holds at the moment the request is built

a predecessor nothing matches is left unset rather than invented, so the
breakpoint is armed immediately — which is what it would have been had the client
not asked. and because a waiting breakpoint really is bound, it comes back
`verified` with the waiting said in its `message`: an editor that showed it as an
ordinary breakpoint would leave somebody at a line the interpreter is not
watching, having been told it was set

## what is not built

- **`attach`**, which is PEP 768 and needs 3.14
- **variable paging**. `start`, `count` and `filter` are refused rather than
    ignored: a value is read with a stated bound on how many children come back,
    and the answer says when the bound bit
- **`restart`**, **function breakpoints**, **data breakpoints**, **goto**,
    **step back**, and **`setExpression`**

the vs code extension is **driven in the editor it is for**. its schema is
pinned to `bpd_dap::Configuration` by a test that fails if either side moves,
and `editors/vscode/test/` downloads a real vs code and starts a real session
through it — a breakpoint hit, the stack and a local read, a resume that ends
with the program's own exit. what that still leaves uncovered is on
[that page](vscode.md), and it is a shorter list than it was
