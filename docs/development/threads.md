# threads

**a stop holds one thread. every other thread in the program keeps running.**

gdb calls this non-stop mode and DAP exposes it as
`supportsSingleThreadExecutionRequests`. it is `bpd`'s default, because a live
server should go on serving while one of its handlers is inspected

stop-the-world is available as an explicit mode, because a coherent view of a
data structure needs it

this page is mostly about the *cost* of that choice, because the cost is real
and a debugger that did not report it would be lying by omission

## it is the same on every build

the agent **releases the GIL for the duration of a stop**. it takes it back only
to answer a request, and gives it up again

without that, the GIL would be the thing deciding the threading behaviour: on a
gil-enabled build the held thread would keep the GIL and freeze the whole
process by accident, and on a free-threaded build it would not. "threads keep
running, except on the interpreter most people have" is a capability ladder,
which this project refuses everywhere else

`another_thread_goes_on_running_while_one_thread_is_held` is the acceptance, and
it does not ask the agent anything: a worker thread is told to go **while the
main thread is held**, and the test waits for a file that only a thread running
python could have written

## concurrent stops queue, and each is a stop of its own

a second thread reaching a breakpoint reports its stop immediately, and both
threads are held at once. it does not wait for the first to be resumed — a
thread waiting for the debugger to finish with another thread is a thread that
is not running, and nothing would have said so

that needs the connection to be readable while several threads are held, so a
rust thread of the agent's owns the reading end and routes each request to the
thread it names. it never takes the GIL: **every answer is computed on the
python thread the question is about**, because an expression evaluated anywhere
else would run the program's code on the wrong thread and report another
thread's `threading.current_thread()`

a request names its thread through the stop it belongs to:

| request                              | addressed by                                           |
| ------------------------------------ | ------------------------------------------------------ |
| stack                                | the stop number                                        |
| variables, evaluate, set variable    | the frame id, which carries the stop                   |
| resume                               | the thread identities, or all of them                  |
| breakpoints, threads, stop the world | the process, answered on the lowest-numbered held stop |

a stop that has ended is **refused**, naming the stops that are held now. a
resume that names a thread bpd is not holding is refused too, and refused
**entirely**: a resume that half happened would leave the client's idea of what
is running different from the agent's, with nothing saying which is right

## the held thread still holds its locks

this is the one that will be reported as a bug in `bpd`, so it is stated first
and plainly

stop a thread inside `with lock:`, inside `logging`, or mid-import, and every
other thread that wants the same lock piles up behind it. "all threads keep
running" quietly becomes "all threads are piled up behind the debugger", and
from the outside it looks exactly like `bpd` hanging when it is the debuggee
deadlocked on itself

what `bpd` does about it is split in two, because what is knowable is split in
two

### what is knowable: the import system

cpython's import machinery runs in **python frames**, whose filenames name it —
`<frozen importlib._bootstrap>` and `<frozen importlib._bootstrap_external>`. so
a stop inside an import is detectable by walking the held stack, and the stop
carries it:

```text
the import system, importing `app.db` — another thread importing it blocks
until this one is resumed
```

the module name comes from the machinery's own `_find_and_load` frame. the
filenames are not taken on trust: `the_import_machinery_runs_in_frames_named_
after_itself` pins them in a **bare** interpreter, so a rename in cpython fails
a test rather than silently turning the detection off

### what is not knowable: every other lock

**cpython exposes no owner for a lock.** `_thread.lock` records nothing about
who took it, there is no registry of locks, and `threading` keeps no map from a
lock to a thread. so "thread 7 is waiting for a lock thread 3 holds" is not
something `bpd` can know, and it is not claimed. a stop that took a
`threading.Lock` reports `holding` as **empty**, and
`a_thread_piled_up_behind_a_lock_the_held_thread_took_is_reported_as_getting_
nowhere` asserts exactly that, so the limit is pinned rather than papered over

a heuristic was available and was rejected: looking through the held frames'
locals for a `_thread.lock` whose `locked()` is true would find a lock this
thread never took as readily as one it did. "probably right" is the thing this
project does not ship

### what is observable: a thread that is getting nowhere

so the general half is reported from the other end. **ask what the threads are
doing**, and each one comes back with where it was and whether it moved:

```text
thread 6153441280   held by stop 3   app/db.py:41 in connect
thread 6154014720   running          app/db.py:52 in query      still
thread 6154588160   running          app/web.py:18 in serve     moved
```

a thread is `still` when it was in the **same frame at the same bytecode offset**
in two samples a stated interval apart. the offset rather than the line, because
a thread going round a one-line loop is on the same line every time it is looked
at, and calling that still would report a busy thread as a stuck one

