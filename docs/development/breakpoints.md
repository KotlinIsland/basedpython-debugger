# breakpoints

a breakpoint is a request against a *source location*. binding it means finding
the code object and the offset that will actually run, and `bpd` never reports
one as set unless it has both

everything on this page is about the same rule, applied in four places: if the
answer is not knowable, say so and say why, rather than reporting a breakpoint
that quietly never fires

what exists today is binding, stopping, conditions, hit counts and logpoints.
none of it has a command line surface yet — breakpoints reach a user through the
adapters, which are not built

## finding the code objects at all

PEP 669 has no "code object created" event. there is no notification to
subscribe to, and `exec` can build a function an hour into a run

so the agent arms `PY_START` globally, registers every code object the first
time it is called, and returns `DISABLE` — one native call per code object,
once, and never again for that code object

that alone would only ever see code that has *run*. what closes the gap is that
a module's code object holds its functions, its classes, its lambdas and its
generator expressions in `co_consts`, recursively. registering the module gives
the whole tree, and binding walks it. so a breakpoint in a function that has
never been called binds the moment the module it lives in is executed

the discovery callback is armed **only while a breakpoint is set**. with nothing
set nothing can stop, so the instrumentation would be paid for and never used.
the program checks this itself, in `discovery_is_turned_off_while_nothing_is_set`

### what the registry keeps

one strong reference per code object registered, so that the address it is keyed
by can never be handed to a different object. that is bounded by the code the
program actually executes, with one deliberate exclusion: a code object whose
filename does not identify a file on disk is **not** kept. it could never be the
target of a breakpoint, and retaining it would mean a program that calls `exec`
in a loop grows a reference for every iteration

## the same file, spelled differently

the user gives a path. the interpreter has whatever string was handed to
`compile`. deciding whether they are the same file is where breakpoints quietly
fail to bind, so text is not compared at all

| what is really going on                                                  | what comparing text does        |
| ------------------------------------------------------------------------ | ------------------------------- |
| an editable install symlinks the package into the tree                   | two paths, one file, no binding |
| macos `/var` is a symlink to `/private/var`                              | two paths, one file, no binding |
| a case-insensitive volume holds `Widget.py`, the editor says `widget.py` | two paths, one file, no binding |

so the identity used is the **filesystem's own**: `(device, inode)` on unix, and
the canonical path on windows, where `GetFinalPathNameByHandle` already resolves
links and normalises case. `canonicalize` alone is not enough on macos, which
does not correct case at all

a path with no such identity has no identity, and nothing resembles it into one:

| `co_filename`                   | what it is                             |
| ------------------------------- | -------------------------------------- |
| `<string>`, `<stdin>`           | source that was never on disk          |
| `<frozen importlib._bootstrap>` | a module compiled into the interpreter |
| `/opt/app.zip/pkg/module.py`    | a zipimported module                   |

a breakpoint against any of these is reported **unbound**, and the reason says
that the interpreter has loaded code under exactly that name and that the name
is not a file. that is a different sentence from "there is no such path", and
the difference is what tells a user whether they made a typo

## binding, and moving

the executable lines of a code object come from `co_lines()`. two rules follow
from it, and both are pinned against `co_lines()` run in a separate interpreter
rather than against numbers written down here:

- **one line can belong to several code objects.** `def render(self):` is a line
    in the class body *and* the first line of the method. `return (x for x in y)`
    is a line in the enclosing function *and* the whole of the generator's body.
    a breakpoint arms **every** code object that holds the line, because arming
    only the one that was easy to find is how a breakpoint inside a comprehension
    silently never fires
- **a line that is not executable moves.** a blank line, a comment or a `pass`
    the compiler elided has no location, so the breakpoint moves to the next
    executable line in that file and the response says which line it moved to. if
    there is no executable line at or after it, it is unbound, and the reason
    names the last line that could have held one

a line of `0` is never a breakpoint. cpython attributes a module's leading
`RESUME` to line 0, source lines are 1-based, and a breakpoint bound there could
not be reached

### about comprehensions

the architecture doc says a comprehension is a different code object. since
cpython 3.12 and PEP 709 that is only true of **generator expressions**: list,
dict and set comprehensions are inlined into the function that contains them, so
a breakpoint in one binds to the enclosing function's code object. the recursive
walk is still what finds that function — a method of a class is two levels down
the module's `co_consts`, and a generator expression inside one is three

### a file only half seen

binding walks **down** from the code objects registered for a file, and the
premise of that walk is that one of them is the file's own module. a root that
is not the module reaches only what is nested inside it, so the union of
executable lines is a *subset* of the file's — and every answer taken from a
subset is wrong in a way that looks right:

- a breakpoint moves past the line it should have landed on, to a later one
- a line that belongs to two code objects is armed in only the one that is
    visible, so it fires in some of the places it should
