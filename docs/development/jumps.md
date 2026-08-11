# set next statement, and restart frame

two operations that move a frame's instruction pointer. **set next statement**
moves the frame a thread is executing to another line of the code it is running;
**restart frame** moves it to the first line of its own code object, so the frame
runs again from the top

they are the same act on the interpreter — an assignment to `frame.f_lineno` —
and they differ only in where the line comes from. neither resumes anything: the
thread is still held afterwards, at the line it moved to

## it works from a `sys.monitoring` callback, with no trace function

this was written down as the open question of the milestone, on the grounds that
cpython permits assigning to `f_lineno` only from inside a trace function and
`bpd` does not install one. that is the `settrace` era's requirement and not this
one. measured on 3.13, 3.14 and 3.15, from a `LINE` callback with
`sys.gettrace()` returning `None` throughout:

- a forward jump takes effect
- a backward jump takes effect and the block genuinely re-executes — jumping from
    the third statement of a three-statement body to the first runs
    `A, B, A, B, C`
- an illegal one raises `ValueError` with a reason already in it

pydevd sets `f_trace` first, which is why reading it suggests otherwise

## where the program is afterwards is derived, never waited for

**no `LINE` event is delivered for the line a jump moves to.** in the run above
the events were `A, B, C, B, C`: the destination really is where the frame is and
it really does run, and the event for it is simply not sent

so the position in the answer is read **off the frame** after the assignment.
a debugger that waited to be told would report the line *after* the one it moved
to, or wait forever for an event that is not coming. that is the single most
likely way to get this feature wrong, and
`a_backward_jump_re_executes_the_lines_between_and_says_where_the_frame_is` is
what fails if it is ever "simplified" into waiting

the same fact has a second consequence, and it is reported rather than left to be
discovered: **a breakpoint on the line a jump moves to does not fire for the pass
the jump lands in**. it is not offered the destination's own execution of that
line. the breakpoint is still set and fires the next time the line runs, and the
answer names it — `unannounced` in MCP, an `output` event in DAP.
`a_breakpoint_on_the_line_a_jump_moves_to_is_named_as_one_that_will_not_fire`
pins both halves

## what a jump does to the frame besides moving it

cpython binds **every unbound local of the frame to `None`** as part of the jump,
and warns that it did — `RuntimeWarning: assigning None to 2 unbound locals`.
that is a change to the program's own state, made because the debugger was asked
to move, and nothing else in a session would ever mention it. the names are read
back **out of the frame** afterwards rather than predicted from the warning, and
reported as `bound_to_none`

a program whose warning filters make `RuntimeWarning` an error is a program where
that assignment **raises**, and cpython leaves the frame where it was. that comes
back as a refusal carrying the exception, like any other

## what a jump does not do

it does not run the cleanup of a block it leaves:

- jumping out of a `with` body does **not** call `__exit__`
- jumping out of a `try` does **not** run its `finally`

cpython accepts both — neither is a refusal it makes — so what is on the other
side of such a jump is a program with a context manager still open. `bpd` does
not pretend otherwise and does not undo it. the lines a jump skips were not
executed, and the effects they would have had did not happen

## which frame can move

**only the frame the thread is executing.** every frame below it is suspended in
a call

cpython does *not* refuse a move in one of those, which is the whole reason `bpd`
does. measured on 3.13, 3.14 and 3.15: assigning to a suspended frame's
`f_lineno` is accepted, and the frame then runs on with a value stack that no
longer matches where it is — a function jumped in this way returned a value it
never computed. so a request naming a deeper frame is refused, with that reason
and with the frame that can move

the refusal also answers the question DAP's `restartFrame` implies, because
somebody will ask it: making a deeper frame the executing one would mean
discarding the frames above it, and there is no mechanism for that.
`frame.clear()` answers `RuntimeError: cannot clear an executing frame`, no
public C API pops a frame, and the only remaining route — making each intervening
frame return — runs its `finally` and `except` blocks on the way out, which is a
**different operation** and would have to be described as one. pydevd advertises
`supportsRestartFrame: false` for the same reason

a django template frame cannot be moved either. it is synthesised over the
`Node.render_annotated` frame that renders it and the interpreter has no frame
for it, so there is no instruction pointer to move

## what restart frame re-enters with

**what the parameters hold now.** a parameter the frame has already assigned to
holds the new value, and the original is gone