`still` is a symptom and is documented as one. a thread blocked in `sock.recv`
and a thread piled up behind a lock the held thread took look identical from
here. it says where to look, not what is wrong — and the interval it was
measured over comes back in the answer, so "still" never means something the
caller did not ask for

## a read is a sample, not a snapshot

while the world moves, reading a container's length and then its contents can
describe a state the program was never in. so **every read carries the mode it
was taken in**, the same rule already applied to
[a timed out control call](agent-interface.md)

| mode             | what it says                                                  |
| ---------------- | ------------------------------------------------------------- |
| `non_stop`       | one thread was held and the rest of the program kept running  |
| `stop_the_world` | everything that could be held was, and here is what could not |

one thing *is* a snapshot in either mode: **the held thread's own stack**. it is
inside a monitoring callback and cannot return, so its frames cannot be torn
down underneath a walk. what the mode qualifies is everything the frames point
*at*

## stopping the world

there is nothing in cpython that suspends a thread. what there is, is an event:
`LINE` armed for the whole program, so every thread executing python reaches a
callback, and the callback does not return until the world is released

it is not free, and the cost is stated rather than hidden. arming it means
calling `restart_events()`, which undoes **every** `DISABLE` in the process —
the performance model of this debugger, reset. the program pays it back as those
lines disable themselves again afterwards

the world is released when the stop that asked for it is resumed. two held
threads can both ask, and it is released when the last of them goes

### a thread in a C call cannot be stopped, and is never counted as held

a thread parked in `lock.acquire()`, `sock.recv()` or `time.sleep()` has already
released the GIL and executes no python. it reaches no monitoring event, and
nothing available here can stop it

it is **running**, and it is reported as running in native code:

```text
stop-the-world, except for 1 thread(s) parked in a C call that nothing here
can stop: [6154588160]
```

an answer whose `native` list is empty is the only one that describes a single
moment of the whole program. `stopping_the_world_holds_what_it_can_and_never_
counts_a_native_thread_as_held` puts one thread in a busy python loop and one
blocked on a lock, and requires the first to be held and the second to be named
as running

the list is fixed when the world is stopped rather than recomputed per answer,
so it can name a thread that has parked since. overstating what was moving is
the safe direction to be wrong in

while the world is stopped, a thread that returns from a C call and reaches a
line parks there — including a thread that was just resumed. a line that holds a
breakpoint is still handled as a breakpoint first, because a breakpoint that
silently did not fire is worse than a thread that was held for the wrong reason

## the program can end while a thread is held

the interpreter finalizes by joining the program's non-daemon threads. a held
thread cannot be joined, so a program whose last statement runs while bpd is
holding a worker will sit there — and that looks exactly like a hang in `bpd`

so it is said rather than left to be discovered. when the program reaches its
end with threads still held, the agent reports which ones, before finalization
starts. resuming them is what lets the process finish

one thing that sounds like a hazard here is not one: **the frames of a held
thread cannot be torn down underneath an inspection**. the thread is inside a
monitoring callback and cannot return, so no frame of its stack can be popped
while it is held

what is **not** covered is a held **daemon** thread. cpython does not join those
at finalization; it terminates them at the point they next try to take the GIL,
which for a held thread is the moment it is resumed. `bpd` reports the thread as
held right up to that point, which is true, and it does not claim to know what
becomes of it afterwards. there is no test for it here, and there is no code
pretending to handle it

## what is not built

- **a thread bpd is not holding has no stack request.** its frames are moving,
    and a stack read off one would be a description of a moment that had already
    gone. where it is, stated as the sample it is, is the thread census
- **there is no cli surface.** the thread model reaches a user through the
    adapters, which are not built. the capability is `Debuggee::held`,
    `Debuggee::resume`, `Debuggee::threads` and `Debuggee::stop_the_world`

## stepping is one thread's, and it is built

a step steps the thread its stop is holding, and every other thread goes on
running underneath it. the interaction that needed care is `DISABLE`: it is
process wide, so a line reported and disabled on one thread is a line a step on
another would never be offered again. while any step is armed anywhere, nothing
disables a line — and `a_step_is_offered_a_line_another_thread_would_have_
disabled` holds a step open while a second thread runs the same function
throughout

a **pause** is the one request made with nothing held at all, and it holds one
thread like every other stop: whichever reaches a line first. see
[stepping](stepping.md)

## how it is tested

`crates/bpd_engine/tests/threads.rs`, against a real interpreter, with
multi-threaded fixtures

the threads coordinate through **files** rather than through timing, so a slow
machine makes a test slower and never makes it flaky: a worker waits for a file
the test writes, and the test waits for a file the worker writes. nothing takes
the agent's word for anything — the proof that a thread is running is a file it
wrote while another thread was held, and the proof that a thread is held is a
file that did not appear
