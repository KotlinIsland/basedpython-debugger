# sessions

a **session** is one control connection, to one agent, in one debugged process.
a debuggee can hold more than one of them, and an ordinary one holds one. this
page is about how they are named and how a request says which it is for

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

neither protocol has a field for a session id, and the two reach a second
session by opposite routes — which is what `Facet::Session` in the parity table
is about:

- a **DAP** session *is* the connection, so a client never writes one. the
    spec's answer to a second debuggee is the `startDebugging` reverse request:
    the adapter asks the client to start a second session, and the configuration
    it hands over carries the session id in a field of bpd's own. the client
    sends it straight back on its `attach`, and from then on that connection
    *is* that session — a request of its that names none means it, rather than
    "whichever there is". `Reach::OnItsOwn`
- an **MCP** tool takes one as an argument, because MCP has no push. `sessions`
    lists them, and every tool that is about one takes an optional `session`.
    naming none still means the only session there is. `Reach::Direct`

that is a **push** and a **pull** for the same capability, and it is the shape
the two protocols have rather than a difference in what an agent and a person
can do. `crates/bpd_dap/tests/coverage.rs` and
`crates/bpd_mcp/tests/coverage.rs` each drive their adapter through a whole
conversation and assert that a request really was addressed to the session its
stop came from, and that nothing was ever addressed anywhere else

### a stop says which session it is of

on MCP every rendered stop carries `session`, and it is not decoration: two
agents both count their stops from one, so a number alone stops naming one thing
the moment a program has forked into a debugged child. a `session` argument that
disagrees with the stop a call is about is **refused** rather than believed — the
stop was named by the connection it arrived on, which is unforgeable, and an
argument cannot change it

## a second session

the listener a debuggee attached on is **kept open** for the life of that
debuggee. an agent that connects to it and presents that debuggee's session
token becomes a second session of the same `Debuggee`, with its own breakpoint
set, its own stop numbering and its own held threads

what produces one is a **debugged fork**: the child inherits the endpoint and
the token in memory, gives the connection it inherited up, and opens one of its
own. see [child processes](subprocesses.md). it arrives **held**, so a front end
that never learned of it would have a stopped program it cannot reach — which is
why the engine reports it as it happens, through `Reporting::attached`, rather
than leaving it to be discovered

what a connection arriving there is treated as is decided by the token and by
nothing else. any local process can open a socket to a loopback port; one that
cannot answer the handshake is closed and is not counted, and the session it
interrupted carries on. it is not reported, because a peer that said nothing has
said nothing *about the program* — and the handshake is given a short deadline of
its own so that a peer which connects and stays silent cannot hold up the wait it
arrived in

the wait is what watches the door. a session spends almost all of its time
waiting for its program, so the wait polls the connection **and** the listener
rather than blocking on the connection alone. the other sessions' connections
are deliberately not read during it: a wait is addressed to one session, a stop
that arrived on another has nowhere in the answer to go, and unread bytes stay
in the kernel until a wait addressed to that session reads them

### a program bpd did not start

the second session is over a process bpd is not the parent of, and two things
follow that a front end has to be told rather than left to assume:

- **its exit is not bpd's to read.** bpd cannot reap it and never learns its
    status. the connection closing is the whole of what bpd observes, so the
    outcome is `Running::Ended` — the program is over, and there is no number.
    MCP renders it as `"outcome": "ended"` with no `exit_code` field at all, and
    DAP as `terminated` with **no** `exited`, because DAP's `exited` event
    carries an `exitCode` and there is none. a zero there would be the adapter
    inventing the one field the event is for
- **it cannot be terminated.** ending a debuggee is signalling the child bpd
    holds; there is no child. `terminate` is refused **by name**, because a
    `terminate` that quietly did nothing is one a client reads as a program that
    has been ended

## what is not built

- **nothing produces a second session but a fork.** a child that was `exec`'d —
    `subprocess`, `multiprocessing` with `spawn` — is a fresh interpreter with
    none of this process's memory in it, so there is nothing for it to inherit an
    endpoint through. it is reported and not debugged, and reaching one means
    giving it something through its environment. see
    [child processes](subprocesses.md)
- **a session cannot be joined by hand.** there is no command that opens a
    connection to a debuggee's listener; what does it is the agent inside a
    forked child, and `crates/bpd_engine/tests/sessions.rs` does it directly
    against the engine
