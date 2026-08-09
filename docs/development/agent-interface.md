# the interface for ai agents

a design document. the requirement is blunt: **an ai agent can perform every
debugging operation a human can**, and the interface it uses should suit an
agent rather than being a human interface with a wrapper on it

## why MCP, and not DAP or LSP

- **LSP** is the wrong protocol. it models a document and a static view of it.
    a debug session is a live process with threads, stops and mutation. every
    debugging concept would have to be smuggled through custom requests, and
    nothing about LSP's document synchronisation would earn its keep
- **DAP** is the right protocol for editors, and `bpd` speaks it, because that
    is how it plugs into vs code, pycharm and neovim. it is the wrong shape for
    an agent, for reasons below
- **MCP** is what agents already speak, and its tool model matches how an agent
    thinks: call a thing, get the answer, decide what to do

so: **DAP for editors, MCP for agents, both thin adapters over one session
core**, with the parity rule from [architecture](architecture.md) making
"everything a human can do" structural rather than aspirational

## what is wrong with DAP for an agent

DAP is asynchronous and event driven, and it is *chatty by design*, because it
was built for a ui that renders a tree the user expands one node at a time.

1. **the answer arrives as an event, not as a response.** `next` returns an
    acknowledgement. the actual result — where it stopped, and why — arrives
    later as a `stopped` event. an agent that wants to step has to issue a
    request, then wait on an event stream, and correlate the two. every step is
    a state machine
1. **reading state is a tree walk.** to see one local variable: `stackTrace`,
    then `scopes` for the frame, then `variables` for the scope, then
    `variables` again for each nested object. four or more round trips, each one
    a tool call, each one a chunk of the agent's context spent on protocol
    scaffolding rather than on the program
1. **it is stateful in a way that is easy to desynchronise.** variable
    references are handles valid only until the next resume. an agent that
    reasons across turns will use a stale one

none of this is a flaw in DAP. it is a ui protocol being asked to be an api

## the shape the MCP interface takes instead

### every control operation returns the stop it produced

`step_over`, `step_in`, `step_out`, `continue`, `run_to`, `set_breakpoint`
— each blocks until the program stops again, and returns the resulting stop
state: why it stopped, which thread, the top frames, and a digest of the locals
that changed. one call, one answer, no event correlation

each takes a deadline. when the deadline passes without a stop, the call
returns a *timeout* result rather than hanging, which is itself the answer to
the question the agent was probably asking

what that result may say is narrower than it first looks. the program is still
running, so anything read off it is a **sample**, not a stop — a stack taken
from a thread that is executing describes a moment that has already gone. so a
timeout reports that it timed out, and any thread state it carries is labelled
as sampled and possibly already stale. it never presents a sampled stack as a
stopped one

this is not a limitation to be engineered away. a sample honestly labelled is
useful — it is what a profiler gives you, and "it is still inside
`socket.recv`" answers most of these questions. a sample presented as a stop is
the debugger reporting a state the program was not in

### state is queried declaratively, in one call

instead of walking scopes and variables, the agent describes what it wants:

- a set of expressions to evaluate in a given frame
- a depth to expand object graphs to, with a budget in bytes
- whether to include the source around each frame's current line

the answer comes back in one response, already at the level of detail asked
for. the DAP tree walk is still available underneath — the same session core
answers both — but an agent never pays for it

### stop conditions are expressed as intent

an agent almost never wants "step 47 times". it wants *"run until this variable
stops being `None`"*, or *"until this function is entered with a negative
argument"*, or *"until this line is hit for the eleventh time"*

those are conditions on the debuggee, and they belong in the debuggee where
they cost a native predicate rather than a round trip each. expressing them as
tools — `run_until`, `run_to`, `watch` — turns thousands of protocol exchanges
into one

### snapshot and diff

a large fraction of debugging is "what changed between here and there". the
interface supports capturing a state snapshot at one stop and asking for the
**difference** against another, rather than shipping both states to the agent
and making it compare them. the diff is the answer; the two states are raw
material

### errors are never soft

a tool that cannot do what was asked returns a failure with a reason. it never
returns a partial answer that reads like a complete one. an agent cannot see
the elision that a human would notice, so a truncated object graph says it was
truncated, and by how much, and how to ask for the rest

## a whole investigation in one call

the interface above removes a round trip per operation. a submitted **script**
removes them per *investigation*: the agent hands over a program of debugger
steps with its own branching, and gets back what happened at every step

that is the difference between "step, look, decide" fifty times and one call
that says *run until this handler is reached with a negative amount, then step
until the total changes, and tell me the frame where it did*

