# child processes

`bpd` debugs one process. a program that starts another one has moved some of
its work somewhere the debugger is not, and until something says so the only
symptom is a breakpoint that never fires

that is what this page is about. `bpd` **notices** a python child and reports
it, and it does not debug one yet

a child that was started — by `subprocess`, by `multiprocessing`, by `exec` — is
not touched at all: it runs exactly as it would have. a child that was
**forked** is the one case where something has to happen, because a fork copies
the debugger into it. what happens is that it stops being a debuggee, which
leaves it running exactly as it would have too

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

`--noreload` is still the answer for django today, and
[django templates](django-templates.md) says so. what changed is that a session
without it now says why nothing is binding

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
- **both file descriptors of the control connection**

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

that report is only true because of what the child then does, which is: give the
whole session up, before `os.fork()` has returned to python, and run exactly as
it would have if the program had never been launched under `bpd`

it is arranged with `os.register_at_fork(after_in_child=…)`, registered once from
the agent at attach. `os` is already imported — the entry point uses it to take
`bpd`'s own variables back out of `os.environ` — so this adds nothing to the
debuggee's `sys.modules`, and there is no python code in the handler: it is a
native function the interpreter holds a reference to

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
control connection is written to by the reader thread, which does not hold the
GIL, so even on a gil build a fork can land while its lock is held. on a
free-threaded build there is no GIL to argue from at all, and free-threaded
builds are a first-class target

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
- the stop registry is not read. a process that gave the session up has nothing
    to report about threads that did not survive the fork

### only the process that attached reports

the audit hook is inherited by a forked child too. it compares the pid against
the one recorded at attach and stays silent when they differ, so a child never
writes into the parent's socket

the consequence is stated rather than hidden: an `os.exec` performed **inside** a
forked child is not reported, and the `os.fork` that made it is. `os.spawnv` is
the shape that produces it

### what a forked child can tell, and the one thing that gives it away

nothing, in the child. `a_forked_child_sees_exactly_what_it_would_have_seen_without_the_debugger`
runs one program twice — once bare and once under `bpd`, with a breakpoint armed
on a line the child runs — and requires the two children's records of
`sys.monitoring.get_tool`, `get_events`, `get_local_events`, `sys.argv`,
`sys.path[0]`, `__name__` and `__file__` to be identical

the **parent** is a different matter, and it is a limit this page states rather
than leaves to be discovered. the agent reads the control connection on a thread
of its own, so a debuggee is multi-threaded where a bare run of the same program
is not — and since 3.12 cpython emits a `DeprecationWarning` on `os.fork()` in a
multi-threaded process. so a program that forks writes a line to stderr under
`bpd` that it does not write bare. that is not something this feature introduced
and not something it can take away: a stop holds one thread and leaves the rest
of the program running, so the connection has to be readable while a thread is
held, and that needs a thread which is not the held one

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
    during. this server writes nothing that is not an answer, so there is no
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

the environment and `sys.path` are untouched by any of this. this feature adds
no variable, no path entry and no module

the fork handler is the same shape: cpython exposes `os.register_at_fork` and
**nothing that enumerates what has been registered**, so a program cannot read
`bpd`'s handler out either. what it *can* observe is the warning above, which is
about the reader thread rather than about anything on this page

## what is not built

propagation, including for a fork. a forked child gives the session up and is
not handed a new one, which is the honest default rather than the final answer:
a fork inherits memory, so a child could be told how to reconnect without
anything going through the environment at all. that is a feature, and it is not
this one

the child is not debugged, and telling an exec'd one how to reach the session
means giving it something through its environment — which is the one channel the
parity guarantee currently keeps clean. what that would cost, and what the
guarantee would become, is designed rather than guessed at, and it is the rest of
this milestone
