# child processes

`bpd` debugs one process. a program that starts another one has moved some of
its work somewhere the debugger is not, and until something says so the only
symptom is a breakpoint that never fires

that is what this page is about. `bpd` **notices** a python child and reports
it, and with [`debugChildren`](#a-child-that-is-debugged) asked for it debugs
one — however the child was made

there are two ways a program makes one and they are not variants of each other,
which is why they are two mechanisms rather than one:

- a **fork** copies this process, so the child is born holding the agent, the
    breakpoint table and the control connection's descriptors. something has to
    happen there whether child debugging was asked for or not, because doing
    nothing leaves two processes writing into one socket
- an **`exec`** replaces the program with a fresh interpreter that inherits
    nothing but the environment and the file descriptors. nothing of bpd is in
    it, so with child debugging off there is nothing to undo — and with it on
    the agent has to be *found*

with `debugChildren` **off**, which is the default, both run exactly as they
would have without a debugger. with it on, both open a session of their own and
arrive **held**: a fork at the line that forked, and an `exec`'d child at its
own interpreter startup, before its program has been compiled

## the case this exists for

`django.utils.autoreload.restart_with_reloader` calls `subprocess.run(args)`,
and the parent then does nothing but wait on the exit code — read in django 6.1.
so under the default `runserver`, the **child** serves every request, and
`bpd launch manage.py runserver` has attached its agent to a supervisor that
never imports the template engine

nothing is reported wrongly when that happens. a template breakpoint is reported
**unbound**, which is the truth. but "unbound" reads as "bpd could not find the
line", and the real answer is "the code you are pointing at is running in
another process" — so what was missing was not correctness, it was the reason

the same shape is `multiprocessing`, flask's reloader, gunicorn, and anything
that forks a worker per core

**this is what `debugChildren` is for**, and with it on `runserver` works
without `--noreload`: the reloader's child opens a session of its own and a
breakpoint in a template is bound and hit *there*. see
[django templates](django-templates.md), and
[a child that is debugged](#a-child-that-is-debugged) below

with it off — the default — the report is still what a session gets, and it is
what turns "unbound" into a reason

## how a child is noticed

through an **audit hook**, added with `PySys_AddAuditHook` from inside the
agent. cpython raises an audit event for every way of making a process, and a
hook is the interface it documents for seeing them

it is not `sys.addaudithook`. a hook is called for *every* audit event the
process raises — `open`, `import`, `exec`, `compile`, `marshal.loads` — so a
python callable there would be a python frame per file the program opens.
importing `multiprocessing` alone raises over two hundred of them. the rule that
the agent is native on hot paths applies, and an audit hook is hotter than it
looks

it is also **not** what pydevd does. `_pydev_bundle/pydev_monkey.py`
monkeypatches `os.exec*` and `subprocess` so that debugpy can rewrite the
child's command line on the way past. [python support](python-support.md) rules
monkeypatching the stdlib out, and a hook is the alternative — with the
limitation that comes with it, stated below

## which events, and why they differ by release

measured on 3.13.15, 3.14.7 and 3.15, on posix, by recording every event of
every name that each way of making a child raised:

| what the program does                  | 3.13                                   | 3.14 and 3.15                                    |
| -------------------------------------- | -------------------------------------- | ------------------------------------------------ |
| `subprocess.run([...])`                | `subprocess.Popen`                     | `subprocess.Popen`, `_posixsubprocess.fork_exec` |
| `subprocess.run(..., shell=True)`      | the same, with `/bin/sh -c`            | the same, with `/bin/sh -c`                      |
| `subprocess.run(..., close_fds=False)` | `subprocess.Popen`, `os.posix_spawn`   | `subprocess.Popen`, `os.posix_spawn`             |
| `multiprocessing`, `spawn`             | **nothing at all**                     | `_posixsubprocess.fork_exec`                     |
| `multiprocessing`, `forkserver`        | **nothing at all**                     | `_posixsubprocess.fork_exec`, `os.fork`          |
| `multiprocessing`, `fork`              | `os.fork`                              | `os.fork`                                        |
| `os.fork()`                            | `os.fork`                              | `os.fork`                                        |
| `os.execv(...)`                        | `os.exec`                              | `os.exec`                                        |
| `os.posix_spawn(...)`                  | `os.posix_spawn`                       | `os.posix_spawn`                                 |
| `os.spawnv(...)`                       | `os.fork`, then `os.exec` in the child | the same                                         |

**`_posixsubprocess.fork_exec` only became an audit event in 3.14.** so the
watch list is chosen from the running interpreter's version:

|                | watched                                                              |
| -------------- | -------------------------------------------------------------------- |
| 3.14 and later | `_posixsubprocess.fork_exec`, `os.posix_spawn`, `os.exec`, `os.fork` |
| 3.13           | `subprocess.Popen`, `os.posix_spawn`, `os.exec`, `os.fork`           |
| windows, any   | `_winapi.CreateProcess`, `os.exec`                                   |

that is **not** a capability ladder, and this project does not ship one. it is
one capability reached through the name the interpreter raises it under — the
same kind of release-to-release change as 3.14 splitting `BRANCH` into
`BRANCH_LEFT` and `BRANCH_RIGHT`, which [python support](python-support.md)
already names as a thing to check on every release. what `bpd` reports is
identical on both, apart from the one thing 3.13 genuinely cannot say — and
that one thing is said out loud

three of these choices are not obvious, and all three were forced by the
measurement:

- **on 3.14 and later, `subprocess.Popen` is not watched.** it fires for the
    same child as the event underneath it, so watching both reports every
    ordinary subprocess twice
- **on 3.13 it has to be**, because there it is the only event an ordinary
    `subprocess` child raises at all. that reopens the double against
    `os.posix_spawn`, which fires beside it whenever `close_fds=False` lets
    `subprocess` take that path. `subprocess.py` raises its event and then calls
    one of the two, on the same thread, with nothing else watched in between —
    so "the previous watched event on this thread" identifies the pair exactly,
    and that is the whole of the deduplication.
    `a_child_started_the_posix_spawn_way_is_reported_once` runs on every
    interpreter and is what proves it rather than assuming it
- **`_posixsubprocess.fork_exec` cannot be left out on 3.14.**
    `multiprocessing` with the `spawn` start method goes through
    `multiprocessing.util.spawnv_passfds`, which calls it directly — no
    `subprocess.Popen` event, no `os.*` event, nothing. a watch list built from
    the obvious names would miss every `multiprocessing` child on posix

## the blind spot on 3.13, which is stated rather than left silent

on 3.13, a `multiprocessing` child started with the `spawn` or `forkserver`
start method raises **no audit event of any name**. that is not a gap in the
watch list — it was established by recording every event raised while one
starts, and the whole set is `import`, `exec`, `open`, `marshal.loads` and some
sockets. there is nothing to watch

a feature whose normal output is silence cannot afford an unannounced blind
spot: a user who sees no report concludes there was no child, and on 3.13 that
conclusion would be wrong. so `bpd` **says so** — once per program, on the
import of `multiprocessing`, which is the first moment at which such a child
becomes possible:

> this program imported `multiprocessing`, and python 3.13 raises no event at
> all when it starts a child with the `spawn` or `forkserver` start method — so
> bpd cannot see one, and silence here does not mean there was none.
> `_posixsubprocess.fork_exec` became an audit event in 3.14, where this is
> visible. the `fork` start method is visible on every version, and so is
> `subprocess`

it is announced on the import rather than at launch on purpose. a warning on
every 3.13 launch of every program — most of which never start a child — is a
warning everybody learns to skip, and then nobody reads it when it matters

it reaches a client the same three ways a child does, and on DAP it goes on the
**`important`** category rather than `console`: everything else `bpd` says is a
positive claim, and this is the one message about an *absence*, which a
collapsed console must not hide. on MCP it is `spawned.cannot_see`, carrying
`silence_is_not_evidence`, beside the children rather than instead of them

windows has no such blind spot: `multiprocessing`'s spawn method reaches a child
through `_winapi.CreateProcess` there, which has been an audit event since
PEP 578 landed in 3.8

## what a report can and cannot know

everything `bpd` sees is the argument vector the program is about to hand the
operating system. that is enough to recognise the interpreter it is already
running, and it is not enough to know what `/bin/sh -c "…"` will do

so the verdict has three values and one of them is "cannot tell". the rule, in
order:

1. one of the candidate programs **is** the interpreter this process is running,
    compared as a resolved path. this is as certain as an argument vector gets,
    and it is the django case — the reloader starts `sys.executable`
2. a candidate program's **file name** is a python interpreter's name —
    `python`, `python3`, `python3.14`, `python3.14t`, `pythonw`, any of them with
    a windows `.exe`. a name is evidence and not proof, because a file called
    `python` can be a wrapper script, so the report says which name it read
3. an **argument** after the first is a python interpreter's name. this is
    `/usr/bin/env python3 …` and `uv run python …`, where the child's program is
    a launcher and what it will actually run is the launcher's business.
    reported as uncertain, in those words
4. none of those, and **nothing is said at all**

the fourth case is the one that keeps this readable. a build script that shells
out to `git`, `ls` and `sh` fifty times would otherwise bury whatever the
debugger had to say, and a report nobody reads is the same as no report

### what rule three deliberately does not do

it does not look **inside** an argument. `sh -c "python app.py"` is one string,
and splitting it to hunt for a word would report a python child for
`git commit -m "port to python 3"`

so a command handed to a shell is not examined, and a python grandchild reached
that way is missed. that is a stated limit rather than a silence

## a fork is a different thing, and is reported as one

`fork` copies the process and keeps **only the calling thread**. what the child
inherits was measured on 3.13, 3.14 and 3.15, on a gil build and a
free-threaded one:

- the `sys.monitoring` tool id, still held and still named `bpd`
- every global and local event, unchanged, with the callbacks still registered
    and still firing
- the breakpoint table, the code registry and the stop registry, because they
    are memory
- **every file descriptor this session opened** — the two on the control
    connection, and the two of the socket pair the reader thread is woken through

and what it does not inherit is the thread that reads that connection. so a
forked child would be an armed debuggee that can write to the session socket and
can never be answered:

- `bpd_protocol` frames are length prefixed, and two writers interleaving
    mid-frame desynchronise the stream. the engine turns that into "the peer sent
    a message this build does not understand" — the debugger blaming its own
    protocol for the program having forked
- a child that reached a breakpoint would report a stop and then wait for a
    resume that no process is able to send. its own thread would be held for ever
    inside a callback

so a fork gets its own report, saying that the child was a copy of this process,
that it gave the agent's monitoring state and this session's connection up
before it ran a line, and that `bpd` is not debugging it as a session of its own

### the child stops being a debuggee

this is what a fork does **unless [`debugChildren`](#a-child-that-is-debugged)
was asked for**, and it is the default. that report is only true because of what
the child then does, which is: give the whole session up, before `os.fork()` has
returned to python, and run exactly as it would have if the program had never
been launched under `bpd`

it is arranged with `os.register_at_fork(after_in_child=…)`, registered once from
the agent at attach, in the same call as the `before` and `after_in_parent`
handlers [below](#the-parent-is-not-left-multi-threaded-either) — one
registration, so cpython decides their order relative to each other rather than
anything here. `os` is already imported — the entry point uses it to take `bpd`'s
own variables back out of `os.environ` — so this adds nothing to the debuggee's
`sys.modules`, and there is no python code in any of the three: they are native
functions the interpreter holds references to

**not `pthread_atfork`.** a `pthread_atfork` child handler is called by the C
library from inside `fork()`, before cpython has put its own runtime back
together — the GIL, the import lock and the per-interpreter locks are in whatever
state the fork left them, and every call the handler has to make is a call into
the interpreter. cpython runs `after_in_child` from `PyOS_AfterFork_Child`,
*after* reinitialising all of that and *before* `os.fork()` returns, which is the
one place where both halves of the job are possible. it also has an order
relative to cpython's own handlers, which `pthread_atfork` does not

what the handler does, in this order:

1. **gives up the session.** from that instruction on nothing in the agent
    writes a frame: the one place a frame is written checks it first
1. **closes both inherited descriptors** — see below
1. **replaces the cells the session's state lives in**, so that the child can use
    them again without waiting on a lock a thread the fork did not keep was
    holding — see below
1. **takes the tool off the process.** the local events on every code object the
    session armed, then the global events, then the callbacks, then the tool id
    itself. all four, because they are independent: `free_tool_id` explicitly
    does *not* clear events, so an id handed back with local events still set on
    it is an id the next tool to claim it would receive this session's
    breakpoints on

the code objects it clears come from the one place `set_local_events` is called,
so that set is complete by construction rather than by every part of the agent
that arms one remembering to say so. it is not `sys.monitoring.clear_tool_id`,
which does all four in one call and arrived in 3.14: the minimum here is 3.13,
and a second mechanism for the newer release would be two things to keep true
instead of one

a child of a child inherits the handler as well, and it is idempotent — the
second run finds the session already given up and returns

### why the descriptors are closed, and what closing them does to the parent

a socket is closed when the **last** descriptor referring to it is. a fork copies
the descriptor *table*, so parent and child hold separate entries pointing at one
open socket, and a child that kept its copies would hold this session's
connection open for as long as it ran — the engine would go on waiting to read
from a debuggee that had exited, on a connection whose only remaining owner is a
process it is not debugging. every worker pool and every reloader has exactly
that shape, and
`a_child_that_outlives_its_parent_does_not_hold_the_session_open` is what proves
it

closing them in the child does **nothing** to the process it was forked from. the
parent's own entries are untouched, and no FIN is sent while it still holds them
— measured against a real socket, and pinned by
`a_fork_leaves_the_parents_session_exactly_as_it_was`, which has the parent reach
a breakpoint after the fork and read its own tool id back

### the handler takes no lock at all

a fork keeps only the calling thread, so a lock another thread held at the
instant of the fork is one the child's copy would wait on for ever — and unlike
almost everything else in the agent, that is not something the GIL rules out. the
reader thread is off the process by then, because the `before` handler stood it
down. what is still on it is the **program's** threads: one inside `send` holds
the writing end across a socket write, and one registering a stop holds the stop
registry. on a gil build `os.fork()` holds the GIL and so do both of those, which
keeps them apart. on a free-threaded build nothing does

measured on 3.14t, and the measurement is the reason this is written down rather
than assumed either way: with the stop registry deliberately held for twenty
milliseconds by a thread registering itself, two hundred forks all **waited**,
and not one child inherited a locked table. so a free-threaded `os.fork()` does
appear to wait for the threads attached to the interpreter, and the hazard is
not reachable through a program thread on 3.14t today

that is an observation about one interpreter, not a guarantee. nothing in the
language reference or in PEP 703 promises it, a release could narrow it, and it
covers only threads the interpreter can see. relying on it would be the agent
being correct because of something it does not control, in the one place a wrong
guess is a debugger that hangs — so the agent does not rely on it, and what
follows is why it does not have to

so nothing the handler does takes one:

- the descriptors are closed **by number**. their owning values are unreachable
    in a forked child anyway — one is on the stack of a thread that did not
    survive, the other behind the lock in question
- the code objects to clear come from an **immutable snapshot behind an atomic
    pointer**, republished by the one place `set_local_events` is called whenever
    the set changes. what the child reads is whatever was published at the instant
    of the fork
- the snapshot is deliberately a **superset** of what is armed rather than
    exactly it: a code object joining the set is published before the interpreter
    is told and one leaving it afterwards, so a fork landing between the two finds
    a code object listed that has nothing on it. clearing that one is a no-op,
    where losing one that really was armed would not be
- the writing end, the reader and the stop registry are **replaced rather than
    emptied**, one atomic store each. see below

### the cells a child replaces

the three pieces of state a session keeps are not `static Mutex<T>`. each is a
`ForkCell`: an atomic pointer to a `Mutex<T>` on the heap. giving one up is a
store of null, and the next use makes a fresh one — so a child never takes a lock
to stop using what it inherited

| cell              | what a thread could be holding it for             | what the child gets          |
| ----------------- | ------------------------------------------------- | ---------------------------- |
| the writing end   | a frame being written to the session socket       | nothing connected            |
| the reader        | a concurrent fork standing the reader thread down | no reader, and none to start |
| the stop registry | a thread registering itself as held               | no thread held               |

**the cell that is given up is not freed.** either reason on its own is enough:
its mutex can be locked by a thread the fork did not keep, and destroying a
locked mutex is undefined; and what it holds owns the descriptor *numbers* the
handler has just closed, so dropping it would `close(2)` a number the child has
since recycled — the debugger closing a file the program opened. what leaks is
one box per fork, in a process that was about to allocate anyway

**the stop counter is not one of them.** it lives outside the table it numbers,
in an atomic, and a forked child **keeps** it. that is not a claim that two
processes cannot land on the same number — counting on from the same place, they
can. what inheriting removes is the collision that resetting to one guarantees: a
child starting again at one reissues, immediately, numbers its parent has already
reported. that a number can still name a stop in two sessions is why a stop is
named by the session it arrived on — see
[sessions](sessions.md) — and why a request naming a number that two sessions
hold is refused rather than answered from whichever is nearest

the mechanism is pinned by the `ForkCell` tests in the agent, which abandon a
cell that is **locked** and require the replacement to be a different cell that
locks. what drives it against a real interpreter is
`a_child_forked_while_threads_are_registering_stops_runs_to_the_end`: three
threads go round a breakpoint as fast as the session resumes them while the main
thread forks two hundred times, and every child leaves the loop, runs off the end
of the program and out through the agent's exit path — which reads the stop
registry it inherited. a child that inherited a locked one waits there for ever,
and its parent waits for it in `waitpid`

that test does **not** fail on today's interpreters without the cells, and it is
worth saying why rather than leaving it looking like coverage: the fork waits, as
above, so the child never inherits a locked table to hang on. what it does catch
is the child no longer having a special case — the exit path used to return early
in a forked child precisely because the registry was not safe to read there, and
now it reads it like any other process

### only the process that attached reports

the audit hook is inherited by a forked child too. it compares the pid against
the one recorded at attach and stays silent when they differ, so a child never
writes into the parent's socket. a child that opened a session of **its own**
records itself instead, because its children are then its session's to report

the consequence is stated rather than hidden: an `os.exec` performed **inside** a
forked child is not reported, and the `os.fork` that made it is. `os.spawnv` is
the shape that produces it

### what a forked child can tell

nothing. `a_forked_child_sees_exactly_what_it_would_have_seen_without_the_debugger`
runs one program twice — once bare and once under `bpd`, with a breakpoint armed
on a line the child runs — and requires the two children's records of
`sys.monitoring.get_tool`, `get_events`, `get_local_events`, `sys.argv`,
`sys.path[0]`, `__name__` and `__file__` to be identical

### the parent is not left multi-threaded either

the agent reads the control connection on a thread of its own, and since 3.12
cpython counts the process's **operating system** threads at `os.fork()` and
raises a `DeprecationWarning` when there is more than one. that thread is
registered with nothing, so `threading.active_count()` is `1` and
`threading.enumerate()` is `['MainThread']` under `bpd` exactly as bare — the
count cpython takes is the one place it shows

it is not a matter of output. this program

```py
import os, warnings

with warnings.catch_warnings(record=True) as caught:
    warnings.simplefilter("always")
    pid = os.fork()
    if pid == 0:
        os._exit(0)
    os.waitpid(pid, 0)
print([w.category.__name__ for w in caught])
```

prints `[]` bare. under `bpd` it printed `['DeprecationWarning']` — the program's
own recorded data, differing — and it prints `[]` now. so it is the same class of
thing as every other assertion in `crates/bpd/tests/launch_parity.rs`, and that
is where
`a_program_that_forks_records_exactly_the_warnings_it_would_have` now lives. it
compares what the program recorded, not what reached stderr, and it forks a
second time with a thread the program started itself — which has to warn on both
runs, or the first comparison is being made against an interpreter that stopped
counting

**the warning is taken away by taking the thread away.** the count keys on the
threads alive at the instant of the fork and not on whether the process ever had
one, so a thread that is not running then does not produce it. the agent
registers `before` and `after_in_parent` beside the `after_in_child` above: the
first stands the reader thread down, the second starts it again. where cpython
takes the count was measured from both sides, on 3.13, 3.14, 3.15 and a
free-threaded 3.14 — a thread stopped in a `before` handler is not counted, and
one started in an `after_in_parent` handler is not counted either

the thread is **joined**, not signalled and abandoned. `join` is `pthread_join`,
and that is the only thing that says the operating system thread has gone, which
is what is being counted

this is the mechanism rather than the instance: `os.forkpty()` warns the same way
and runs the same handlers — measured — so it is covered by the same registration
rather than by a second one

#### the window, and what arrives in it

between `before` and `after_in_parent` nothing is reading the connection. a
request that arrives there **waits in the kernel's receive buffer** and is read
afterwards. that is true because of where the reader is allowed to stop: it
waits with `poll` on the socket *and* on a wakeup, and standing down wins over a
frame that has begun to arrive — so it only ever stands down **between** frames,
never owning part of one. nothing is dropped, and a length-prefixed stream that
no reader has half-consumed cannot desynchronise

the engine is not asked to know about any of this. it writes when it has
something to say and the bytes wait

#### a stop in flight

a thread held at a breakpoint is held with the GIL released, so a fork on another
thread neither waits for it nor disturbs it. the resume it is waiting for is a
request like any other: if it arrives in the window it waits in the receive
buffer and is delivered when the reader starts again, and the thread stays held
until then rather than being let go early or lost.
`a_thread_held_at_a_breakpoint_is_still_answered_while_another_thread_forks` is
what says so — it holds a worker on a breakpoint, has the main thread fork over
and over until the test releases it, and walks the stack and evaluates in the
middle of that

the `before` handler runs on the forking thread **with the GIL held, and does not
give it back**. everything it does is a socket write and a join, none of which
needs the interpreter — and releasing it would put a wait for the GIL inside
`os.fork()` that a bare run does not have, where a C extension holding the GIL
elsewhere could hold the program's fork up

#### two threads forking at once

`before` counts rather than flags, and the reader goes back on when the **last**
fork in flight is through. so a fork that starts while another is still going
still finds the thread gone, instead of finding it just restarted by the other
one's `after_in_parent`

#### if it cannot be started again

then the session is over — nothing can answer a stop or deliver a resume — and
the program would carry on undebugged. so it does not carry on: the agent writes
the reason to stderr and exits, the same answer this project already gives when
the debugger disappears mid-session. a debuggee running unobserved after the
thing watching it has gone is the outcome that is refused

#### what is left, and is not claimed away

arming a **pause** needs a thread of the agent's own for the length of one
arming, because the reader must not block on the GIL. a fork landing in that
window still finds a second thread and still warns. it is not waited for, and
that is deliberate rather than unfinished: it is a thread that is waiting for the
GIL, so waiting for it inside `os.fork()` would make the program's fork depend on
the GIL becoming free — and a C extension holding the GIL while it waits on the
forking thread would then deadlock. a warning that a debugger's own explicit
interrupt raced with a fork is a smaller thing to be wrong about than a fork that
never returns

## where the report comes out

the same fact, in the three places, from one sentence written once in
`bpd_core::Spawn` — so a person and an agent looking at the same session read
the same words:

- **the CLI** — on stderr, prefixed `bpd:`, as it happens. it is the debugger
    talking and not the program, and a supervisor that starts a child and then
    waits for ever never reaches an end to report at
- **DAP** — an `output` event on the `console` category. `console` and not
    `stdout`, because the program did not write it. it carries no `source` and no
    `line`: the hook sees what the program asked the operating system for, not
    where it was asked, and a location taken from whichever frame happened to be
    running is a location nobody can act on
- **MCP** — a `spawned` key on the answer of the call the program was running
    during. a session that joined is a separate key, `attached`, because it is a
    separate claim: `spawned` says a child exists, and `attached` says one is
    **held** and waiting to be told what to do. this server writes nothing that is not an answer, so there is no
    event to correlate. beside the sentence it carries the event, the executable,
    the arguments, the verdict, `certain`, and `debugged: false` — an agent that
    assumed the child was being debugged would set breakpoints in it and wait for
    stops that never come

## what a program can still not tell

the guarantee that a program cannot tell it is being debugged is unchanged, and
it is unchanged **as tested** rather than as claimed:
`a_program_that_watches_its_own_audit_events_sees_exactly_the_ones_it_would_have`
in `crates/bpd/tests/launch_parity.rs` runs a program that installs its own
audit hook, records every event it receives, and starts two children — once
bare and once under `bpd`. the two streams are compared

cpython exposes `sys.addaudithook` and `sys.audit` and **nothing that enumerates
the installed hooks**, so there is nothing to read `bpd`'s out of. the test
asserts that too, so that a future release growing such a call fails here rather
than quietly ending the guarantee

the environment and `sys.path` are untouched by any of this. **noticing** a child
adds no variable, no path entry and no module, and neither does debugging a
forked one. what does is debugging a child that was `exec`'d, which is the one
feature in bpd a program can see and is enumerated in
[what a program can tell](#what-a-program-can-tell-with-child-debugging-on)

the fork handlers are the same shape: cpython exposes `os.register_at_fork` and
**nothing that enumerates what has been registered**, so a program cannot read
`bpd`'s handlers out either. what a program records around its own `os.fork()`
is compared both ways above, and the one case that is still not equal is named
there

what a debuggee does have is **open file descriptors a bare run does not** — the
control connection's two, and the two of the pair the reader thread is woken
through. a program that walks its own `/dev/fd` sees them. that is the session
itself rather than anything on this page, and it is the one fingerprint that
cannot go while the agent is talking to an engine at all

## a child that is debugged

**off by default, and it stays off.** a debugged fork *stops*, and a setting
that produced stopped processes without being asked for would be a debugger that
hangs programs by default. debugpy defaults its equivalent to on and that is the
one thing in its design not to copy

it is `bpd_core::Request::DebugChildren`, reached as the `debugChildren` field
of a DAP launch configuration and as MCP's `debug_children` tool. what comes
back is what the **agent** says is set, read off the process that will make the
child rather than echoed from the request

**one setting, two mechanisms.** there is one question a user asks and two ways a
child comes into being, so they are set together and never apart — a debuggee
where one was on and the other off would debug half the children a program makes,
and which half would depend on a start method the user did not choose. what
follows is the fork; [a child that is `exec`'d](#a-child-that-is-execd) is the
other half

### a fork needs nothing it was not born with

a fork inherits memory, so the endpoint and the token the child presents are the
ones its parent was given — held in a `OnceLock` since attach. **no environment
variable, no `sitecustomize`, no path entry, and no file.** every assertion in
`crates/bpd/tests/launch_parity.rs` is untouched by this feature, including the
two that compare the whole environment and the whole `sys.path` against a bare
run

the setting reaches the child the same way, and it has to have been set
**before** the fork: `after_in_child` runs inside `os.fork()`, on the only
thread the fork kept, with nobody left to ask. it is an atomic, read without a
lock, for the reason everything else that handler touches is

### what the handler does instead of disarming

the same first two steps — give the inherited session up, close the four
descriptors, replace the cells — and then, instead of taking the tool off the
process:

1. **open a connection of its own**, to the endpoint it inherited, presenting
    the token it inherited. that is a fifth descriptor rather than a second owner
    of its parent's: the parent's four were closed first, so there is no instant
    at which this process holds a writable handle on a socket it does not own
1. **record itself as the process that reports children**, so that its own
    subprocesses are its session's to report. the audit hook compares the pid
    against the one recorded at attach, which is what kept a detached child off
    its parent's socket
1. **report a stop and hold there.** measured on 3.13, 3.14, 3.15 and a
    free-threaded 3.14: in `after_in_child` the python frame chain of the
    `os.fork()` caller is intact and `sys.monitoring.get_tool` still names this
    tool — so the child has a stack to walk and a breakpoint table it inherited,
    and the line that forked is where it is held

the reason is `StopReason::Forked`, carrying the file, the line and the
**parent's pid**. it is the child's `Entry`: nothing of the child has run. the
parent's pid is on it because the two sessions are otherwise unrelated numbers,
and a client shown two with nothing between them cannot tell which program made
which

the stop **number** is not 1. the counter lives outside the table it numbers and
is inherited, so a child carries on from where its parent had got to — a child
starting again at one would reissue, immediately, numbers its parent has already
reported

### a fork of a fork

the handler is inherited by every generation, and a child that opened a session
of its own is a debuggee like any other: its own fork is a third session, held
at the line **it** forked on. `debugChildren` reaches it because the atomic is
inherited too, and on DAP because the `startDebugging` configuration is written
out of the parent's — so the child's session carries the same settings, this one
among them

### if it cannot reconnect

the child says so on its own stderr, prefixed `bpd:`, naming the endpoint and
the failure, and then does exactly what it does with child debugging off: takes
the tool off the process and runs undebugged. there is no other channel — the
connection it inherited has been closed and the one of its own is what failed —
and it is not silence

it is deliberately not the exit the agent takes when the debugger vanishes
mid-session. that rule is about a session that *existed* and was lost; this
child never had one, and killing a worker because the debugger could not reach
it would be the debugger changing what the program did

### what it costs, and is not claimed away

the child inherits the whole of the session's state, and that includes its
locks. the three the fork handler itself needs — the writing end, the reader,
the stop registry — are `ForkCell`s and are replaced rather than taken, so
reconnecting and stopping need no lock at all. what is **not** replaced is
everything the child goes on to use as a debuggee: the breakpoint table, the
code registry, the armed set. those have to survive the fork with their contents
— they are the point — so they cannot be abandoned

on a gil build `os.fork()` holds the GIL and so does every thread that could be
holding one of them, which keeps them apart. on a free-threaded build nothing
does, beyond the stop-the-world `os.fork()` was measured to perform there — and
that is an observation about one interpreter rather than a guarantee. it is the
same exposure any program has when it forks from a multi-threaded process, which
is why cpython reinitialises its own locks in `PyOS_AfterFork_Child`, and it is
stated here rather than left to be discovered

## a child that is `exec`'d

`subprocess`, `multiprocessing` with the `spawn` or `forkserver` start method,
`os.execv`, `os.posix_spawn` — all of them end in a **fresh interpreter** with
none of this process's memory in it. there is nothing for it to inherit an
endpoint through, so the agent has to be found through the only two things an
`exec` does inherit: the environment, and the file descriptors

### the mechanism, and why there is no second candidate

| option                               | reaches the child | what it costs                                                                                                                                                          |
| ------------------------------------ | ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `PYTHONPATH` + a `sitecustomize`     | yes               | inherited by **every** descendant, and visible                                                                                                                         |
| `PYTHONSTARTUP`                      | no                | interactive sessions only — not `-c`, `-m` or a script                                                                                                                 |
| `PYTHONPATH` + a `.pth` file         | yes               | the same inheritance, and it runs for every venv on the path                                                                                                           |
| an audit hook that rewrites          | not soundly       | it *can* rewrite a child's arguments — measured — but only through where cpython raises the event relative to where it reads the list back, which no document promises |
| monkeypatch `subprocess`             | yes               | what debugpy does, in `_pydev_bundle/pydev_monkey.py`, and ruled out by [python support](python-support.md)                                                            |
| `PYTHONEXECUTABLE` / a launcher shim | yes               | replaces the interpreter the program named, which is a lie about what ran                                                                                              |

so it is **`PYTHONPATH` plus a `sitecustomize`**. [launching](launching.md)
rejected a `sitecustomize` for the *parent* precisely because it is inherited by
every subprocess — and that inherited-ness is the property a child needs. the
rejection stands for the parent and is reversed for children, which is a
different decision about a different process rather than a change of mind

### what the file is, and where it lives

eleven lines, in a directory of its own holding nothing else, cached under
`~/.cache/bpd/children/<sha-256 of its bytes>/`. it reads three variables,
imports the agent, and calls one function. every decision it could contain
belongs in the agent, where it is rust and is tested

it is **not** basedpython under `python/`: the architecture invariant puts a
python layer there when it is more than about a dozen lines, and this is under
that. it is also the one file in the tree that has to be readable by an
interpreter bpd was not built for, because a program can start any python — and
what it prints when the agent will not import into one is the whole of what a
user has to act on

it is **appended** to `PYTHONPATH` and never prepended. the agent's own staged
directory is prepended at launch, and
`a_program_that_reads_its_own_import_path_finds_no_debugger_on_it` exists
because a directory searched before everything else is the debugger deciding
what the program imports. appended, it cannot shadow a module of the program's
own — and the directory holds one file, so there is nothing in it to shadow with
but `sitecustomize` itself

it is idempotent. the same directory is on the **parent's** `sys.path` too, so a
program that does `import sitecustomize` by hand reaches the child entry point in
a process that already has a session — and it returns, because the first thing it
asks is whether this process has one. a child of a child is a different
interpreter, has no session, and attaches

### the token is not the session's

the session token is taken out of the environment before a line of the program
runs, and it has to stay out: anything that can read a process's environment
could otherwise write frames into the session bpd is already answering. so a
child is given a **different** secret, `BPD_CHILD_TOKEN`, whose whole power is to
*open* a session. the engine's listener accepts either, and what a connection
becomes is the same

it is **not rotated per child**, and that is a limit rather than an omission:
`subprocess` builds the child's environment block before the audit event is
raised, so the only way to give each child its own would be to rewrite that block
from a hook — the undocumented path ruled out above. for as long as child
debugging is on, any local process that can read this debuggee's environment can
open a session on its listener. it cannot reach the session bpd is already
holding

### where the child is held

at its own interpreter startup, from `site`, before `__main__` exists and before
a line of its program has been compiled. that is `StopReason::Started`, carrying
the pid of the process that started it, and it carries **no file and no line** —
the only python running is bpd's own eleven, and reporting those as the program's
location would be the debugger pointing at itself. the stack is **empty** for the
same reason, and that is the truth about a process that has not begun its program

so it is the child's entry stop. a breakpoint set there is bound when the child
compiles the file, which the agent announces while the child runs, exactly as it
does in a program that imports a module late

### a child that is not python, and a grandchild that is

a child that is **not** python — the `/bin/sh` behind `shell=True`, `git`, `ls` —
inherits the variables and **ignores** them, because nothing but an interpreter
reads `PYTHONPATH`. that is inert

a **grandchild** that is python inherits them too and attaches. say plainly which
that is: it is **the feature working through a shell**, not a surprise.
`sh -c "python worker.py"` really is a python child of this program, and it is
one the audit hook's report deliberately cannot see — [what rule three
deliberately does not do](#what-rule-three-deliberately-does-not-do) says why.
so a program whose children are debugged will find its python descendants
debugged however deep the shell is, and a program that wanted only its immediate
children debugged has no way to say so

an interpreter started with `-E`, `-I` or `-S` reaches none of it: the first two
ignore `PYTHONPATH`, and the third does not import `site`. a child of one runs
exactly as it would have, and is reported and not debugged

### what a debugged child gains

`bpd_agent` and `sitecustomize`, and nothing else — compared against a child of
the same program run with `debugChildren` off, in
`a_debugged_child_gains_exactly_the_modules_that_are_written_down`. the
**parent's** list did not move: the directory holding `sitecustomize` goes on the
parent's path after `site` has already run, so the parent never imports it

## what a program can tell, with child debugging on

this is the one feature in bpd that a program can notice, and the rule is written
into the tests in both directions rather than into a paragraph:

> a program run under `bpd` cannot tell it is being debugged. a program run under
> `bpd` **with child debugging asked for** can — it has `PYTHONPATH` ending in a
> directory holding a `sitecustomize`, three `BPD_CHILD_*` names, and exactly one
> extra `sys.path` entry, which is the last one. it can see nothing else, and off
> is the default

- the **off** case is `a_program_that_reads_its_own_environment_finds_no_debugger_in_it`
    and `a_program_that_reads_its_own_import_path_finds_no_debugger_on_it` in
    `crates/bpd/tests/launch_parity.rs`, which compare the whole environment and
    the whole `sys.path` against a bare run. neither moved by a byte when this
    landed
- the **on** case is `a_program_whose_children_are_debugged_can_tell_and_only_that_much`
    beside them, with an enumerated list of four names and a reason each. a fifth
    name fails there

`sys.path` gains the directory as well as `PYTHONPATH`, and it is appended so it
is the **last** entry. the two have to agree: a `PYTHONPATH` naming a directory
this interpreter's `sys.path` does not have is a lie about this process, and
programs read it back — several rebuild the variable out of `sys.path`, which
would drop the channel on the way to a child

turning it **off** puts all of it back, exactly: `PYTHONPATH` as it was, absent
if it was absent, which is not the same as set and empty

## what is not built

**a token per child.** see above: the environment block is fixed before bpd is
told a child is coming, and the only mechanism that could rewrite it is the one
this design rules out

**windows.** the mechanism is portable — `PYTHONPATH` and `sitecustomize` are not
posix — but `debugChildren` is refused where there is no `fork`, because half a
feature reported as the whole of one is what this project does not ship

**`multiprocessing` with `spawn` or `forkserver` on 3.13 is still not
*reported*.** that blind spot is about the audit hook and is
[stated above](#the-blind-spot-on-313-which-is-stated-rather-than-left-silent).
such a child is *debugged* on 3.13 exactly as it is on 3.14 — the environment
reaches it either way — it is only the notice that bpd cannot give
