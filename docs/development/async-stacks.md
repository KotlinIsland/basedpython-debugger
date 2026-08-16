# where an asyncio task was created

`await` preserves a stack. `create_task` severs it, and what is left does not say
who is responsible

```python
async def h():
    raise TypeError("Something broke in h")


async def g():
    asyncio.create_task(h())
```

**measured on 3.13, 3.14 and 3.15**: the traceback for that is *one frame* and
the process exits **0**, because an exception nobody retrieved is handed to a
loop handler rather than raised. a debugger that says nothing here is agreeing
that the program succeeded

## what the stack really looks like

measured, at a stop inside the task and at the scheduling:

```text
inside the scheduled task:  h <- Handle._run <- _run_once <- run_forever <- … <- <module>
inside the scheduler:       g <- Handle._run <- _run_once <- run_forever <- … <- <module>
```

they are identical below the top frame. `g` is not above `h` — it is *beside*
it, on an earlier turn of the loop, and the running frame's real caller is the
event loop

that is the whole design constraint. the frames that scheduled the work
**did not call** it, so putting them in the stack would be a call chain that
never happened

`a_stop_inside_a_task_says_where_the_task_was_created` in
`crates/bpd_engine/tests/async_stacks.rs` asserts both halves against a real
interpreter: that the record names the scheduler, and that the scheduler is
**not** among the frames

## so it is a separate list, and a different type

`Stack::scheduled_by` is its own field and is never spliced into `frames`. it
holds [`Scheduling`], which is deliberately **not** a `Frame`:

| `Frame`                             | `Scheduling`                                              |
| ----------------------------------- | --------------------------------------------------------- |
| something the interpreter has now   | a record of something that happened earlier               |
| carries a `FrameId`                 | carries nothing to ask about                              |
| every scope can be read and written | those frames have usually returned; their locals are gone |

giving them the same type would offer a client a frame id that resolves to
nothing and a variables request that answers about the wrong frame

- **DAP** — console `output` events beside the `stackTrace` response, one per
    frame. not among `stackFrames`: DAP's stack *is* a call chain and every
    client draws it as one, so a scheduling frame put there would be an editor
    showing a call that never happened
- **MCP** — `scheduled_by` on the `stack` answer, with a note saying what it is,
    beside `frames` rather than merged into them

## how it is captured, and the three routes that do not work

`BaseEventLoop.create_task` is a **python** function, so its own code object
takes local `PY_RETURN` events and the callback is handed what it returned — the
`Task` — with the stack that made it still on the thread. one event gives both
halves, and it costs nothing anywhere else

**that one function is on every route to a task**, which was measured rather than
assumed. `asyncio.create_task`, `asyncio.ensure_future`, `loop.create_task` and
a task group's own `create_task` were each driven with `PY_RETURN` armed on all
of them:

| route                   | what it reached                              |
| ----------------------- | -------------------------------------------- |
| `asyncio.create_task`   | `BaseEventLoop.create_task`, `create_task`   |
| `asyncio.ensure_future` | `BaseEventLoop.create_task`, `ensure_future` |
| `loop.create_task`      | `BaseEventLoop.create_task`                  |
| `TaskGroup.create_task` | `BaseEventLoop.create_task`                  |

so one hook covers all four. watching the outer functions as well would record a
stack per route and leave the innermost frame being whichever asyncio function
was used — a fact about asyncio rather than about the program

the routes that were measured and rejected, before this one was built:

- **`Task._source_traceback`** is exactly this record, and is `None` unless
    asyncio debug mode is on. bpd cannot turn that on: `loop.get_debug()` is
    something the program reads, and debug mode changes what the program does.
    that is the launch parity rule, not a preference
- **cpython 3.14's introspection** — `Task._asyncio_awaited_by` and
    `asyncio.tools.get_all_awaited_by` — is the **await** tree, which is who is
    waiting on a task *right now*. the case this feature exists for is a task
    nobody ever awaits, and it is `None` for exactly that task
- **importing anything from the callback.** the first version asked
    `sys.modules` and reached for the attribute, which meant calling the import
    machinery from inside a `PY_START`. measured: `KeyError: '__import__'` part
    way through `import asyncio`, taking the program down with it — a debugger
    that breaks every asyncio program is worse than one without this feature

so the code object is taken from the one place it can be had for free: it is
nested inside `asyncio/base_events.py`'s own module code object, which the agent
is handed when that module runs — *before* any task exists. it is a **method**,
so it sits two levels down, under the class body

## what the record is keyed on

the **task**, weakly.

the coroutine's frame is the same object at creation as at run — measured on
3.13, 3.14, 3.15 and 3.14t — and it is still the wrong key. a frame *address* is
not an identity here, which
`the_interpreter_hands_a_freed_frames_address_to_the_next_one` already records,
and holding the frame alive to keep one meaningful would mean holding every task
the program ever made

a weak key also answers what becomes of a record when its task is collected: it
goes with it, which is the truth — there is nothing left to ask about. the weak
reference is held beside the record rather than dropped, because a weak
reference that is itself collected never calls its callback, and the entry would
then outlive the task it describes

a stop finds its own task with `asyncio.current_task()`, which is a read on a
path that already runs python because a stop evaluates conditions

**and it is only asked when asyncio is already there.** reaching for a module
that is not loaded *imports* it, so the first version of this left `asyncio` in
`sys.modules` of a program that never imported one — measured, by having the
program write out what it saw after the stack was walked. bpd adding a module to
a program, running its body, is the launch parity rule broken well after the
launch

the guard is the hook itself: it is nested in `asyncio/base_events.py`, so
having one is evidence that module has already run. without one there is no task
to be in and nothing to ask

## where a record starts

at the program's own frame. every **leading** asyncio frame is dropped, which is
what makes one hook enough: `ensure_future` sits between the program and the
loop, and a task group puts two more there, so a record that began wherever the
callback fired would say the scheduler was asyncio

only the leading ones. what sits *below* the program's frame is the event loop
that was running it, and that is a true part of the stack the task was made on —
dropping it would be bpd editing a real stack because it found part of it
uninteresting

the rule is positional rather than a list of function names, so a route nobody
has thought of yet gets the same answer

## what is not covered

nothing is left of the creation routes — all four reach the watched function. the
case that still carries no record is a task made **before bpd was watching**,
which is a task created while `asyncio/base_events.py` was still being imported

**that is announced rather than left silent**, which is why `Stack` carries
`in_a_task` beside the record. an empty `scheduled_by` would otherwise mean two
different things:

| `in_a_task` | `scheduled_by` | what it means                                                                       |
| ----------- | -------------- | ----------------------------------------------------------------------------------- |
| `false`     | empty          | not in a task. there is nothing to say                                              |
| `true`      | empty          | in a task bpd did not see created. a limit of **bpd**, not a fact about the program |
| `true`      | frames         | where it was made                                                                   |

a client shown the same empty answer for the first two would read a gap in the
debugger as a fact about the program, which is the blind spot rule this project
already follows on 3.13. both front ends say the middle case in words