- a refusal names a last executable line that is not the file's last

so a file whose module has never been registered binds **nothing**, and the
reason says exactly that. it is not a subset answered carefully; it is not
answered

there is one way to reach that state and it is the debugger's own doing: a
module first imported from inside a breakpoint's condition. the interpreter
reports no code object created while a monitoring callback is running, so the
module's `PY_START` never arrives — and then the program calls one of its
functions, that one *is* reported, and bpd is holding one function out of a
whole file. see [re-entrancy](#re-entrancy-and-what-the-interpreter-already-does)

## unbound is an answer, and it can change

a breakpoint in a module that has not been imported cannot bind. it is reported
unbound with that as the reason, and it stays in the set

when the module *is* imported, the code object is registered, the breakpoint is
resolved again, and the agent tells the engine — unprompted, while the program
is running. the client is told both times. this is the same path that binds code
built by `exec`, because from the agent's point of view nothing distinguishes
them

**there is a step before that one**, and it is the front end's rather than the
agent's: a breakpoint can be set before a debuggee exists at all. DAP's
handshake is built that way — `initialized` is the adapter asking for the
client's configuration, and a client is entitled to send every breakpoint it has
and only then ask for a program. so `Unbound::NoProgram` sits beside `NotLoaded`
in the same vocabulary and answers `true` to `will_bind_later` for the same
reason: the difference between them is a file the interpreter has not read and
an interpreter that does not exist yet, and both bind when the missing thing
arrives. the agent never sees one — nothing has been asked of it — and the front
end that holds it says so with the core's word rather than one of its own. see
[the configuration phase](dap.md#the-configuration-phase-happens-before-there-is-a-program)

a django template is the other thing that binds late, and by the same rule: it
binds the first time django parses the template, which for a template reached
through `{% include %}` happens in the middle of a render. everything about that
is [django templates](django-templates.md), including the one place `Bound` is a
different variant — there is no code object behind a template breakpoint, so
`BoundInTemplate` carries the node classes rather than sites

## what changes when a breakpoint is added

`sys.monitoring.restart_events()` re-enables every location that has been
disabled, **process wide**. it is a blunt instrument and it is the right one
here: a line that ran once and was not a breakpoint told the interpreter never
to offer it again, and PEP 669 has no per-location undo. moving a breakpoint
onto such a line has to restart everything

a step pays for the same thing, for the same reason: a line it has to be offered
may have disabled itself on an earlier pass. see [stepping](stepping.md)

## conditions, hit counts and logpoints

a breakpoint carries three optional things beyond its location, and they are
applied in one order on every hit, with no exceptions:

1. the **condition** — a python expression. false means nothing else happens
1. the **hit count** — which of the hits the condition let through this
    breakpoint acts on
1. the **log message** — if there is one, a record is produced and the program
    keeps running. if there is not, the program stops

so a hit whose condition was false does not count, and a breakpoint that logs
does not stop. both of those are choices, and both are written into the tests
rather than into a comment

everything above happens **in the agent**. nothing about a hit is decided by
asking the engine, which is what makes a logpoint on a hot line affordable:
`a_logpoint_on_a_hot_line_costs_no_round_trips` puts one on a line executed a
million times, counts a million records, and counts the requests the engine sent
while it ran — which is one, the resume

### compiled once

an expression is compiled when the breakpoint is **set**, not when it is hit,
and never on the event path. one that does not compile makes the breakpoint
[unbound](#unbound-is-an-answer-and-it-can-change), carrying the interpreter's
own `SyntaxError`, because a breakpoint whose condition can never be answered
can never fire. the same is true of a log message with an unbalanced brace or an
embedded expression that does not compile

the filename each expression is compiled under names the breakpoint it belongs
to, so anything the interpreter says about it points back at the right one:

```text
<bpd condition of breakpoint 53>:1: SyntaxWarning: "is" with 'int' literal. Did you mean "=="?
```

### the native comparison

`name <op> literal` is the shape a breakpoint condition almost always has, and
it is answered without building an evaluation frame: the name is read from the
frame's fast locals and compared against a constant built when the breakpoint
was set. the answer a breakpoint reports says which path it is on —
`comparison` or `expression` — for the same reason a `Site` reports its offset

it is a fast path, so it has a differential test rather than an argument.
`crates/bpd_engine/tests/conditions.rs` asks every condition in a corpus twice,
once bare and once wrapped in parentheses — the same expression, and a shape the
native path cannot read — and requires the two to agree on every pass over the
line

what it declines, and why declining is the point:

| shape                                             | why                                                                                                                                                                                   |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `value > 1.5`                                     | parsing a float is a second implementation of python's parser, and one that rounds differently is worse than no fast path                                                             |
| `value == 1_0`, a literal too big for an `i64`    | python takes both spellings and rust does not, so both decline rather than differ                                                                                                     |
| `value is 3`                                      | identity is only knowable for `None`, `True` and `False`. against anything else the interpreter compares with the object it put in `co_consts`, which is not the one this would build |
| `'x' in value`, `value.attr == 1`, `(value == 3)` | not the shape                                                                                                                                                                         |

a name that reads natively but is **not a local of that frame** is handed to the
interpreter too. it is then a global, a builtin, or nothing at all, and deciding
which is `LOAD_NAME`'s job — reimplementing it is how a debugger reads a
variable from the wrong scope

### a condition that raises

it stops, and the stop carries the exception: its type, its message, and the
frames its traceback holds. treating a failed condition as false is the exact
shape of quiet wrongness this project exists to prevent — an expression that
raised has not said "no", it has said nothing

the exception is read off the object rather than formatted by
`traceback.format_exception`, which would mean importing a module from inside a
monitoring callback — the thing that corrupted line numbers once already — and
would leave `traceback` in the `sys.modules` of a debuggee that never asked for
it

a log message that raises is treated identically, and the stop says which of the
two it was

### log messages

the text is emitted as written, except that `{...}` is a python expression
evaluated in the frame and converted with `str()`, and `{{` and `}}` are a
literal brace. the expressions are compiled with the breakpoint

format specifiers are not part of it: `{value:.2f}` is not an expression and is
refused when the breakpoint is set. a brace that does not pair up is refused as
well, rather than emitted as itself — a log message that silently drops the
value the user asked for is a log message that lies about the program

### the hit counter

a counter belongs to a breakpoint, counts across every thread, and answers three
questions: the nth hit exactly, the nth and every one after, or every nth

it survives a request that does not change the breakpoint, and starts again for
one that does. rebuilding it whenever any *other* breakpoint in the set moved
would make "the third time this line runs" quietly mean something else, and the
engine sends the whole set every time

## a breakpoint that waits for another one

"stop in the handler, but only after the request that set this flag came
through". `after` names another breakpoint **in the same set**, and until that
one has acted this one is bound and **not armed**

it is not a condition with extra steps, and the difference is the whole reason
it exists. a condition is evaluated on every pass over the line; a waiting
breakpoint has no `LINE` events on its location at all, so the line runs at the
speed of a line nobody is watching. it costs nothing until the one before it
fires

### what "hit" means here, and the three questions it settles

arming happens when the earlier breakpoint **acts** — stops, or writes a log
record. a pass whose condition was false is not an act, and neither is one the
hit count has not reached: "after the request came through" means the breakpoint
did its thing, not that the interpreter went past the line

the other two answers are forced rather than chosen:

- **it is per process, not per thread.** the interpreter's local events are per
    code object, so a per-thread sequence would mean watching the location on
    every thread and discarding what the other threads saw — which is exactly the
    cost this feature exists not to have. a per-thread version that quietly paid
    it would be a performance claim that is false
- **a hit count starts at arming.** before arming, the location has no events, so
    the interpreter never reported those passes and the agent cannot count what
    it did not see. this is not a policy; it is what the mechanism can know

arming is **permanent**: the chain is one-way, and a sequence that re-arms is a
different feature rather than a setting on this one

### it is a chain, and a chain that cannot arm is refused

a breakpoint names one predecessor. what makes that safe to report is that the
impossible cases are refused **at set time**, the way a condition that does not
compile is — a breakpoint that can never arm can never fire, and reporting it
bound would be the debugger saying it is set when nothing will ever watch it:

| what was asked for                               | what comes back                                       |
| ------------------------------------------------ | ----------------------------------------------------- |
| `after` names an id no breakpoint in the set has | `NeverArms`, `no_such_breakpoint`                     |
| `after` names the breakpoint itself              | `NeverArms`, `itself`                                 |
| the chain comes back to itself                   | `NeverArms`, `cycle`, naming every id it runs through |

a cycle refuses **every** breakpoint in it rather than the one that closed the
loop. each link alone looks like it is merely waiting, and that is true of each
one and false of the set

### a waiting breakpoint is bound, and says so

this is the part a front end can get wrong without anything failing. a waiting
breakpoint **did** bind — the interpreter has somewhere to stop — so the answer
carries `waiting_for` beside the binding rather than pretending it is unbound. an
editor that showed it as an ordinary breakpoint would leave a user sitting at a
line nothing is watching, having been told it was set

- **DAP** — `after: {path, line}` on the breakpoint, and the waiting is said in
    the breakpoint's own `message`. it names a file and a line rather than an id
    because this adapter mints breakpoint ids and re-mints them on every
    `setBreakpoints`, so an id a client read off an earlier response is already
    stale
- **MCP** — `after` is the **position** of a breakpoint in the same list, since
    the server numbers them by where they appear, and the answer carries
    `armed: false` and `waiting_for`

### arming from inside a callback, which was measured

when the earlier breakpoint acts, the later one has to start being watched *from
that moment* — a logpoint that armed nothing until the next stop would arm
nothing at all. so the arming happens inside the `LINE` callback, and that it is
allowed was measured rather than assumed: on 3.13, 3.14, 3.15 and 3.14t,
`set_local_events` from inside a callback takes effect both on **another** code
object and on the one the callback is running in, and `restart_events` from
there is what undoes a per-location `DISABLE` the line had already returned

resolving the whole set again is not free, so it happens only when an act is the
**first** for that breakpoint and something is actually waiting on it — once per
link of a chain, rather than once per hit

## re-entrancy, and what the interpreter already does

evaluating a condition runs arbitrary user python **inside a `LINE` callback**.
a condition that calls a function containing a breakpoint would stop inside
itself, and one that reaches the line it is attached to would recurse without
end. so a breakpoint reached while a thread is evaluating does not fire, does
not count, and is not disabled — the flag is per thread, and another thread
reaching the same breakpoint at that moment is a real hit

cpython already does this, and the tests say so rather than the code assuming
it. `call_one_instrument` refuses to enter a tool's callback on a thread that is
already in one, so the events raised by a condition are never offered to us at
all. that is not in PEP 669, so it is measured — in a bare interpreter, with no
agent anywhere near it, by
`the_interpreter_does_not_report_an_event_raised_from_inside_a_callback`

the agent's own suppression therefore cannot be observed today, and is kept
because the behaviour it enforces is the one `bpd` chose: a stop whose stack is
half debugger is the thing the stack rules exist to prevent. if cpython ever
changes, that test fails, and the suppression is what stops the recursion

the same interpreter behaviour has a consequence the discovery design has to
live with, and it is the reason for
[a file only half seen](#a-file-only-half-seen): `PY_START` is suppressed the
same way, so a module a condition imports is never registered by it

## what a breakpoint stop holds

**one thread.** every other thread in the program goes on running, and that is
the model rather than a shortfall of it — see [threads](threads.md)

the thread that hit the breakpoint is genuinely held: it is inside the monitoring
callback and does not return until the engine resumes it. it does **not** hold
the GIL while it waits, which it used to: the agent gives the GIL back for the
duration of a stop, so a gil-enabled build behaves the same way a free-threaded
one does instead of freezing the process by accident

a second thread that reaches a breakpoint while the first is held reports its own
stop straight away, and both are held until each is resumed by name. what a stop
does not claim, and the thing that bites — **the held thread still holds its
locks** — is on the [threads](threads.md) page, along with the explicit
stop-the-world mode

## the shape of a request

the engine sends the **complete** set, not a delta. a debugger that accumulates
edits ends up with two ideas of what is set, and they diverge. every breakpoint
carries the client's own id, which comes back in every report about it and in
the stop it causes — and two breakpoints in one request may not share an id,
because that would hand the client a single answer for two questions with no way
to tell which it belonged to

a request is only answered while a thread is held. the agent runs the
interpreter's own api on a thread it is holding and at no other time, so asking a
program with nothing held to bind something would be a request answered whenever
it next happened to stop — which is not an answer, and waiting for it looks
exactly like a hang. the engine refuses instead

## what it costs

with breakpoints set, the steady state is:

- `PY_START` fires once per code object, natively, and returns `DISABLE`
- the code objects that hold a breakpoint have `LINE` enabled *locally*, via
    `set_local_events`. every other code object in the program has no line
    instrumentation at all
- inside an instrumented code object, a line that is not a breakpoint returns
    `DISABLE` the first time it is reached and is never offered again

so the cost is bounded by the number of *distinct lines executed once* in the
handful of code objects that hold a breakpoint, not by the number of lines
executed. `a_breakpoint_fires_on_every_pass_over_the_line` is what stops that
optimisation from eating the answer: the lines around a breakpoint are disabled
and the breakpoint still fires on every pass

neither callback materialises a frame to decide whether a line is interesting:
that needs the code object's address and a line number, and nothing about the
frame that is about to run. a frame is fetched only once a line has already
matched a bound breakpoint **and** something on it has an expression to
evaluate, so a plain breakpoint costs exactly what it did before conditions
existed

## how it is tested

`crates/bpd_engine/tests/breakpoints.rs` for binding and stopping, and
`crates/bpd_engine/tests/conditions.rs` for everything a breakpoint carries.
nothing in either takes the agent's word for a stop: the fixture programs write
a marker on a line *after* the breakpoint, and every stop asserts that it has not
happened yet. the expected line tables and offsets come from `co_lines()` in a
separate interpreter process, so the answer is whatever cpython says on the
machine running the test
