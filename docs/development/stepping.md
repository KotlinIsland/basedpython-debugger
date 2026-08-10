# stepping, pausing, and exceptions

**a step steps one thread.** every other thread in the program keeps running
while it happens, which is the same model a stop has — see [threads](threads.md)

this page is about the three ways execution is controlled from a stop — step
over, step in, step out — plus the pause that reaches a program with nothing
held, and the two exception breakpoints. none of it has a command line surface
yet: it reaches a user through the adapters, which are not built

## a step follows a frame, not a code object

the thing a step is "in" is a **frame**, and a code object is not one. that
distinction is the whole of the correctness here, because every way of getting
it wrong looks right in a test that calls a function once:

| what re-enters a code object | what a step following the code object does |
| --- | --- |
| a recursive call | lands one level down, in a frame the user never asked about |
| a second generator built from the same function | lands in the other generator |
| a coroutine awaited from two places | lands in the wrong one |

so a step holds the frame object itself and compares identity. it holds a
**strong** reference to it, which is load bearing rather than tidy: cpython
hands the address of a frame it has freed straight back to the next one.
`the_interpreter_hands_a_freed_frames_address_to_the_next_one` measures exactly
that — two coroutines awaited one after another from the same function get the
same `id()` — so a step comparing addresses would read the second as the first
and be confidently wrong

## what leaves a frame, and what does not

`PY_RETURN` and `PY_UNWIND` **finish** a frame. `PY_YIELD` does not: it hands
control away, and the frame is resumed later still holding its locals and still
being where the step is

that is a decision, and it is the one that makes stepping through modern python
bearable:

- a step over an `await` lands on the **next line of the same coroutine**, not
    somewhere inside the event loop
- a step over a `yield` lands on the next line of the **same generator**, not in
    whatever consumed it
- a step out of a generator runs it to its **end**, because a generator that has
    yielded is not a generator that has returned

`a_step_over_a_yield_lands_on_the_next_line_of_the_same_generator` is the
acceptance, and it does not take the agent's word for which generator it landed
in: a second generator of the same function runs both of its first lines while
the first is suspended, and the test reads `tag` out of the frame it landed in

when a frame really is finished, the step moves to its caller and lands at the
caller's next line. that chains: a frame that returns immediately is followed
out of too, which is why stepping over the last statement of a function lands
where the call was made

when there is no caller `bpd` would report — the frame above the program's own
module is the `-c` the interpreter was entered through — the step has nowhere to
land. it is **given up**, and what the program does next is what the client is
told: `a_step_out_of_the_outermost_frame_lets_the_program_finish` gets
`Exited`, not a stop that never came

## comprehensions are not something to step into

since cpython 3.12 and PEP 709, list, dict and set comprehensions are **inlined**
into the function that contains them. there is no code object to step into and
no `<listcomp>` frame to land in, so stepping through one walks the enclosing
function's own lines. generator expressions and lambdas still have code objects
of their own

## what it costs

| step | what is armed |
| --- | --- |
| over | `LINE` and `PY_RETURN` on the code object of the frame, locally |
| in | the above, plus `PY_START`, `PY_RESUME` and `PY_THROW` for the program |
| out | `PY_RETURN` on the code object of the frame, locally |

plus `PY_UNWIND` for the program, for all three

two of those are not where the architecture doc said they would be, and the
reason is measured rather than assumed:

- **`PY_UNWIND` cannot be a local event.** `set_local_events` refuses it, along
    with `RAISE`, `RERAISE`, `EXCEPTION_HANDLED` and `PY_THROW`. so a step that
    needs to know its frame was left by an exception arms it for the whole
    program
- **`PY_START` is global because that is the only way it exists.** there is no
    local event for "some frame, somewhere, was entered", which is exactly what
    a step in is waiting for

### a step restarts the process's events

arming a step calls `sys.monitoring.restart_events()`

a line of the frame being stepped in may have run before and returned `DISABLE`,
which is precisely what a breakpoint in that function makes happen: the lines
around it disable themselves on the first pass. PEP 669 has no per-location
undo, so a step that did not restart everything would silently not be offered
the line it was waiting for and would land somewhere else —
`a_step_is_offered_a_line_an_earlier_pass_disabled` fails with `Exited` when the
restart is taken out, because the step never lands at all

there **is** a cheaper instrument and it is deliberately not used. taking a code
object's local events to zero and setting them again re-enables every location
in it, which `clearing_a_code_objects_local_events_undoes_its_disables` measures
in a bare interpreter. on a free-threaded build another thread can execute that
code object between the two calls and miss a breakpoint, and a missed breakpoint
is not a price this project pays for a faster step

### nothing disables a line while anything is stepping