that is a decision rather than an oversight, and the alternative was priced:
capturing a call's arguments would mean materialising a frame and copying every
argument of **every call in the process**, on the event path, for an operation
almost nobody makes. `PY_START` is disabled per code object after its first
sighting precisely so that discovery costs one native call per code object rather
than one per call — capturing arguments would mean never disabling it. the
architecture is fast because of what it does not do per event, and this would be
paid by every program to serve a keystroke

so nothing in `bpd` says restart frame restores the original arguments, because
it does not. `restarting_a_frame_runs_it_again_with_what_its_parameters_hold_now`
is the acceptance: a function that adds 100 to its own parameter and is then
restarted returns the answer for the **new** value

side effects the frame already performed are not undone either. nothing here can
undo them, and a debugger that implied otherwise would be inviting a belief about
the program that is false

## where a restart lands

the line of the **first instruction of the code object that carries one**, in
offset order — not `co_firstlineno`, and the two really do differ:

| code object                        | its first instruction              |
| ---------------------------------- | ---------------------------------- |
| a module                           | a `RESUME` whose line is `0`       |
| a function that closes over a name | `MAKE_CELL`, which carries no line |
| a function with decorators         | the first **decorator** line       |

`co_firstlineno` is where the code was written; what a jump needs is a position
the frame can be put at. a code object with no such line at all is refused rather
than moved to a line nothing said was there

no `LINE` event is delivered for that line either — it is the `def` — so a
restart is followed by a step landing on the **first statement of the body**,
which has not run yet. `a_restart_lands_before_the_first_statement_and_a_step_runs_it`

## the frames restart frame refuses

a **generator, coroutine or async generator** frame. the first instruction of
such a code object is the `RESUME` that `send`, `throw` and `await` enter at,
rather than the top of the body — so moving there is not "run it again".
measured on 3.13, 3.14 and 3.15: a generator restarted this way is **over**, and
the very next `next()` raises `StopIteration` without it having yielded anything

set next statement to a line of the body is the operation that works there, and
it works normally: a backward jump inside a generator body re-executes it and the
generator goes on yielding. the refusal names that alternative rather than only
saying no

## stepping and breakpoints across a jump

- a **step** cannot be armed while a jump is being made: the thread is either
    held, and can be moved, or stepping, and is not held. a step asked for after
    a move is armed from where the frame is **now**, so it executes the
    destination and lands on the line after it —
    `a_step_after_a_jump_runs_the_destination_and_lands_on_the_line_after_it`
- a **breakpoint on a line the jump skipped** does not fire, because the line was
    not executed. that is what was asked for
- a **breakpoint on a line the jump goes back over** fires normally when the line
    runs again
- a **breakpoint on the destination** is the one exception, described above: it
    is passed over exactly once, and the answer says so

## what cpython refuses, in its own words

a refusal is cpython's and is passed through with its reason intact rather than
being rewritten into something vaguer or caught into a `false`:

| what was asked                 | what comes back                                         |
| ------------------------------ | ------------------------------------------------------- |
| into the body of a `for`       | `can't jump into the body of a for loop`                |
| a line outside the code object | `line 1 comes before the current code block`            |
| from the entry stop            | `can't jump from the 'call' trace event of a new frame` |
| from an exception stop         | `can only jump from a 'line' trace event`               |

the last two are worth stating, because they are stops `bpd` produces routinely:
the entry stop is reported from `PY_START` and a raised or uncaught exception
from `RAISE` or `PY_UNWIND`, and cpython permits a move from none of them. the
frame does not move, and the position in the answer is read off the frame the
same way a successful move's is

## reaching it

|                    | DAP                        | MCP                  |
| ------------------ | -------------------------- | -------------------- |
| set next statement | `gotoTargets`, then `goto` | `set_next_statement` |
| restart frame      | `restartFrame`             | `restart_frame`      |

DAP's `goto` carries a **target id** rather than a line, and `gotoTargets` is
where one comes from. that round trip is the protocol's mechanics rather than a
capability of the core, and it earns its keep: a target is minted only when the
location the client asked about is in the file a held thread is **executing**. a
line number means nothing without its file, and cpython would take the same
number against whatever file the frame happens to be running

offering a target is not a claim that the move will happen — whether a line can
be reached from where the frame is is cpython's answer, given when the move is
made, and the target's label says so

both requests are answered and then followed by a `stopped` event, with reason
`goto` or `restart`, because the thread was never resumed and the client has to
re-read the stack to see where it is. that event repeats what the stop was
announced with about the other threads rather than recomputing it: nothing about
them changed, and a second event saying otherwise would contradict the first
