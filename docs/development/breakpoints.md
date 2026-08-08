# breakpoints

a breakpoint is a request against a *source location*. binding it means finding
the code object and the offset that will actually run, and `bpd` never reports
one as set unless it has both

everything on this page is about the same rule, applied in four places: if the
answer is not knowable, say so and say why, rather than reporting a breakpoint
that quietly never fires

what exists today is binding and stopping. conditions, hit counts and logpoints
are not built

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

| what is really going on | what comparing text does |
| --- | --- |
| an editable install symlinks the package into the tree | two paths, one file, no binding |
| macos `/var` is a symlink to `/private/var` | two paths, one file, no binding |
| a case-insensitive volume holds `Widget.py`, the editor says `widget.py` | two paths, one file, no binding |

so the identity used is the **filesystem's own**: `(device, inode)` on unix, and
the canonical path on windows, where `GetFinalPathNameByHandle` already resolves
links and normalises case. `canonicalize` alone is not enough on macos, which
does not correct case at all

a path with no such identity has no identity, and nothing resembles it into one:

| `co_filename` | what it is |
| --- | --- |
| `<string>`, `<stdin>` | source that was never on disk |
| `<frozen importlib._bootstrap>` | a module compiled into the interpreter |
| `/opt/app.zip/pkg/module.py` | a zipimported module |

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

## unbound is an answer, and it can change

a breakpoint in a module that has not been imported cannot bind. it is reported
unbound with that as the reason, and it stays in the set

when the module *is* imported, the code object is registered, the breakpoint is
resolved again, and the agent tells the engine — unprompted, while the program
is running. the client is told both times. this is the same path that binds code
built by `exec`, because from the agent's point of view nothing distinguishes
them

## what changes when a breakpoint is added

`sys.monitoring.restart_events()` re-enables every location that has been
disabled, **process wide**. it is a blunt instrument and it is the right one
here: a line that ran once and was not a breakpoint told the interpreter never
to offer it again, and PEP 669 has no per-location undo. moving a breakpoint
onto such a line has to restart everything

it is the wrong instrument for anything per-frame — see the stepping section of
[architecture](architecture.md)

## what a breakpoint stop holds

this is the part where it would be easy to overclaim, so it is stated plainly

the thread that hit the breakpoint is genuinely held: it is inside the
monitoring callback and does not return until the engine resumes it. that thread
also holds the GIL, so on a gil-enabled build no other python thread makes
progress either — but that is the interpreter's doing, not stop coordination
that `bpd` implemented, and on a **free-threaded build it does not happen at
all**. a thread sitting in a C call has already released the GIL and keeps
running on both

so the stop reports **which thread stopped**, and says nothing about the others.
it does not list them as suspended, because they are not, and a debugger that
reported a whole-program stop it had not performed would be lying about the one
thing it exists to measure

real stop coordination — suspending every thread, and reporting the ones parked
in native code as *running in native code* rather than as stopped — is its own
piece of work, and it is not built. until it is, a second thread that reaches a
breakpoint while another is stopped waits for the control connection and then
reports its own stop, in order

## the shape of a request

the engine sends the **complete** set, not a delta. a debugger that accumulates
edits ends up with two ideas of what is set, and they diverge. every breakpoint
carries the client's own id, which comes back in every report about it and in
the stop it causes — and two breakpoints in one request may not share an id,
because that would hand the client a single answer for two questions with no way
to tell which it belonged to

a request is only answered while the debuggee is stopped. the agent reads the
control connection inside a stop and at no other time, so asking a running
program to bind something would be a request answered whenever it next happened
to stop — which is not an answer, and waiting for it looks exactly like a hang.
the engine refuses instead

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

neither callback materialises a frame. deciding "this is not interesting" needs
the code object's address and a line number, and nothing about the frame that is
about to run

## how it is tested

`crates/bpd_engine/tests/breakpoints.rs`. nothing there takes the agent's word
for a stop: the fixture programs write a marker on a line *after* the breakpoint,
and every stop asserts that it has not happened yet. the expected line tables
and offsets come from `co_lines()` in a separate interpreter process, so the
answer is whatever cpython says on the machine running the test
