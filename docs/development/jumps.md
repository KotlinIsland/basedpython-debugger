# set next statement, and restart frame

two operations, and they are **not** the same act any more

- **set next statement** moves the frame a thread is executing to another line of
    the code it is running. one assignment to `frame.f_lineno`, and nothing is
    resumed: the thread is still held afterwards, at the line it moved to
- **restart frame** runs a frame again. it does that by forcing the frame to
    **return** and then rewinding its **caller** to the line the call was made
    from, so the interpreter builds a frame that has never run. it uses the jump
    primitive twice and it **resumes the thread**, because a caller has to
    actually execute a call for a frame to exist

everything down to [what a restart really is](#what-a-restart-really-is) is about
the jump, and is true of both — a restart is made of two of them

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
`frame.clear()` answers `RuntimeError: cannot clear an executing frame`, and no
public C API pops a frame

what is left is making each intervening frame **return**, which is a different
operation and would have to be described as one. [restart frame](#what-a-restart-really-is)
does the nearest thing for **one** frame — and note that it does not get the
`finally` and `except` blocks either, because the way it makes a frame return is
a jump. it is not a way to reach a deeper frame at all: it needs the caller's
line to be the thing that built the frame. pydevd advertises
`supportsRestartFrame: false` rather than answering either question

a django template frame cannot be moved either. it is synthesised over the
`Node.render_annotated` frame that renders it and the interpreter has no frame
for it, so there is no instruction pointer to move

## the two mechanisms, and which one is tried first

a restart can be done two ways and they have almost nothing in common.
**running the frame again where it stands** never touches the caller.
**rewinding the caller to the call** never touches the frame's locals. one keeps
the frame's identity, the other keeps the caller's line unexecuted, and no single
answer is right for both — so the request carries which is wanted:

| `again`                | what it does                                |
| ---------------------- | ------------------------------------------- |
| `either` (the default) | the reset, falling back to the rewind       |
| `in_place`             | the reset, or the reason it cannot be done  |
| `through_the_caller`   | the rewind, or the reason it cannot be done |

a refusal names the ways that were **tried**, and only those. a user who asked to
rewind the caller is not told about the frame's cell variables, because cell
variables were never what stood in their way

the reset is what `either` prefers, because of what it does *not* do. the rewind
re-executes the caller's whole line, so every question about whether a restart is
safe is a question about that line — and most of the refusals below exist only
because of it. reset the frame instead and the caller stays suspended in its
`CALL`, so the line is never re-entered and none of those questions arise:

| the caller's line             | rewound                               | reset in place                  |
| ----------------------------- | ------------------------------------- | ------------------------------- |
| `x = f(f2())`                 | refused — `f2` would run twice        | restarted, `f2` runs once       |
| `x = f(obj.attr)`             | refused — a property would run twice  | restarted, the getter runs once |
| `sorted(items, key=f)`        | refused — no line event in a C caller | restarted                       |
| `x = f(a)` inside a `finally` | refused — bpd cannot say which copy   | restarted                       |
| `f()` as the last statement   | refused — nothing runs after the call | restarted                       |

what the reset costs instead is a much smaller list, and it is about the frame
rather than about its caller — see `Unresettable`. the two that matter:

- a frame that **writes over one of its own parameters** cannot be reset. the
    `CALL` moved the caller's operands into the parameter slots, so those slots are
    the only place what the call passed still exists, and a frame that has written
    over one has lost it. this is the case the rewind still serves, because
    rewinding re-evaluates the arguments
- a frame that **closes over names of its own** cannot be reset, because
    `MAKE_CELL` is unreachable — see below

### it is a jump, and then the part a jump cannot do

the reset is `frame.f_lineno = co_firstlineno`, which is ordinary and supported:
cpython pops the operand stack, closes what it pops, runs the block cleanup the
jump implies, and sets the instruction pointer

what it will not do is **unbind a local**. a jump binds every unbound local of
the frame to `None`, so a frame sent back to the top of its own body starts with
names bound that a frame the interpreter had just built would not have:

```py
def f(arg):
    if arg == "never":
        cond = 1
    print(cond)  # a fresh call raises UnboundLocalError
```

a frame reset by a jump alone prints `None` there. that is a false belief about
the program, produced by the debugger, with nothing downstream to catch it — so
the reset is the jump **and** a second pass that puts every slot a fresh call
would not have bound back to empty

the order is fixed and is not an implementation detail: the jump binds them, so
the pass has to come after it. a pass made first would clear exactly the slots
the jump is about to fill

### why the layout is measured rather than written down

that second pass reaches `_PyInterpreterFrame`, which is internal and moves.
`localsplus` is word 9 on 3.13, which has an `int stacktop`, and word 10 on 3.14
and 3.15, which have a `_PyStackRef *stackpointer` instead. `f_frame` is word 3
of `PyFrameObject` under the gil and word 5 without it, because a free-threaded
`PyObject_HEAD` is four words rather than two

that is a per-version, per-build table, and this project does not hand-maintain
one: a table is right until an interpreter it was not written for loads it, and
then it is silently wrong about somebody's memory

so nothing is declared. every field is found by **matching a value bpd already
knows**, against a probe it compiles itself — a generator, because a suspended
generator keeps its locals and its frame data lives *inside the generator
object*, so every read the calibration makes is bounded by an object bpd is
holding and has asked the size of. the probe's three locals are the signature:
one owned reference, one immortal, and one the interpreter never binds at all

|              | `f_frame` | `localsplus` | `PyStackRef_NULL` |
| ------------ | --------- | ------------ | ----------------- |
| 3.13         | 3         | 9            | `0`               |
| 3.14, 3.15   | 3         | 10           | `1`               |
| 3.14t, 3.15t | 5         | 10           | `1`               |

**one match or none.** two candidates is not a tie to be broken by preferring the
lower — it is bpd not knowing which word is the field, and the next thing it
would do with that is write. a build that does not match uniquely is one the
reset refuses on

which slots own what they hold needs no table and is argued rather than measured:
a pointer to a `PyObject` is at least eight-aligned, so its low two bits are
clear, and every tagged form cpython puts in a slot sets one of them —
`Py_TAG_REFCNT` and `Py_TAG_DEFERRED` are both `1`, `Py_INT_TAG` is `3`,
`Py_TAG_INVALID` is `2`. so a slot owns a reference exactly when `bits & 3 == 0`,
and a tagged slot is left alone, which is what `PyStackRef_CLOSE` does with one

### the door there is no handle on

`MAKE_CELL` — and `COPY_FREE_VARS` for a closure — sits before `RESUME`, in a
`co_lines` range carrying **no line at all**. measured as `(0, 2, None)` on 3.13,
3.14 and 3.15. `f_lineno` chooses among marks and a range with no line has none,
so offset 0 is not somewhere a frame can be sent: a reset lands on the `RESUME`
at offset 2, and the prologue never re-runs

free variables are unaffected — a fresh frame is handed the very same cells by
`COPY_FREE_VARS` rather than making new ones, so keeping those slots is exactly
right. **cell** variables are not: a fresh call makes new cells, and a closure
the first pass created is still holding the old ones. a reset frame sharing them
would let that escaped closure see the second pass's writes, which is a program
behaviour the program does not have. so a code object with cell variables is
refused, and the reason names the instruction rather than the symptom

### it runs no block cleanup either

sending the frame back to its first line is an `f_lineno` jump. cpython pops the
operand stack and closes what it pops, which is **not** running a block's
cleanup: a `with` the frame was inside gets no `__exit__` and a `try` gets no
`finally`, and the body then re-enters that block from the top — measured, two
`__enter__` against one `__exit__`, so the first context manager is still open

`Reset::inside_a_block` says when this happened, and both front ends print it. it
is the one thing a reset does that a call made afresh never would, so it is said
rather than left to be discovered

running the cleanup first is a chain of jumps through the compiler's own
`__exit__` path — jump to the innermost cleanup, regain control at the next line
boundary, repeat outward — and it is not built yet

### what the reset does not claim

**the frame object is the same object.** a fresh call makes a new frame and this
does not, so `id(frame)` is unchanged and anything the program holds it by still
holds it. both front ends say so on every reset, because a client that believed
it had a new frame would be wrong about the program

### resetting a frame that is not the one executing

a frame with live frames above it cannot be reset while they are there — cpython
**crashes** rather than refuses when a frame that is not executing is moved,
measured on 3.13, 3.14 and 3.15. so they go first, innermost outward, each moved
to a point in its own code where a return's value is loaded, exactly the way a
one-frame restart forces its own frame out

that makes it the one reset that **lets the thread go**: a frame leaves by
returning, and returning is the interpreter running. so it answers
`Restarted::Unwinding` and the reset itself arrives later, as
`StopReason::FrameReset` — or as `RestartAbandoned` if the target left before the
unwinding reached it

```
outer:3  middle:3  deepest:3   held here, `outer` is asked for
outer:3  middle:3              `deepest` forced to return
outer:3                        `middle` forced to return
outer:1                        `outer` reset, and this is the stop
outer:3  middle:3  deepest:3   and on, into frames the interpreter built
```

#### what runs between the links

**this is the cost, and it is the reason the chain is decided before it starts.**
when a frame returns, the rest of the line that called it runs — with the value
the forced return produced, which is a value the program never computed — before
any event reaches bpd. there is no event between the two, so bpd cannot be there
for it

where that remainder is loads and stores into the frame's own locals, nothing
observable happened: the frame is being discarded, and the target's own locals
are unbound by the reset anyway. where it calls something, or writes a global, a
cell or a name, the program has done something it would never have done

so the same allow list that decides a rewind decides this, asked once per frame
in the chain instead of once — minus the three stores that write outside the
frame, which a rewind may permit because it separately reasons about what the
restarted call reads, and an unwind may not because the write outlives the frame
that made it. `Unresettable::ATailWouldRun` names the frame and the instruction,
and it refuses the **whole** request: a chain that stopped half way would have
destroyed frames for a reset that then did not happen

two more fall out of the same walk. a frame whose line ends in a return leaves on
its own and needs no forcing; one that does not have to be forced, which takes a
clean exit, and `AFrameAboveHasNoCleanExit` is a frame that has none. and the
**target** must reach another line after its call returns — `return helper()` is
`NoLineFollowsTheCall`, because a frame that returns as soon as the one above it
does is never executing again, so there is no moment at which it could be reset

## what a restart really is

**the frame it names does not move.** it is moved to the point in its own code
where a return's value is loaded, so the loads and the return are all that runs
and it **returns** — and then the caller is rewound to the line the call was made
from, so the caller makes the call again and the interpreter builds a frame that
has never run

```
caller:9  →  f:3 f:4 f:5   the call, and the frame forced out at f:5
caller:10 →                the caller's next line, where the rewind is made
caller:9  →  f:3 f:4 f:5 f:6   the call again, into a frame with fresh locals
caller:10                  and on, with the answer the program would have had
```

two things fall out of that, and they are the whole reason it replaced a jump to
the frame's own first line:

- **the locals are the ones a call binds.** a function that adds 100 to its
    parameter and is restarted gets the parameter the *call* passes, not the
    value it had already written.
    `a_restarted_frame_runs_again_with_locals_the_call_bound_and_not_the_ones_it_had`
    asserts on what the program itself recorded, because bpd reporting a fresh
    frame and the frame being fresh are different claims
- and **the cleanup still does not run.** forcing the frame out is an
    `f_lineno` jump, so [what a jump does not do](#what-a-jump-does-not-do)
    applies to it unchanged. measured on 3.13 and 3.14 with a plain class
    context manager: two `__enter__` and **one** `__exit__`; with a bare
    `try/finally`: the body twice and the `finally` once. so a context manager
    the frame had open is still open, and the restarted call opens a second one

    this page, the commit that introduced the mechanism, and four other places
    claimed the opposite, on a measurement that used a
    `@contextlib.contextmanager` fixture — whose `finally` runs when the
    generator is **collected** rather than when the block is left, which is not
    the jump doing it. `a_context_manager_open_across_a_restart_is_not_exited`
    is the plain class version and pins what the jump really does

    but the collection is not unrelated either, and saying so was the next thing
    to get wrong. the forced-out frame **dies**, and anything it was the last
    holder of is finalised right then — a `__del__`, or the `GeneratorExit`
    thrown into a suspended generator, which runs its `finally` and the
    `__exit__` of any `with` inside it. a `@contextlib.contextmanager` is that
    shape, so its cleanup does run, at a point the program never reached. the
    jump runs no **block** cleanup; the death runs whatever a finaliser does,
    and `Restarting::told()` says so rather than enumerating it

it **resumes the thread**. the answer says what was arranged, and where it got to
arrives as a stop of its own: `restarted` at the first line of the fresh frame,
or `restart_abandoned` carrying which of its reasons it was. that list is
`non_exhaustive` and this sentence deliberately does not enumerate it — an
earlier version named one of three causes, and a client reading it as a complete
set would be reading a false one. a third thing can also happen: another stop
takes the restart off, and neither arrives

what it still does **not** do is undo anything. side effects the old frame
performed are performed, and the frames it called are gone

## what the tail writes where the callee can see it

`disturbed` and the read guard both reason about names **on the caller's line**.
Neither sees state the *callee* reads that the tail writes and the line never
reads back:

```python
G = 1


def reads_it():
    return G + 0


def caller():
    global G
    got, G = reads_it(), 99  # the restarted call reads 99, not 1
```

so a tail `STORE_GLOBAL` or `STORE_DEREF` is refused as `tail_writes_shared_state`
— 143 of 7401 permitted call sites on 3.13 and 150 of 7520 on 3.14. bpd would
have to read the callee's code, and everything it calls, to do better.

### what this costs, and why it is paid

A **module body**'s namespace *is* its globals, so a top-level `kept = f(1)`
writes a global too, through `STORE_NAME` — and if `f` reads `kept`, the
restarted call reads what bpd stored. Same hazard, so it is refused the same
way, which `Namespaces::locals_are_globals` decides: true of a module body and
of nothing else, because a function's locals are slots and a class body's
namespace is its own.

| what the tail writes                              | 3.13        | 3.14        |
| ------------------------------------------------- | ----------- | ----------- |
| a global or a cell, of all permitted sites        | 143 of 7401 | 150 of 7520 |
| any name, of permitted sites in a **module body** | 520 of 1128 | 537 of 1156 |

The second row wants the module-body denominator rather than the global one. An
earlier version of this page used the global one and concluded "very nearly every
module-level restart" — asserted rather than measured, and wrong by about a
factor of two: **608 module-level sites on 3.13 and 619 on 3.14 still restart**,
a majority.

The module-body case is refused rather than excused because the alternative was
to refuse `got, G = f(), 99` and allow `kept = f(1)` on the grounds that the
second is *unlikely* to be read — treating the two on likelihood rather than on
kind, which this project does not take as a justification.

**What still restarts from a module body:** any line that stores nothing *after*
the call — a bare call statement, one whose result is discarded, a decorator, and
`f(w := 3)`, whose store lands before the call. Pinned by
`a_module_body_call_that_stores_nothing_still_restarts`.

**What is not covered, and is a real limit rather than a safe one.** The refusal
is about *where* the tail writes, not about whether the callee reads it. bpd
does not read the callee's code, so:

- a restart is refused for a global the callee never touches — an over-refusal,
    fail-closed, and the message says bpd does not read the callee to find out
- a class body's `STORE_NAME` is allowed, and it is genuinely not the globals —
    but a callee reading the *class* namespace through a closure over it is not
    something this analysis models
- nothing here reaches state written through an attribute or a subscript;
    `STORE_ATTR` and `STORE_SUBSCR` are off the allow list entirely, so a line
    carrying one is refused before this question is asked

Closing the first of those would need the callee's `co_names`, and everything it
transitively calls, which is why it is not attempted.

## which of the caller's names the forced return lands in

the answer is a **walk**, not a list of the stores after the call. the call
leaves one value on the stack and the analysis follows the rest of the line to
see which stores consume that value — because naming every store is right for
`x = f()` and wrong for ordinary lines beside it:

- `a = f(); b = spare` fuses into `STORE_FAST_LOAD_FAST ('a', 'spare')`, whose
    two names are different operations: it **writes** `a` and **reads** `spare`
- `a, b = f(), spare` stores in the opposite order to the one it is written in,
    so the name that gets the call's value is not the first
- `box = [f(), spare]` puts the value in a container, and the container is
    reachable by the name stored right after it

the stack is modelled from the call's own value down. what the caller pushed
before the call is not modelled and does not need to be — none of it came from
the call, so a pop reaching past that floor consumes a value the program
computed itself. the one shape with no answer is a `SWAP` that would put the
call's value below the floor, and that is `span_not_understood`, which says it
is a gap in `bpd` rather than something to change about the program. no
permitted call site in either stdlib has a `SWAP` after the call.

## the exit and the caller's tail are asked different questions

both are spans of allow-listed instructions, and they do **not** get the same
answer about a checked load. `frame_lineno_set_impl` walks `co_nlocalsplus` and
fills every NULL slot with `None`, so a jump to an exit binds every unbound
local on the way — `LOAD_FAST_CHECK` cannot raise there. it leaves cells alone,
which is why an unbound **cell** still can.

the caller's tail is the other way round. it runs *before* anything moves the
caller: the forced return lands, the rest of the line runs with it, and only then
does the line event fire that the rewind is made from. so a slot holding nothing
still holds nothing when `LOAD_FAST_CHECK` reads it, and it raises
`UnboundLocalError` out of the tail.

`bpd_agent::bytecode::Moved` is that difference, as a type rather than a thing to
remember at a call site. before it existed, `packed = (f(1), spare)` with `spare`
bound only on a branch was answered `Arranged` and then abandoned as
`CallerLeft` — a restart that was never possible, discovered by attempting it.

## the unit is the caller's line, not the call

the rewind sets `f_lineno`, which lands at the **first instruction of the line**.
so the whole of the caller's line runs a second time, and everything on it besides
the one call is a thing that would run twice

that is why most of this feature is refusals, and every one of them is decided
off the bytecode **before the frame is touched**. `bpd` either refused, or it has
read the instructions and knows what running them again does — there is no case
where it finds out halfway

| the caller's line      | what is on it | why it is refused                                                                           |
| ---------------------- | ------------- | ------------------------------------------------------------------------------------------- |
| `x = f(obj.attr)`      | `LOAD_ATTR`   | a property is code of the program                                                           |
| `x = f(d[k])`          | `BINARY_OP`   | so is `__getitem__`                                                                         |
| `x = f(k + 1)`         | `BINARY_OP`   | so is `__add__`                                                                             |
| `if f(k):`             | `TO_BOOL`     | so is `__bool__`                                                                            |
| `obj.slot = f(k)`      | `STORE_ATTR`  | a setter, handed the forced return's value                                                  |
| `[f(), f()]`           | two `CALL`s   | the completed sibling call re-runs                                                          |
| `[f(n) for n in ns]`   | two `CALL`s   | PEP 709 inlines a comprehension into the caller's own frame, so the whole construct re-runs |
| `sorted(items, key=f)` | two `CALL`s   | the sort re-runs its key for every element it had already compared                          |
| `x = f(k)`             | nothing else  | **permitted**                                                                               |

**the unit is a span, not a range and not a line.** `co_lines` yields a range per
contiguous run of instructions, and neither one range nor all of them is what
runs again:

- one **range** misses the other branch of a conditional expression, whose
    ranges each look like a single clean call on their own
- every **range of the line** over-includes the second copy cpython makes of a
    `finally` body — which counted one call as two — and *under*-includes a call
    split over source lines, whose argument is attributed to the **argument's**
    line rather than the call's, so `got = f(\n    obj.attr\n)` hid its
    `LOAD_ATTR` entirely

what the interpreter really runs is the span from the **jump destination** to the
end of the contiguous run the call is in. before the call is what re-executes;
after it is what runs with the value of a return the program never made, and then
again after the restart

**the destination is cpython's to choose, and bpd checks it rather than
predicting it.** `marklines` marks every `co_lines` range start as a candidate,
and `frame_setlineno` picks the candidate whose stack is compatible with where
the frame is now — so a jump made from inside a block can land on a later copy.
measured on 3.13, 3.14 and 3.14t: jumping to `with cm:` from inside its body
lands at offset 32, while the lowest offset of that line is 2

an earlier version of this page called the lowest offset a measured fact. it was
measured only from *outside* the blocks, which is the one position where it
happens to hold. the lowest offset is still what the analysis starts from — it
has to start somewhere — but it is a guess, and both places it is used check it:

- the **exit** is not guessed at all, and is not a line: it is one offset, made
    the only destination cpython has to choose between — see
    [reaching an offset](#reaching-an-offset-f_lineno-cannot-name). an earlier
    version jumped speculatively and put the frame back when the landing turned
    out unusable — and the put-back landed on a *different copy* of the same
    line, so a frame stopped on the exception copy of a `finally` came back on
    the normal copy while the answer said `NoCleanExit`, which says nothing
    moved. the debuggee then died
- the **caller** cannot be jumped before the callee is forced out, so its guess is
    carried and compared with the landing at the moment of the rewind. a mismatch
    abandons the restart, with `restart_abandoned` saying which offset was read
    and which was reached, rather than resuming into a span nobody checked

the same allow list governs the **exit**, where it is a forward walk: from the
destination, every instruction up to the first return must be a load. accepting a
line because *some* range of it was loads and a return let
`return (side_effect(99) if total else None)` through — the jump landed on the
call, and the debugger made a call of the program's before restarting anything

that figure, and the one about what lies past the end of a caller's span, come
from `scripts/restart_shapes.py` — which prints what it counted, whole
instruction runs rather than first opcodes, against whichever interpreter invokes
it. a structural claim quoted in a doc and re-derivable by nobody is the thing
this page has spent several rounds removing, and two sets of figures were wrong
here before the script existed

the list of what may share the span is an **allow list**, and that is deliberate.
a list of the dangerous opcodes fails open — an opcode a future interpreter adds
is one nobody wrote down — and 3.13 and 3.14 already spell half of these
differently: `LOAD_FAST` became `LOAD_FAST_BORROW`, `BINARY_SUBSCR` was folded
into `BINARY_OP`, and `RETURN_CONST` became a `LOAD_CONST` and a `RETURN_VALUE`.
so what is permitted is loads, stack shuffles, `BUILD_TUPLE`/`BUILD_LIST`, and a
store into the names the caller's line binds — and anything else refuses, naming the opcode

the instructions are read through `dis.get_instructions`, on the request path and
never on the event path. decoding `co_code` against a table of opcode numbers
would be one more thing hand-maintained against an interpreter that renumbers
them every release

## reaching an offset `f_lineno` cannot name

`frame.f_lineno` is the only supported way to move a running frame, and it will
only land on a **mark**. `marklines` walks `co_linetable` and marks each range
start whose line differs from the last; `frame_lineno_set_impl` then chooses
among the marks carrying the line that was asked for.

that is the *whole* of what made an exit rare. cpython fuses a function's
implicit `return None` onto the **last statement's** line:

```
def f():
    a = 1
    print(a)                line 5: [LOAD_GLOBAL, LOAD_FAST_BORROW, CALL,
                                     POP_TOP, LOAD_CONST, RETURN_VALUE]
```

the last two instructions are a perfectly clean exit. they are just not at a
range start, so no line number names them — and the start of line 5 does name a
`print`. that is an ordinary function, and it used to be refused.

everything else `frame_lineno_set_impl` does is already general over offsets, and
is exactly what this needs: `mark_stacks`, the stack-compatibility check, the
unwind that decrefs what it pops and hands an `Except` entry back to
`tstate->exc_info`, and binding unbound locals to `None`. so the offset is made a
mark. for the length of one assignment the code object's line table is replaced
by one carrying a line number the code object does not otherwise have, starting
at the offset wanted:

```
the real table, byte for byte, up to the offset │ sentinel │ sentinel │ …
```

and then it is put back. the prefix is **byte-identical** — every offset the
frame can actually be at keeps the line it really has, and only the epilogue it
is about to be moved into reads differently.

two things fall out of it:

- the sentinel is the only line in the table, so cpython has exactly **one**
    candidate. the choice between marks — the thing the old rule's "every range
    start must walk clean" existed to be safe against — is gone
- `compatible_stack(from, to)` returns true for **any** `from` when `to` is the
    empty stack, so a depth-zero target is reachable from anywhere in the
    function. no `incompatible stacks`, and no `can't jump into the body of a for
    loop` to route around

`scripts/restart_shapes.py` measures what it buys, against the interpreter's own
stdlib: code objects with a clean exit go from 20.4% to 65.7% on 3.13, 17.2% to
55.2% on 3.14, and 17.4% to 55.4% on 3.15.

### the offset has to be at abstract stack depth zero

the move unwinds the frame's stack **down** to the target's depth and no further.
a target at depth one would have the frame return whatever the unwind happened to
leave on top — an intermediate value of the program, returned as if the function
had computed it, silently.

depth is counted backwards from the return, because that is the one point in a
code object whose depth cpython states: `mark_stacks` holds
`pop_value(next_stack) == EMPTY_STACK` at `RETURN_VALUE`, so exactly one value is
on the stack there, and `RETURN_CONST` carries its own and takes none. each step
back is `dis.stack_effect`, which is `PyCompile_OpcodeStackEffect` — the same
function `mark_stacks` itself falls back to for everything it does not
special-case. a run that never reaches zero is not offered.

### what can be observed while the table is swapped

`PyCode_Addr2Line` reads the line data monitoring cached, **not** the line table,
for any code object instrumented for `LINE` — and a frame `bpd` can restart is in
one. so `frame.f_lineno` and every traceback answer the real line throughout,
measured on 3.13, 3.14 and 3.14t. what does read differently is `co_lines()`,
`co_positions()` and `dis` over that one code object's tail, until the table is
put back.

the cached line data cannot be corrupted by the window either: `initialize_lines`
runs once, when `LINE` is first enabled on a code object, and the result is freed
only in `code_dealloc` — so a code object already instrumented will not re-read
the table at all.

the swapped-in bytes are **never freed**. a thread that read the pointer out of
the field before it was put back may still be reading the bytes behind it, and on
a free-threaded build nothing serialises the two. they are tens of bytes and
there is one per forced exit.

### the one `unsafe` in the tree

`unsafe_code` is denied for the whole workspace and allowed in
`bpd_agent::linetable`, for two reads and four writes of a single word.
`pyo3-ffi` does not declare `PyCodeObject`; hand-writing the layout is the
per-version table this project refuses everywhere else; and doing the same poke
through `ctypes` would only move it out of the lint's sight.

what makes it reviewable is that the slot is not believed, it is **checked**. it
is found by comparing the code object's own words against the address of its own
`co_linetable` — calibrated once against a probe `bpd` compiles itself, and
compared again immediately before every write. swept over both stdlibs it is the
same word for every code object, and unique for all but one of 31927 on 3.14. a
slot that does not check is `exit_not_addressable`, and not a write.

the table `bpd` builds is checked too, and by cpython: with it in place and the
frame still where it was, `co_lines()` has to read back exactly one range,
starting at the exit, carrying the sentinel. if it does not, the table goes back
and nothing moved.

### what this does not reach

the caller half. a rewind can only be made from a `LINE` event, and the same
doctoring **cannot** manufacture one: `initialize_lines` runs once per code
object and every caller `bpd` could restart in is already instrumented. tried and
measured — the table is in place, `set_events` is cycled, `restart_events()` is
called, and no event fires. so
[nothing runs after the call](#the-other-three-refusals) stands.

## the other three refusals

**no clean exit.** a frame is forced out by moving it to the point where a
return's value is loaded, so that the loads and the return are all that runs.
what has no such point is a function **every** return of which returns an
expression:

```
def size(path):
    return path.stat().st_size     [ …, CALL, LOAD_ATTR, RETURN_VALUE ]
```

there is no sequence anywhere in that code object which produces a value and
returns without running the program, so no mechanism reaches it — not this one
and not a different one. a function that falls off its end, or that returns a
name or a constant anywhere, has one. `scripts/restart_shapes.py` counts what is
left: 34.3% of code objects on 3.13, 44.8% on 3.14 and 44.6% on 3.15, the two
later figures inflated by the PEP 649 `__annotate__` functions those releases
compile — 5141 and 5332 of them, each returning a dict display, and none of them
a frame anybody restarts

a function with several returns offers all of them, highest offset first — the
epilogue, which is where the implicit `return None` lives. a refused assignment
moves nothing and binds nothing, measured on 3.13, 3.14 and 3.14t, so trying
costs nothing

**nothing runs after the call.** the rewind can only be made from a `LINE` event:
cpython answers `can only jump from a 'line' trace event` to anything else, so
`PY_RETURN` cannot drive it. if the call line's own instruction range holds the
`RETURN`, execution never enters another line of the caller and there is nowhere
to move it from

```
def c(): f()                   no line event follows — refused
def c(): f(); print(...)       a line event follows
def c(): r = f(); return r     a line event follows
```

that restriction also makes the dangerous case **unreachable** rather than
merely unlikely. with `sorted(items, key=f)` the caller is suspended in a C call
and no line event fires in it while `sorted` runs, so a rewind cannot happen in
the middle of one. it is the case that restricts JVMTI's `PopFrame` too

**no caller.** the outermost frame of the program has nothing above it but bpd's
own bootstrap, and a restart is the caller making the call again

## the three refusals about names rather than shapes

the rules above are about the **shape** of a line. three refusals are about what
the frame holds, and they are the ones that closed this feature's most subtle
defects — a forced exit that raises into the program rather than returning is a
false `Arranged` that looks like a clean one

- **a namespace that is not a plain dict.** `LOAD_GLOBAL`'s fast path needs the
    globals **and** the builtins to be exact dicts, because a miss in one falls
    through to the other; off that path it is `PyObject_GetItem`, which runs the
    mapping's `__getitem__` or `__missing__`. `LOAD_NAME` and `STORE_NAME` are
    the same question about a module or class body, where `__prepare__` can put
    any mapping there. `NamespaceIsNotADict`
- **a read the frame holds nothing for.** `LOAD_DEREF` on an empty cell and
    `LOAD_GLOBAL` on a name in neither globals nor builtins both **raise**.
    cpython binds a frame's unbound *locals* to `None` when it moves and leaves
    both of those alone — measured, and the note here used to say otherwise.
    `ExitWouldRaise`, naming the name
- **a caller stopped where its own line table has no line.** there is nothing to
    rewind to, and picking a nearby line would be picking which statement runs
    again. `CallerHasNoLine`

the first two are produced **twice** — once about a line the frame would be
forced out through, once about the caller's call line — and they carry which,
because for a while they shared one message written for the caller. a user
refused because of a `LOAD_GLOBAL` on the frame's own `return` was told the
caller's line stored into a `__prepare__` namespace, which was wrong in every
particular

## the frames restart frame refuses, and what was measured

a **generator, coroutine or async generator** frame, all three, and for one
reason rather than three: `f_back` of such a frame is **whoever resumed it**,
which need not be what produced it — and the whole mechanism is "make the
caller's line build the frame again". measured on 3.13, 3.14 and 3.14t:

| shape                        | what happened                                                                                                                                              |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| a generator forced out       | `next(it)` raised `StopIteration`, which left the caller instead of reaching a line event, and the program died                                            |
| a coroutine driven by a task | `f_back` was `asyncio.events.Handle._run`, and rewinding *that* answered `InvalidStateError: __step(): already done` — the event loop was what got rewound |
| a coroutine awaited in place | it restarted correctly                                                                                                                                     |

the third is refused with the other two, and that is the finding worth writing
down: nothing in the frame distinguishes it from the task case. both are a
coroutine frame whose `f_back` is a python frame, and a debugger that permitted
the one it could not tell apart from the other would be guessing

the generator case is the one that makes the refusal load bearing. the caller's
line `second = next(it)` is `LOAD_GLOBAL, LOAD_FAST, CALL, STORE_FAST` — it passes
every other test this feature makes, so without the refusal by kind nothing would
have caught it

set next statement to a line of the body is the operation that works there, and
the refusal names it. a backward jump inside a generator body re-executes it and
the generator goes on yielding

## what a restart changes that nothing else would say

the forced return is a **real** return with a real value, so the rest of the
caller's line runs with it before the rewind. `got = f(x)` really does store it in
`got`. that is a value the program never computed, sitting in a live frame, and it
is on the answer as `disturbed` — it is overwritten when the restarted call
returns for real, and until then it is what the caller holds

the exit jump binds the forced-out frame's unbound locals to `None`, like any
other jump, and that is `bound_to_none`. it is reported even though the frame is
about to die, because the frame is live at the moment the answer is made and a
client reading its locals then reads these

## the half that cannot be decided in advance

whether cpython accepts a move **to** the call line from wherever the caller got
to. everything else is read off bytecode first; this is cpython's answer and it
gives it at the time

so it is reported rather than swallowed. the thread is held where the refusal
happened, with reason `restart_abandoned`, saying that the frame was forced out
and returned and that the call was **not** made again. another way to reach it is
an exception leaving the caller before it reaches a line, and a third is the
rewind landing somewhere other than the line it asked for — `Abandoned` is
`non_exhaustive`, and this is not a closed set

## and the gap that is known rather than closed

**a restart cancelled by something else is not announced.** the client was told
the restart was arranged; if a breakpoint, an exception, a pause or a stopped
world holds the thread before the fresh frame is entered, the restart is taken
off — it has to be, because an operation left armed keeps the interpreter from
forgetting a single location for the rest of the run — and the client learns only
about the stop it got

saying so needs a report the debugger makes without being asked, which is a
`Told` in `crates/bpd_core/src/parity.rs` and a `carriage_of` arm in both front
ends. it is written down here rather than left for somebody to find

no ordinary shape reaches either. every block shape was measured on 3.13, 3.14
and 3.14t — a call in a `for` body, a `while` body, a `try`, a `try/finally`, a
`with`, an `if`, a nested `for`, and each of those as the **last** statement of
its block — and cpython accepted the rewind in all of them, including from a loop
header back into its body. what the report exists for is that half a restart is a
frame that returned and never came back, and a silence there would be
indistinguishable from a restart that worked

## stepping and breakpoints across a jump

these are about **set next statement**. a restart resumes the thread itself, so
nothing else can be armed on it while one is in flight, and the line the rewind is
made on does not run at all — a breakpoint there is not passed over, it is a
breakpoint on a line the program did not execute

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

**`goto`** is answered and then followed by a `stopped` event with reason
`goto`, because the thread was never resumed and the client has to re-read the
stack to see where it is. that event repeats what the stop was announced with
about the other threads rather than recomputing it: nothing about them changed,
and a second event saying otherwise would contradict the first

**`restartFrame`** is answered and then nothing — the thread was let go, and
sending a `stopped` event from there would have the client read a stack while the
program ran past it. the `stopped` event arrives when the restart lands, with
reason `restart`, which is DAP's own name for exactly this. a rewind cpython
refused arrives as a stop too, deliberately **not** with reason `restart`: a
client that rendered it as one would say a frame is running again when it
returned and did not come back

what neither protocol has a field for is what really runs again. DAP gets it on
the console and, for a client that asked, as a `bpd/restarting` event; MCP gets it
in the answer's `notes`. both are written from the same values in the same place,
so neither can drift from the other
