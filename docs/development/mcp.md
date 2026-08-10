# the MCP adapter

MCP is how an ai agent plugs into a debugger, and `bpd mcp` is the whole of what
`bpd` says to one — JSON-RPC 2.0, one message per line, on stdin and stdout

it is a **translation and nothing else**, the same as
[the DAP adapter](dap.md): a tool call becomes a `bpd_core::Request`, the answer
is rendered, and no decision about the program is made on the way. `bpd_mcp`
depends on `bpd_core` alone; `crates/bpd/src/mcp.rs` is where `bpd_engine` is
put behind it

why MCP rather than DAP or LSP, and what an agent-shaped interface owes an
agent, is [the interface for ai agents](agent-interface.md). this page is what
was built

## setting it up

`bpd mcp` is a stdio MCP server, so a host configures it the way it configures
any other:

```json
{
  "mcpServers": {
    "bpd": { "command": "bpd", "args": ["mcp"] }
  }
}
```

there are no flags. everything a session needs arrives in the `launch` tool
call, and a flag here would be a second place to configure the same thing

the protocol revision is `2025-06-18`. a client that asks for `2025-03-26` or
`2024-11-05` is answered in its own, because nothing this server speaks differs
between them; anything else is answered with `2025-06-18`, which is the client's
cue to decide whether it can go on. a JSON-RPC **batch** is refused with the
reason — MCP removed batching in the revision above

