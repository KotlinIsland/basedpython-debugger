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

`tools`, `resources` and `prompts` are declared, and each of the three is
declared because it is implemented. what is *load bearing* is only in the tools
and the errors, because those are the only surfaces an agent is certain to see —
[what a resource may say](#resources-carry-the-model-a-schema-cannot-hold) is
the rule that keeps it that way

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

| tool                               | what it is                                                        |
| ---------------------------------- | ----------------------------------------------------------------- |
| `launch`                           | start a program and hold it before its first statement            |
| `set_breakpoints`                  | replace the whole breakpoint set                                  |
| `set_exception_breakpoints`        | stop where an exception is raised, or leaves the program          |
| `continue_`                        | let every held thread go, and return the next stop                |
| `step_over`, `step_in`, `step_out` | step one thread, and return where it landed                       |
| `wait`                             | wait without touching the program                                 |
| `pause`                            | hold the next thread that reaches a line                          |
| `resume`                           | let held threads go without waiting                               |
| `stack`                            | one held thread's frame chain                                     |
| `variables`                        | one scope of one frame                                            |
| `evaluate`                         | a python expression, in a frame                                   |
| `set_variable`                     | write a name of a frame's scope                                   |
| `set_next_statement`               | move the held frame to another line of the code it is running     |
| `restart_frame`                    | re-enter the held frame from the top                              |
| `replace_code`                     | make the running process run the code a file holds on disk        |
| `threads`                          | what every thread is doing, as a sample                           |
| `stop_the_world`                   | hold every thread that can be held                                |
| `state`                            | describe a whole stop in one call, and keep the answer            |
| `diff`                             | what changed between two of those answers                         |
| `run_script`                       | run a whole investigation, and return what happened at every step |
| `terminate`                        | end the debuggee                                                  |

### the schema of a script is its documentation

`run_script` is the one tool whose input is a **tree**, and it is why the steps
are data rather than text. an MCP tool takes JSON Schema input already, so the
step definition goes across as a `$ref` to itself and the agent needs no parser,
no grammar and no syntax errors — and what it reads before writing one is the
schema. the step vocabulary, the transcript and the budget are
[the debug script](scripts.md)

### `state` and `diff` are the same capability at two scopes

`stack`, `variables` and `evaluate` are still here and still answer one question
each — an editor's shape, and sometimes the right one. `state` asks all of them
at once and keeps the answer under an id; `diff` says what changed between two of
those ids, rather than handing back both. what an id claims, what a diff refuses
to claim, and why source is only ever shown when it can be proved is
[the state query](queries.md)

### there are no handles

DAP hands a client an opaque `variablesReference` that looks the same before and
after a resume, so a stale one gets answered. there is nothing like it here: a
tool names a **stop number** and a **frame depth**, both of which are in the
answer that produced them, and `bpd_core::FrameId` carries the stop it belongs
to — so a frame from a stop that has ended is refused by name rather than
resolved against whatever is at that depth now

a snapshot id is the one thing here that outlives a stop, and it is not a handle
either: it names a reading that has already been taken rather than a promise to
take one, so nothing the program does can change what it resolves to — see
[a snapshot is a value](queries.md#a-snapshot-is-a-value-and-does-not-go-stale)

a tool that is about one held thread may leave `stop` out when exactly one is
held. when several are, it is refused and the refusal lists them. that rule is
`bpd_core::only_stop`, in the core, because every front end has to apply it and
two of them applying their own would make the same call mean two things

### no tool takes a session, and every request names one

a stop number is unique in the process that minted it and no further, so a
`bpd_core::Request` may name the **session** it is for. no tool here takes one
as an argument, because this server holds one session — an argument for it would
be an agent naming the only thing there is, and the tool that would make it
meaningful is one that lists them, which arrives with the second session

what the server does instead is address every request it makes: one that is
about a stop goes to the session that stop was reported from, which the `Stop`
carries, and one that is about the program names none and is answered by the
only session open. with two open, a request naming none is **refused** rather
than answered from whichever came first — `bpd_core::only_session`, which is
`only_stop`'s rule one level up. see [sessions](sessions.md)

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

**and no argument is parsed that the schema does not name**, which is the other
half and the half that had drifted: `launch` read a `frames` its schema never
declared, so the number of frames the entry stop comes back with was a setting
no agent could find and a strict client would have had rejected. a schema and a
struct are two descriptions of one thing and nothing makes them agree, so
`every_tools_schema_names_exactly_the_arguments_it_parses` compares them for
every tool — with the field list **asked of serde** rather than written down,
the way `crates/bpd_dap/tests/vscode.rs` does it for the vs code manifest

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

pipes and not a pseudo-terminal, deliberately. what reads the program's output
here is an agent rather than a terminal, so `python program.py | agent` is the
bare run this is the same as — and a program told `isatty()` is `True` would
write colour escapes and progress redraws into text an agent then has to read
around. the reasoning, and what it costs in buffering, is in
[launching a debuggee](launching.md#the-debuggees-own-standard-streams)

it is bounded: the most recent 64 KiB, because what a program printed just
before it stopped is what the stop is about, and whatever fell off the front is
counted and reported. logpoint records are bounded the same way — the first 200,
with the number that were dropped — because there is no bound on how many a
logpoint on a hot line produces

a python **child** the program started rides on the same answer, under
`spawned`, and is bounded at fifty for the same reason a logpoint is. it is its
own key rather than part of `logged`, because an agent that found it there would
reasonably read it as a logpoint having fired. every entry carries
`debugged: false`, since an agent that assumed otherwise would set breakpoints in
the child and wait for stops that never come — see
[child processes](subprocesses.md)

## what a failure looks like

two shapes, and they are different on purpose:

- a **tool** that could not do what was asked is a successful call whose content
    is the reason, with `isError: true`. that is what an agent reads, and a
    refusal here names a cause and an action in the same words `bpd` uses
    everywhere
- a **protocol** failure — a method that does not exist, a `tools/call` with no
    `name`, a tool nobody offers, a resource uri nobody serves — is a JSON-RPC
    error. a client is entitled to hide one of those from the model, which is
    exactly why nothing about the program goes down that channel

**arguments that are not the shape the schema says are the first of those, not
the second.** they read like a protocol failure and they are not one: a
misspelled `deadline_ms` is the *model's* mistake and the model is the one that
has to correct it, so it comes back as a tool failure the model is certain to
see. the refusal names the tool as well as the argument, since an agent that
made several calls in a turn is otherwise told which argument was wrong without
being told which call it belonged to

an expression that raises is **neither**. it is a result carrying the exception,
because the interpreter is the authority on what an expression is

### two errors that used to be one

nothing held has two causes and they need opposite things done about them, so
they are two refusals rather than one:

- the program is **running**, and has to be held before anything can be asked.
    the refusal says so, and names the two ways: run it to a breakpoint, or pause
    it
- the program has **exited**, and there is nothing left to hold. the refusal
    names the exit code

`bpd_core::only_stop` cannot tell them apart from the held stops alone, so it
takes the exit as an argument and every caller supplies it — the engine from the
child process, this adapter through `Session::ended`. a client told only
"nothing is held" about a program that has ended goes on pausing a process that
is not there

## resources carry the model a schema cannot hold

only tools are model-controlled. a resource is read at the **host application's**
discretion, so the rule for one is a rule about what may go in it: **nothing
here may be the only place something is said.** an agent that never receives a
resource still has to be able to use `bpd` correctly from the tool schemas and
the errors, and if the answer to "the agent got this wrong" is a paragraph in a
resource, the tool or the error was the thing that needed fixing

what is left after that is real, and it is the part neither can carry: not what
a call takes, but what its answer **claims**, and where the claim stops

| uri                  | what it is                                                                                                                                                                                                                               |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bpd://model/stops`  | a stop holds one thread and not the program; the frame chain is a snapshot and every value through it is a sample; a timeout carries no location at all and why; what a stop number, a frame depth and a snapshot id each stay valid for |
| `bpd://model/values` | why the four scopes are never merged; why an `int` arrives as text; what every bound that bit is called where it bit; why source is proved in the debuggee rather than read from disk; what a diff refuses to call unchanged             |

two, and no more, because a third would be padding. a uri nobody serves is a
JSON-RPC error under MCP's own `-32002` rather than a page saying so — a page of
prose in place of the page that was asked for is a thing an agent could read as
true

## prompts are canonical investigations

a prompt is invoked by the **user**, usually as a slash command. so the bar for
one is narrower than "a thing you can do with `bpd`": it has to be an
investigation a competent agent would otherwise get **wrong** or do the long
way. a prompt that restates a tool name is a slash command that costs a
keystroke and teaches nothing

| prompt             | what it would otherwise cost                                                                                                      |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------- |
| `nth_call`         | a counter written into the program, or n resumes counted by hand, rather than a typed hit condition the debuggee evaluates itself |
| `step_until`       | a step and an evaluate per line of the program, rather than one submitted script whose transcript is the answer                   |
| `what_changed`     | both states shipped to the agent and compared in its head, which spends the context twice and calls a truncated value unchanged   |
| `why_wont_it_stop` | resuming again with a larger deadline, having read a timeout as a location                                                        |

each carries the whole investigation with its arguments substituted. a required
argument that was not given is refused naming it, because an investigation with
a hole in it is a workflow that says to set a breakpoint and does not say where

## a skill directory, for clients that have one

`skills/bpd/SKILL.md` is in the repository. a skill is a **client** feature and
no part of MCP — a client that has them reads it, and one that does not is not
missing anything load bearing, for the same reason a resource is not load
bearing

it is at the top level rather than under `.claude/`, because `.claude/` is not
committed. a client that reads skills from a project directory wants it copied
or symlinked into place — for claude code that is `.claude/skills/bpd/` in a
project, or `~/.claude/skills/bpd/` for every project

what it carries that the tools do not: when a debugger is the right tool at all,
how the server is configured, the order of a session, and the handful of things
an agent gets wrong. `crates/bpd/tests/skill.rs` checks it against the tool
table, the resource uris and the prompt names, because nothing else parses it

## nothing a resource, a prompt or the skill names has gone away

prose loses to renames. a page that names `run_to` after `run_to` stopped being
a tool reads exactly as well as one that is true, and an agent will act on it —
which is worse than having no page

so every tool a resource or a prompt names is **declared beside it**, and
`crates/bpd_mcp/tests/teaching.rs` checks the declaration in both directions: a
declared name has to be a tool this server offers, and a tool named in the text
has to be declared. every argument a prompt declares has to really reach its
text, and filling one in with everything given has to leave no placeholder
behind — a parameter accepted and ignored is the placeholder ban applied to a
workflow

what none of that catches is a name that never existed anywhere, which is a typo
rather than a drift. the same limit is written beside the parity test's hand kept
list, for the same reason

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

### and about what the debugger says

a `Request` and a `Facet` are what an agent **asks for**. what the debugger
**says** is the other half of the same rule, and it was held by
`bpd_core::Reporting` — a trait whose methods have no default bodies, which
forces an implementation to exist and is satisfied by an **empty one**. nothing
failed if this server took a report and dropped it

`bpd_mcp::carriage_of` is the table for it, matching `bpd_core::Told` with no
catch-all arm. every entry is `Carried::Pulled`, and that is the whole
asymmetry between the two front ends: this server writes nothing that is not an
answer, so a fact that arrives while the program is running is **kept and handed
over on the next answer**:

| what the debugger says                            | where an agent finds it                                               |
| ------------------------------------------------- | --------------------------------------------------------------------- |
| a logpoint's record                               | the `logged` key of the next answer                                   |
| a pause armed while the program ran               | `pause_armed_while_running` under `logged`                            |
| a child the program started                       | `spawned.started`                                                     |
| a way of starting a child this interpreter hides  | `spawned.cannot_see`, beside the children rather than instead of them |
| a debugged fork joining                           | `attached.sessions`, and the `sessions` tool afterwards               |
| a thread stopping                                 | `outcome: stopped`, with the frames                                   |
| the program exiting                               | `outcome: exited`, with `exit_code`                                   |
| the program ending with threads still held        | `outcome: finishing`, with `held`                                     |
| the program being over with no exit bpd can read  | `outcome: ended`, deliberately with no `exit_code` field at all       |
| a deadline passing with the program still running | `outcome: timed_out`, with `waited_ms`                                |

**a pull is a legitimate route and it is the one that has to be watched.** a
server that kept a fact and never handed it over looks exactly like one that
carries it properly, right up until somebody reads what an answer really held.
so the fake session in `crates/bpd_mcp/tests/coverage.rs` says one of
everything, the conversation is run to each of the two ways a program can end,
and the test reads the answers for what would prove each one arrived. a server
that emptied one of those methods passes every other test in the file and fails
that one

DAP's side of this table is the mirror image — everything is pushed as an event
— and the one thing it has nowhere to put is the deadline that passes, because
it answered the `continue` long before. that is this direction's single
justified exception, in a hand written list of its own kept beside the one for
capabilities

## more than one session

MCP has no push. every tool returns the answer to what was asked and the server
writes nothing else — so a second session, which appears while the program is
**running**, has to be learnable rather than announced

- **`sessions`** lists them: the id, whether bpd started that process, what each
    is holding, and how it ended. one bpd did not start — a debugged fork — has
    no exit code to read and cannot be terminated, and both are refused by name
    rather than invented
- every tool that is about a session takes an optional **`session`**. naming
    none still means the only one there is, and with several open a call that
    names none is refused with the list — `bpd_core::only_session`, which is
    `only_stop`'s rule one level up
- a tool that is about a **stop** needs neither. the stop carries the session it
    was reported from, and that is unforgeable: the engine names a stop on the
    connection it arrived on. a `session` argument that disagrees with it is
    **refused** rather than believed, because a caller that believes something
    false about which program it is looking at would have half of it confirmed

so an agent is never left to infer a session from a stop number. every rendered
stop carries `session`, which matters the moment there are two: both agents
count their stops from one

a session that joins while a call is in flight is reported on that call's answer,
under `attached`, with the sentence that says what to do about it. that is not
the same key as `spawned`: `spawned` says a child exists and is running,
`attached` says one is **held** and nothing else will move it

what produces one is `debug_children` — off by default, and see
[child processes](subprocesses.md)

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
- **attach**, which is PEP 768 and needs cpython 3.14. `launch` is the only way
    in, and a tool that needs a program says so by name
- **a subscription for a program that stops on its own.** `wait` touches the
    program in no way and returns whatever it did, so an agent that wants to hear
    about a background stop asks for one. what is not answered is the agent that is
    not asking