### it is a structured program, not a language

the steps are a schema-validated tree — `step_over`, `run_to`, `eval`, `if`,
`while`, `log`, `finish` — submitted as data, not as text to be parsed

this is not a stylistic choice. an MCP tool already takes JSON Schema input, so
a tree of steps needs **no parser, no grammar, and no syntax errors**, and the
schema is itself the documentation the agent reads before writing one. a
bespoke text language would cost a parser and a document nobody has read, to
arrive at the same semantics

the predicates inside it *are* python — expressions evaluated in a chosen frame,
through the machinery breakpoint conditions already use. that is the half python
is genuinely good at, and it is already built

### why not just let the agent submit python

it is the obvious answer and it is worse in three ways:

- **it does not terminate.** a submitted python script can loop forever and hang
    the session, so a budget is needed regardless — which removes the only real
    advantage, familiarity, while keeping the cost
- **it cannot be examined before it runs.** a step tree can be answered with
    "this runs at most 40 steps and evaluates these four expressions", and
    refused when it cannot terminate. arbitrary python cannot
- **it runs somewhere.** in the debuggee it perturbs the program being measured
    and trips its own breakpoints; outside it, it is an SDK rather than a tool,
    and the agent could have written it without us

the script runs in the **engine**, driving the session. only the predicates
reach the debuggee, which is where conditions already go — so the program under
test is disturbed by exactly the evaluations that were asked for and nothing
else

### the transcript is the return value

not the final state. an agent that receives only where a script ended cannot
tell **why** it ended there, and will guess

so every step records what it did, where the program actually was, and — for a
branch — which way it went. the same script over the same run produces the same
transcript, so an agent can re-run one to confirm a reading rather than trusting
its memory of it

three rules keep a transcript from lying:

- **a budget is mandatory** — steps, wall clock, and output bytes. exhausting one
    returns the transcript so far, **labelled partial**, naming what was left
- **a step that fails halts the script.** an evaluation that raises is reported
    and stops, unless the script gave it an explicit branch. carrying on past a
    failure produces a record of an investigation that did not happen
- **no step may report a location the program was not at.** the same rule as
    everywhere else, applied to a log of fifty of them

### it is a capability, so both adapters have it

per the parity rule below. for a human this is "run this investigation" — *run
to the third call with a negative amount and show me the stack* is a thing
people want and no debugger offers. the rule requires the capability in both
adapters; it does not require an IDE to put a button on it

## how an agent learns any of this

MCP has three primitives, and **only one of them is model-controlled**:

| primitive | who decides it is used |
| --- | --- |
| tools | the model |
| resources — read-only documents | the host application |
| prompts — reusable parameterised workflows | the user |

that asymmetry decides the design. an agent will reliably read a **tool schema**
and a **tool result**; whether it ever sees a resource is the host's choice, and
whether it sees a prompt is the user's. so the primary teaching surface is not a
document — it is the tool descriptions and, above all, the errors

this project is already suited to that. an error here names a cause and an
action, and an agent that receives

> the interpreter has run code from `x.py` but never the file itself … import it
> somewhere the program itself runs, or take the condition that imports it off

has been taught the thing at the moment it needed to know, without having read
anything. that is worth more than a page it may never pull in

the layers, in order of how reliably they arrive:

1. **tool schemas** — the semantics, always in context
1. **errors** — the corrections, at the moment they apply
1. **resources** — the deeper model, for a host that pulls them: what a stop
    claims and does not claim, what each thread mode guarantees
1. **prompts** — canonical investigations, surfaced as slash commands
1. **a skill directory in the repository** — for clients that have skills, which
    is a client feature and not part of MCP

writing 3 through 5 before 1 and 2 are right would be writing documentation to
compensate for an interface that does not explain itself

## the parity rule, concretely

a capability is added to `bpd_core` once. both adapters expose it. a pull
request that adds a DAP request without the MCP tool, or the reverse, is
incomplete — the same way a feature without a test is incomplete

## still open

- whether the MCP interface should also expose a subscription for a program
    that stops on its own — an unhandled exception in a background thread — or
    whether surfacing that on the next call is enough
- how a snapshot is addressed across turns without reintroducing DAP's stale
    handle problem. a content addressed id is the current thinking
- what the right default byte budget is for an object graph, given that it is
    spending someone's context window. the mechanism is built and its defaults
    are a placeholder — [the stopped
    state](state.md#expansion-and-saying-what-was-left-out) records what they
    are and why the number cannot be settled until there is an agent surface to
    measure it against
