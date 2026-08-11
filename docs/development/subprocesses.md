# child processes

`bpd` debugs one process. a program that starts another one has moved some of
its work somewhere the debugger is not, and until something says so the only
symptom is a breakpoint that never fires

that is what this page is about. `bpd` **notices** a python child and reports
it. it does not debug one yet, and it does not change anything about the child:
the program runs exactly as it would have

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

`sys.monitoring` state survives `fork()` completely: the child still holds the
tool id, its local events are unchanged, and callbacks fire in it. so a forked
child has the agent, armed, with the breakpoint table intact

it also inherits **the control connection's file descriptor**, and that is not a
detail. two processes writing length-prefixed frames into one socket
desynchronise it, and the engine turns that into "the peer sent a message this
build does not understand" — the debugger blaming its own protocol for the
program having forked

so a fork gets its own report, saying that the child is a copy of this process,
that it holds the agent's monitoring state and this session's connection, and
that `bpd` is not debugging it as a session of its own

### only the process that attached reports

the audit hook is inherited by a forked child too. it compares the pid against
the one recorded at attach and stays silent when they differ, so a child never
writes into the parent's socket

the consequence is stated rather than hidden: an `os.exec` performed **inside** a
forked child is not reported, and the `os.fork` that made it is. `os.spawnv` is
the shape that produces it

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

## what is not built

propagation. the child is not debugged, and telling it how to reach the session
means giving it something through its environment — which is the one channel the
parity guarantee currently keeps clean. what that would cost, and what the
guarantee would become, is designed rather than guessed at, and it is the rest of
this milestone