only the `tools` capability is declared. resources are pulled at the host's
discretion and prompts are invoked by the user, so neither is a surface an agent
can be relied on to see — see [what is not built](#what-is-not-built)

## every control tool returns the stop it produced

this is the whole difference from DAP, and it is one thing: in DAP the answer
arrives as an **event**. `next` returns an acknowledgement, and where it stopped
arrives later on a stream the client has to correlate — so every step is a state
machine

here `continue_`, `step_over`, `step_in`, `step_out`, `wait` and `pause` each
block until the program stops again and **return that stop**: why it stopped,
which thread, and the top of its stack. one call, one answer. the server writes
nothing that is not the answer to something the client asked, which
`crates/bpd_mcp/tests/coverage.rs` asserts message by message

stepping five times therefore costs five tool calls and nothing else, and
`crates/bpd/tests/mcp.rs` counts them against a real interpreter

## deadlines, and what a timeout may say

every control tool **requires** a `deadline_ms`. there is no default, because a
default would be bpd deciding how long an agent waits, and a control tool
without one is a call that never returns on a program that never stops

when the deadline passes the answer is `outcome: "timed_out"`, with how long was
really waited. it is not a stop, and it carries no location at all — no thread,
no frames, no reason

### there is no sample, and that is a limit rather than a choice

the [agent interface](agent-interface.md) design says a timeout may carry a
**sample**: a stack read off a running thread, labelled as already stale. that
is not available, and the reason is the architecture rather than an omission

everything the agent inside the debuggee answers, it answers **on a thread it is
holding**. that includes the thread census: `Request::Threads` is routed to the
lowest-numbered held stop, and with nothing held it is refused. so a program
that is running cannot be asked what its threads are doing, let alone where one
of them is

what a timeout says instead is what is true — the program is still running, and
here is what to do about it:

- **`wait`** carries on waiting and touches nothing. it is the only tool that
  perturbs the program in no way at all
- **`pause`** arms a line event for the whole program and holds the first thread
  that reaches one. that is a real stop, and everything is askable again

a sampled stack presented as a stopped one is the debugger reporting a state the
program was not in. a timeout that says nothing about a location is the honest
shape of what bpd can see from here

## the tools

| tool | what it is |
| --- | --- |
| `launch` | start a program and hold it before its first statement |
| `set_breakpoints` | replace the whole breakpoint set |
| `set_exception_breakpoints` | stop where an exception is raised, or leaves the program |
| `continue_` | let every held thread go, and return the next stop |
| `step_over`, `step_in`, `step_out` | step one thread, and return where it landed |
| `wait` | wait without touching the program |
| `pause` | hold the next thread that reaches a line |
| `resume` | let held threads go without waiting |
| `stack` | one held thread's frame chain |
| `variables` | one scope of one frame |
| `evaluate` | a python expression, in a frame |
| `set_variable` | write a name of a frame's scope |
| `threads` | what every thread is doing, as a sample |
| `stop_the_world` | hold every thread that can be held |
| `run_script` | run a whole investigation, and return what happened at every step |
| `terminate` | end the debuggee |

### the schema of a script is its documentation

`run_script` is the one tool whose input is a **tree**, and it is why the steps
are data rather than text. an MCP tool takes JSON Schema input already, so the
step definition goes across as a `$ref` to itself and the agent needs no parser,
no grammar and no syntax errors — and what it reads before writing one is the
schema. the step vocabulary, the transcript and the budget are
[the debug script](scripts.md)

### there are no handles

DAP hands a client an opaque `variablesReference` that looks the same before and
after a resume, so a stale one gets answered. there is nothing like it here: a
tool names a **stop number** and a **frame depth**, both of which are in the
answer that produced them, and `bpd_core::FrameId` carries the stop it belongs
to — so a frame from a stop that has ended is refused by name rather than
resolved against whatever is at that depth now

a tool that is about one held thread may leave `stop` out when exactly one is
held. when several are, it is refused and the refusal lists them. that rule is
`bpd_core::only_stop`, in the core, because every front end has to apply it and
two of them applying their own would make the same call mean two things

### a breakpoint id is its position in the set

`set_breakpoints` replaces **every** breakpoint, so an id can be the position in
the list the client sent, counting from one. there is no table behind it

that is the one place this adapter is simpler than DAP rather than the same.
DAP replaces one *file's* breakpoints at a time, so `bpd_dap` has to keep what
each file last asked for and send the union — bookkeeping that exists only
because of DAP's shape. `Request::SetBreakpoints` is already the whole set, so
nothing here reassembles anything, and there is nothing to move into the core

### a hit condition goes across typed

`bpd_core::HitCondition` says which of a breakpoint's qualifying hits to act on,
and it is deliberately not DAP's `hitCondition` **string** — whose meaning is a
per-client convention that different debuggers read differently. an MCP tool
takes JSON Schema input, so it goes across as itself:

```json
{ "file": "app.py", "line": 40, "hits": { "hits": "every", "count": 3 } }
```

this is the one capability of the core that DAP has no route to at all, and it
is the only entry in the parity test's list of justified exceptions

### no argument is accepted that is not named

every schema sets `additionalProperties: false`, and every struct the server
parses into sets `deny_unknown_fields` to match. a misspelled `deadlineMs` is
refused by name rather than taking a default that does not exist, and a
misspelled `dept` inside `detail` is refused rather than leaving the answer cut
in the same place with the same advice

### an answer is json, and it says what it left out

each tool result is one text block holding pretty-printed json. the machine
readable part is the core type's own serde, so nothing is dropped on the way;
the sentences beside it are the core type's own wording, so there is one
phrasing rather than one per front end

a value is carried as `{"kind": ..., "content": {...}}`, and an `int` is carried
as **text** inside it, because a python `int` has no width and a json number
that silently became a float would be a different value

every bound that bit is named where it bit — `left_out` on a scope read,
`frames_omitted` on a bounded stack walk — each saying how much there was and
which field to raise

### the program's own output comes back on the answer

the server's stdout **is** the protocol, so the debuggee's cannot be. the
program is launched with pipes and what it wrote is carried on the next tool
answer, under `output`, tagged with the stream each line came from

it is bounded: the most recent 64 KiB, because what a program printed just
before it stopped is what the stop is about, and whatever fell off the front is
counted and reported. logpoint records are bounded the same way — the first 200,
with the number that were dropped — because there is no bound on how many a
logpoint on a hot line produces

## what a failure looks like

two shapes, and they are different on purpose:

- a **tool** that could not do what was asked is a successful call whose content
  is the reason, with `isError: true`. that is what an agent reads, and a
  refusal here names a cause and an action in the same words `bpd` uses
  everywhere
- a **protocol** failure — a method that does not exist, arguments that are not
  the shape the schema says — is a JSON-RPC error. a client is entitled to hide
  one of those from the model, which is exactly why nothing about the program
  goes down that channel

an expression that raises is **neither**. it is a result carrying the exception,
because the interpreter is the authority on what an expression is

## the parity rule, now both sided

no capability exists in one adapter and not the other, and the test of it is
`crates/bpd/tests/parity.rs` — the item the `Request`/`Response` refactor
existed to make writable

it bites in two places:

- **at compile time.** `bpd_dap::reach_of` and `bpd_mcp::reach_of` match
  `bpd_core::Request` with no catch-all arm, so a capability added to the core
  does not compile in either adapter until someone says how that front end gets
  at it
- **at test time.** `bpd_core::parity` also enumerates the capabilities carried
  *inside* a request — a `Facet` — because a front end can implement every
  variant and still not offer a hit condition. the test compares what the two
  adapters claim, requires that everything is reachable from at least one of
  them, and requires that any "cannot" is in a hand written list with the reason
  beside it

the list has one entry: DAP and the hit condition. adding a second means editing
the test, which is the point — a gap that appears quietly is how two front ends
drift apart

## what is not built

- **`run_to` as a *tool***, which is the shape it cannot take. it is either a
  composition the adapter performs — arming a breakpoint of its own and taking
  it off again, which is a decision about the program made in a front end — or a
  capability of the core that DAP has **no** request for, since a DAP client
  performs run-to-cursor itself by setting a temporary breakpoint. it also has
  an unsound failure mode under a deadline: a one-shot breakpoint cannot be
  taken off while the program is running, so a timed-out `run_to` would leave
  the program armed with a breakpoint the agent did not ask for. it is built as
  a **step of `run_script`**, where the engine owns the whole composition
  including the removal — that reasoning, and what became of the failure mode,
  is [the debug script](scripts.md#run_to-lives-here-and-nowhere-else)
- **the declarative state query** and **snapshot and diff**. each is its own
  item, and each is a capability rather than an MCP feature — so each arrives in
  both adapters
- **resources and prompts**. only tools are model-controlled, so they are what
  has to explain the interface first. writing a document to explain an interface
  that does not explain itself is how the tool that needed fixing stays broken
- **attach**, which is PEP 768 and needs cpython 3.14. `launch` is the only way
  in, and a tool that needs a program says so by name
