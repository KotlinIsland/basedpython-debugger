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
returns a *timeout* result naming what the program is doing instead —
`still running`, `blocked on this call`, `waiting on this lock` — which is
itself the answer to the question the agent was probably asking

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
    spending someone's context window
