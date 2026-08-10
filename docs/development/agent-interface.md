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
was built for a ui that renders a tree the user expands one node at a time

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

#### three things this section got wrong, found when it was built

the control tools and their deadlines are built —
[the MCP adapter](mcp.md) is what shipped. three claims above did not survive
contact with the session:

- **there is no sample at all.** everything the agent inside the debuggee
    answers, it answers on a thread it is **holding** — including the thread
    census. so a program with nothing held cannot be asked what its threads are
    doing, and there is nothing to label. a timeout therefore carries no
    location of any kind, and names the two things that can be done instead:
    keep waiting, or `pause` and get a real stop. the paragraphs above are still
    the right rule; they describe a capability the thread model does not offer
- **`set_breakpoint` is not a control operation.** it does not resume anything
    and nothing stops as a result of it, so there is no stop for it to return.
    it is also the whole *set* rather than one breakpoint, because a debugger
    that accumulates edits has two ideas of what is set
- **`run_to` is not a control tool, and it does not fit as written.** it is
    either a composition a front end performs — arming a breakpoint of its own
    and taking it off again, which is a decision about the program made in an
    adapter — or a capability of the core that DAP has no request for at all,
    which the parity rule below forbids. and under a deadline it is unsound: a
    one-shot breakpoint cannot be removed while the program is running, so a
    timed-out `run_to` leaves the program armed with something the agent did not
    ask for. **it is built as a step of the debug script**, where the engine owns
    the whole composition including the removal — see
    [the debug script](scripts.md#run_to-lives-here-and-nowhere-else)

### state is queried declaratively, in one call

instead of walking scopes and variables, the agent describes what it wants:

- a set of expressions to evaluate in a given frame
- a depth to expand object graphs to, with a budget in bytes
- whether to include the source around each frame's current line

the answer comes back in one response, already at the level of detail asked
for. the DAP tree walk is still available underneath — the same session core
answers both — but an agent never pays for it

#### what it turned out to be

built as `Request::Query`, the `state` tool and the custom DAP request
`bpd/state` — [the state query](queries.md) is what shipped. the query is
**composed of the requests the tree walk is made of**, so the two cannot
disagree about a value; what it removes is the round trips

three things this section did not say, which the build had to decide:

- **the budget is one budget.** the paragraph above puts a byte budget beside a
    depth as though it bounded each read. it bounds the *whole* query, or a query
    of twenty parts under eight kilobytes would spend a hundred and sixty. a part
    that did not fit is named rather than absent, and the order parts are read in
    is fixed so that what a spent budget cuts is the open ended part
- **"whether to include the source" is not a boolean, and it is not free.** the
    file on disk is not evidence of what the interpreter compiled — files are
    edited while programs run, and `linecache` has exactly this bug. so source is
    read *in the debuggee* and **proved**: the file is compiled and the frame's
    own code object has to be in what comes out, line table included. a file that
    has been edited says so instead of showing a line one off
- **a frame is not a scope.** the scopes and the source are read for the frames
    the query describes, and `frames` therefore defaults to one — the frame the
    program is in. every extra frame is a scope read nobody asked for, spending
    the budget this section exists to protect

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

#### the open question below, answered

**a snapshot is not a handle, so it does not go stale.** that is the whole of
it, and it is why the question looked harder than it was. DAP's variable
reference is a *promise to read something later*, and that promise is what the
next resume breaks. a snapshot is the reading, already taken — nothing the
program does afterwards can change it

so an id stays valid for the life of the session, across any number of resumes,
and content addressing earns its place for a different reason than expected: not
to detect staleness, but so that the same state read twice **is one state**
rather than two that happen to agree. the id carries the stop as well, following
the [frame identity](state.md#a-frame-identity-says-which-stop-it-belongs-to)
precedent, which makes it self-describing to a reader

what does end with the stop is asking that stop anything more — the frame ids
*inside* a snapshot name frames that have run on, and the existing rule refuses
them. the snapshot goes on being true; what it points at cannot be asked again

and one thing the section above does not say, which the diff cannot do without:
**a value a bound cut short in either snapshot is reported as not compared**.
"unchanged" is a claim, and half a list is not evidence for it. the same applies
to a depth of the stack that is running different code in the two, and to
anything only one of them read — see [the diff](queries.md#the-diff-is-the-answer)

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

### four things this section got wrong, found when it was built

the script is built — [the debug script](scripts.md) is what shipped, with the
step vocabulary and the transcript in full. four claims above did not survive
contact with the session:

- **it is not a tree of `Request`.** [architecture](architecture.md) says the
    same enum serves four consumers and that the script is "a *tree* of it".
    it cannot be: a `Request` names a stop and a frame by **absolute id**, and
    the stop a step will run at does not exist when the script is written. so
    the steps are a vocabulary of their own, relative to the stop the script is
    currently at, which the engine turns into requests as it walks them. that is
    not a loss — a step tree an agent writes in one call could never have carried
    ids it has not been told yet
- **there is no deadline per step.** the section above gives every control
    operation its own, which is right for a *tool*, whose caller is waiting on it.
    inside a script the wall clock budget is the one clock — a script waiting for
    a program that never stops is spending exactly that — and a second deadline
    per step would be a second place to say the same thing
- **"the same script over the same run produces the same transcript" costs
    something, and it is worth saying what.** nothing measured may be in a
    transcript, so there is no duration anywhere in one; and what is comparable
    between two *processes* stops short of the interpreter's identity for a
    thread, which is the operating system's number rather than anything bpd may
    claim. within one run it is exact, which is what the sentence meant
- **a budget is not enough to make a `run_to` sound; a pause is.** the paragraph
    above says a timed-out `run_to` "leaves the program armed with something the
    agent did not ask for", and treats that as the reason it cannot be a tool. it
    is the reason — but inside a script the engine can finish the composition: it
    arms a **pause**, takes its own breakpoint off on whichever thread that holds,
    and says in the transcript that it did. that is bpd touching the program
    without being asked, and the honest resolution is to report it rather than to
    pretend the problem is gone. the one case that survives — no thread reaches a
    line at all — is answered with what is still armed and what to do about it

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

### what building it found, in the order above

all five are built — [the MCP adapter](mcp.md) has the resources, the prompts
and the skill in full. what matters is that **1 and 2 were not already right**,
and going over them first is what found it. three of the four things fixed were
not gaps in what the interface *said*; they were places where it said something
untrue:

- **`launch` read an argument its schema did not declare.** the number of frames
    the entry stop comes back with was a real setting, undiscoverable from
    `tools/list`, and a client that validates against `additionalProperties:
    false` would have refused the call that used it. a schema and a struct are
    two descriptions of one thing and nothing made them agree, so a test now
    compares them for every tool — the field list asked of serde rather than
    written down, the way the vs code manifest is checked
- **`pause` claimed a cause that was often false.** an empty `running` was
    reported as "every thread is parked in a C call", and `running` deliberately
    leaves out the threads bpd is itself **holding**. so a pause armed while a
    stop was outstanding told an agent its program was stuck in native code when
    bpd was what was holding it still. the honest form has two branches and the
    answer says which
- **every read said "this is a sample rather than a snapshot".** carried on a
    *stack* answer that is wrong, and it contradicted the `stack` tool in the
    same session: a held thread's own frame chain **is** a snapshot, because it
    is inside a monitoring callback and cannot return. what the rest of the
    program can move underneath is the values reached through it
- **"nothing is held" was one refusal doing two jobs.** a program that is
    running has to be held before it can be asked anything; a program that has
    **exited** cannot be held at all. an agent told the first about the second
    goes on pausing a process that is not there, so they are two refusals now,
    and each names what to do

what 3 through 5 were left to carry, once those were fixed, is narrower than the
list above suggests and is the honest shape of it: not what a call takes, but
what its answer **claims** and where the claim stops. the rule for a resource is
that nothing in one may be the only place something is said, and the bar for a
prompt is that it be an investigation a competent agent would otherwise get
wrong or do the long way — four met it

the one thing prose could not do for itself is stay true. every tool a resource
or a prompt names is declared beside it and checked both ways against the tool
table, and the skill is checked the same way, because a page that names a tool
after it is renamed reads exactly as well as one that is true

## the parity rule, concretely

a capability is added to `bpd_core` once. both adapters expose it. a pull
request that adds a DAP request without the MCP tool, or the reverse, is
incomplete — the same way a feature without a test is incomplete

it is a build failure rather than a review habit: `crates/bpd/tests/parity.rs`,
over the two adapters' `reach_of`, both of which are exhaustive matches on
`bpd_core::Request` with no catch-all arm. **enumerating the request variants
was not enough** — a front end can implement every one of them and still not
offer a capability carried in a *field*, which is what DAP's hit condition is —
so `bpd_core::parity::Facet` enumerates those beside them, and a protocol that
genuinely cannot carry one has to say so with the reason, in a list the test
compares against by hand

## still open

- whether the MCP interface should also expose a subscription for a program
    that stops on its own — an unhandled exception in a background thread — or
    whether surfacing that on the next call is enough. **surfacing it on the
    next call is what shipped**, and it is not obviously a compromise: `wait`
    touches the program in no way and returns whatever it did, so an agent that
    wants to hear about a background stop asks for one. what is not answered is
    the agent that is *not* asking
- ~~how a snapshot is addressed across turns without reintroducing DAP's stale
    handle problem~~. **answered**: a snapshot is a value rather than a handle
    and does not go stale at all, so an id is valid for the session. content
    addressing shipped, for the second reason above rather than the first
- what the right default byte budget is for an object graph, given that it is
    spending someone's context window. the mechanism is built and its defaults
    are a placeholder — [the stopped
    state](state.md#expansion-and-saying-what-was-left-out) records what they
    are and why the number cannot be settled until there is an agent surface to
    measure it against
