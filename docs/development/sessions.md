# sessions

a **session** is one control connection, to one agent, in one debugged process.
`bpd` runs exactly one of them today. this page is about the fact that it is
*named*, which is what has to be true before there can be a second

## why an id exists at all

every id that names something inside a debuggee is minted by the **agent** and
counts from one:

- a stop's number, `Stop::stop`
- `FrameId::stop`, which is that number plus a depth
- `SnapshotId::stop`, the same

an agent cannot see another agent, so two of them give the number 1 to two
different stops — and every id built on it inherits the collision. a second
debuggee therefore cannot be added by holding two of anything: it needs a name
for *which* debuggee, and the name has to come from something that can see both

that is the engine. `bpd_core::SessionId` is minted there, once per attached
agent, and it is what makes the pair `(session, stop)` unique

## a stop is named where it arrives, not where it is made

what crosses the control connection is `bpd_core::Reported` — everything about a
stop the **debuggee** can know, which is all of a `Stop` but the session. the
engine turns one into a `Stop` with `Reported::in_session`, on the connection it
arrived on

it is done that way rather than by telling the agent its own id, and the reason
is not tidiness:

- the connection is the only unforgeable evidence of which session a report
    belongs to. an agent that echoed an id back would be a second source of
    truth for it, and a debugger with two sources of truth for "which program is
    this" has a way to report one program's state as another's
- there is no session-shaped hole in the agent. nothing in the debuggee needs to
    know what the engine calls it, and a value carried through a process that
    never reads it is a value that can go stale without anything noticing

so a `Stop` that nothing named cannot be constructed: the field has no default
and `in_session` is the only way to get one

## what a request that names no session means

`bpd_core::Addressed` is a request and the session it is for. naming none is the
ordinary case, and the rule for it is `bpd_core::only_session`, beside
`only_stop` and for the same reason — every front end has to apply it, and two
of them applying their own would make the same call mean two things

| what is open  | what a request naming none does               |
| ------------- | --------------------------------------------- |
| one session   | it is answered by that session                |
| more than one | it is **refused**, and the refusal lists them |

that is `only_stop`'s rule one level up: refuse rather than pick. today only the
first row can happen, and the second is written and tested rather than left to
be decided by whoever adds the second session — a debugger that answered from
whichever session came first would be reporting one program's state as
another's, and it would be doing it silently

an id that names no open session is refused as well, with what was asked for and
what is held. it means a session that has ended or one this engine never minted,
and resolving it to the nearest one would be the same wrong answer by a
different route. this is the rule
`a_frame_id_from_a_stop_that_has_ended_is_refused` already applies to frames

## how a front end addresses one

`Addressed::of` is the rule, in the core, and both adapters address with it —
bar DAP's own wait loop, which names none directly because a wait is about the
program and cannot be about a stop:

- a request that is **about a stop** goes to the session that stop was reported
    from. the stop carries it, which is the whole of why it does
- a request that is about the **program** — the breakpoint set, a run, a pause,
    the thread census — names no session, and the only-session rule answers it
- a stop number the front end has not been told about, or one that two sessions
    both hold, names no session either. the number does not identify one, and
    naming none is what makes the engine refuse rather than the adapter guess

neither protocol has a field for a session id, and neither needs one while there
is one session:

- a **DAP** session *is* the connection. the spec's answer to a second debuggee
    is the `startDebugging` reverse request, which has the client start a whole
    second session of its own
- an **MCP** tool could take one as an argument, and none does. the tool that
    lists the sessions is what would make an argument meaningful, and it arrives
    with the second session rather than before it

both are recorded as `Reach::OnItsOwn` in `reach_of_facet`, against
`Facet::Session`, so the parity test holds this to the same standard as every
other capability: `crates/bpd_dap/tests/coverage.rs` and
`crates/bpd_mcp/tests/coverage.rs` each drive their adapter through a whole
conversation and assert that a request really was addressed to the session its
stop came from, and that nothing was ever addressed anywhere else

## what is not built

**a second session.** there is one `Debuggee`, it holds one `Session`, and the
listener still accepts exactly one agent. what exists is the naming:

- nothing lists the sessions, because there is one to list
- no tool takes a session argument and no DAP request carries one
- a whole-program request from either front end names no session, so with two
    open it would be refused rather than routed. giving a front end a way to say
    which one is the same work as giving it a way to *learn* there are two, and
    both belong with the feature that produces one

the feature that produces one is a debugged child process — see
[child processes](subprocesses.md)