`DISABLE` is **process wide**. a line reported on one thread and disabled there
is a line a step being made on another thread would never be offered again, so
while any step is armed anywhere the line callback stops answering `DISABLE` at
all. `a_step_is_offered_a_line_another_thread_would_have_disabled` holds a step
open — the call it steps over waits for a file — while a second thread runs the
same function throughout, and requires the step to land where it said

the same rule applies to `PY_START` while a step in is in flight, for the same
reason: a code object the interpreter has been told never to report again is one
a step in would never be offered, and a step in that was never offered the frame
it entered behaves exactly like a step over. **that guard cannot be made to fail
on a gil-enabled build**, because the window between arming a step and reaching
the call is a handful of bytecodes and no other thread runs in it. it is written
down here rather than left looking like coverage — see
[testing](testing.md#what-is-not-covered-yet)

## a breakpoint wins over a step

a breakpoint reached while a step is in flight reports itself as a breakpoint
and the step is taken off. the thread is held exactly where the step was going
to put it, so nothing is lost, and a breakpoint reported as a step landing would
be a breakpoint the client never saw fire

## pausing

`pause` is the one request made to a program with **nothing held**, and it is
the one that cannot say in advance which thread it will get

nothing in cpython suspends a thread. what there is, is an event: `LINE` armed
for the whole program, plus `restart_events()`, so that a thread going round a
loop inside a code object whose lines are already disabled is reachable at all.
the first thread to arrive is held as an ordinary stop, with frames and scopes
and everything else, and the pause disarms itself as it is taken

which thread that turns out to be belongs to the operating system, so the
acknowledgement says which threads were **running python** when the pause was
armed. an empty list is the useful answer: every thread is parked in a C call,
where no monitoring event exists, and nothing is going to arrive until one of
them comes back

the arming happens on a rust thread of the agent's own. the connection's reader
must not do it — taking the GIL there would stop it routing anything else for as
long as some thread holds the GIL in a C call, and a resume that could not be
delivered is the debugger hanging

## exception breakpoints

two settings, and they answer different questions because cpython only lets them
be different questions

### raised

stop where an exception is raised, caught or not. the frame that raised it is
the one held, so the stack is the whole of the program at the moment it went
wrong

**cpython raises the event again in every frame the exception propagates into**,
with the same exception object, as it looks for a handler. one `raise` two calls
deep produces three `RAISE` events, which
`the_interpreter_raises_an_exception_event_in_every_frame_it_passes_through`
measures in a bare interpreter. what `bpd` reports is the **first sighting** of
an exception on a thread, so one exception is one stop

the exception a thread last reported is held by a strong reference for as long
as it is the last one. a pointer would be cheaper and wrong, for the same reason
a step does not compare frame addresses

what follows from "first sighting", stated rather than discovered: re-raising an
object a thread has just been stopped for is a continuation of that exception as
far as `bpd` is concerned, not a new one

### uncaught

whether an exception will be caught is decided by what happens **after** it is
raised. a debugger that answered at the raise would be scanning exception tables
and predicting, and a wrong prediction here says "nothing will catch this" about
something a library catches a frame later

so it is answered where it is known: at the `PY_UNWIND` that takes the exception
out of a frame with no caller `bpd` would report. the cost of knowing rather than
guessing is that the frames it came through have already been popped — what is
left of them is the exception's own traceback, which is what the stop carries

`an_exception_caught_inside_a_library_is_not_an_uncaught_one` is the acceptance:
a `KeyError` raised and caught inside a library produces no stop, and the
`ValueError` that escapes `__main__` does

two things it does not claim:

- **an exception that escapes a `threading.Thread`'s target is not uncaught.**
    `threading` catches it in `_bootstrap_inner` and hands it to
    `threading.excepthook`, so it never unwinds out of the thread's outermost
    frame and there is no moment at which it is leaving the program.
    `an_exception_a_worker_thread_lets_escape_is_caught_by_threading_itself`
    measures that in a bare interpreter, so the limit is pinned rather than
    papered over. `raised` is what sees it
- **`SystemExit` is an exception and is reported as one.** `sys.exit()` raises,
    and it leaves the program. there is no list of types this quietly skips,
    because a debugger with a list of exceptions it does not mention is a
    debugger that is silent about the one you are looking for

the agent's own `SystemExit` — the one it raises out of its bootstrap frame to
report an exception the program did not catch — is **not** reported, because
that would be `bpd` stopping the program for a decision `bpd` had just made. it
is excluded as a frame in its own right rather than as a caller, which is the
same rule that keeps the bootstrap out of every stack

## how it is tested

`crates/bpd_engine/tests/stepping.rs` and
`crates/bpd_engine/tests/exceptions.rs`, against a real interpreter

nothing in either takes the agent's word for where the program got to. every
call a step might or might not enter writes a marker file as it goes, and each
landing is checked against which markers exist: a step over asserts the callee's
marker **is** there and a step in asserts it is **not**, which is the same claim
from both sides
