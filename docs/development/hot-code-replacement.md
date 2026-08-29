# hot code replacement

replacing the code a live process is running with the code that is now on disk,
without restarting it — or refusing, and naming what stood in the way

## the bug this exists because of

import a module, edit the file, raise:

```
File "/tmp/staleness/victim.py", line 3, in boom
    # nor did this one
RuntimeError: original line 3
```

the running code raised at what *was* line 3. cpython printed the current file's
line 3, which is now a comment. correct line number, wrong text, total
confidence — a traceback is rendered by `linecache` reading the file **now**, and
every python debugger inherits it

`bpd` already refuses to be part of that. the source around a frame is compiled
and the frame's own code object has to be in what comes out, or the answer is
`not_the_same_code` rather than a line — see
[reading a stopped program](state.md). this is the same comparison, inverted: a
mismatch is what makes a replacement worth offering, and `not_the_same_code` is
the thing that tells a client to offer it

## what a replacement is

a set of assignments to `function.__code__`, and nothing else

the top level is **not** re-run. no name is bound or unbound, no object is
created, and no dictionary is written. every reference the program already holds
is the one it held before, and it now runs different code

that is also why a class needs no machinery of its own. a method **is** a
function object in the class dictionary, so rebinding its code is seen by every
instance that already exists, at once

what is found is every function object in the **process**, through one walk of
the heap, rather than every name in the module's namespace. that is the whole
difference between replacing a module and replacing the names in its dictionary:

| what holds it                            | a namespace walk | this |
| ---------------------------------------- | ---------------- | ---- |
| `module.thing`                           | yes              | yes  |
| a method, a `staticmethod`, a `property` | yes              | yes  |
| a closure a factory handed out earlier   | no               | yes  |
| the original a decorator captured        | no               | yes  |
| a function stored in someone's registry  | no               | yes  |

## the rule

the file is compiled and the tree that comes out is walked against the tree the
process is running, code object by code object, from the module's own code
object down through `co_consts`. compiling runs none of the program — it is the
compiler, on bytes, and a module that would raise on import raises nothing here

a replacement is applicable exactly when **every difference between the two is
inside the body of a function that exists in both and takes the same arguments**

- a **body** — the module's code object, or a class's — has to be identical in
    everything it does: the instructions it runs, the names it reads and writes,
    and the sequence of things it defines
- a **function** may differ freely in its body and may not differ in its
    parameters

what is compared for a body is the **resolved** instruction stream — `dis`, with
each operand resolved to what it means and each nested code object reduced to its
`co_qualname` — rather than `co_code`. raw bytecode carries indices into
`co_names` and `co_consts` rather than the names and values themselves, so a
class attribute changed from `5` to `7` is a different instruction here and is an
index that looks the same there

what is **not** compared is the line table. it moves whenever a function body
above gains or loses a line, and it says nothing about what the body does. that
it moved is the whole reason for doing this at all

## the one thing cpython forces an exception for

**since 3.13 every class body stores its own source line**, as
`__firstlineno__`. so a class that merely moved down the file has a genuinely
different body. measured on 3.13, 3.14, 3.15 and 3.14t:

| interpreter       | how the line gets into the body                       |
| ----------------- | ----------------------------------------------------- |
| 3.13              | `LOAD_CONST <line>` then `STORE_NAME __firstlineno__` |
| 3.14, 3.15, 3.14t | `LOAD_SMALL_INT <line>` then the same store           |

that is a **line number**, in the category already excluded for a body along with
the line table. left in the comparison it would refuse every edit above a class
as a changed class layout, which is most edits

so the one instruction that feeds `STORE_NAME __firstlineno__` is masked. it is
*replaced* rather than dropped, because both loads are two bytes wide on every
interpreter here and replacing keeps every later jump's target where it was.
`a_class_body_carries_its_own_source_line_and_it_is_the_only_thing_masked` is
what fails if cpython changes: half of it moves a class and requires the
replacement to go through, and the other half changes what the class body really
does and requires it to be refused

## classes are in this slice

they were the open scoping question and the answer is that they cost nothing
extra. a method is a function object, so the mechanism is the same one; and
"a changed class layout" is not a check of its own — it is the body rule applied
to a class body, which is the only other code object cpython leaves
unoptimized. the two refusals below are the same rule at two levels

## what is refused, and why

nothing is ever applied partially. a process half way between two versions of a
file produces evidence about neither, so every refusal is collected **before**
anything is written and a replacement that cannot be made whole changes nothing
at all. the answer carries *all* of what stood in the way rather than the first,
because a client fixing them one at a time is a client asking this seventeen
times

| what was found                                                | why it cannot be applied                              |
| ------------------------------------------------------------- | ----------------------------------------------------- |
| the module body is different code                             | applying it needs the top level **re-run**            |
| a class body is different code                                | applying it needs the class dictionary written        |
| a function's parameters changed                               | callers in flight were compiled against the old ones  |
| a frame is running code that is about to change               | it would leave two versions running at once           |
| the file does not compile                                     | there is nothing to apply                             |
| the interpreter compiled nothing from the file                | there are no live function objects                    |
| only part of the file's code has been seen                    | a partial view answers every question wrongly         |
| the file was compiled more than once                          | which copy a live function belongs to is unanswerable |
| a changed function holds two nested code objects of one name  | nothing says which replaces which                     |
| a live function's closure does not fit the new code           | cpython refuses that assignment                       |
| live objects run a nested function the file no longer defines | they would run code in no version of the file         |

### re-running the top level is not a reload

the roadmap names "a module with import side effects" as a refusal. it is not a
case of its own — it is the instance of the module-body rule that people meet
first

a module body that is different code could only be applied by executing it again,
and that is running the program a second time: its imports, its calls and its
registrations all happen again, and every name it binds becomes a **new object**
that anything already holding the old one never sees. a decorator that registered
a handler registers a second one. a class statement builds a second class, and
the instances that exist are instances of the first

so `bpd` never re-runs it, and a module body that changed is refused with that
reason. what this costs is real and is stated rather than hidden: **adding a
function, deleting one, or changing a module-level constant is refused**, because
each of them is a change to the module body

### a live frame

this one was written into the roadmap as a safety requirement and it is not one.
**measured on 3.13, 3.14, 3.15 and 3.14t**: assigning `function.__code__` while a
frame of that function is in flight is accepted, the frame runs the old code to
completion, and the next call gets the new one. nothing aborts, nothing is
corrupted. a frame holds its own reference to its code object, so the assignment
rebinds the *function* and not the frame

that is worth stating precisely because the neighbouring operation is the
opposite. assigning `f_lineno` on a frame suspended in a call is *also* accepted
and then kills the interpreter outright — `Fatal Python error:
_PyEval_EvalFrameDefault: Executing a cache.` on 3.13, `_TAIL_CALL_CACHE` on 3.14
and 3.15, `SIGABRT` on all three. that finding is real and it is in
[set next statement, and restart frame](jumps.md). it does **not** generalise
here, and no message, comment or page in this project may say that it does — a
reason that is false is worse than no reason

so the refusal is a design decision, and the reason is honesty. between the
assignment and that frame returning, the process really is running two versions
of one function, and a stack whose frames behave two different ways is evidence
about neither version — which is the roadmap's own objection to a partial
application, one level down

it costs the case people most want this in: stopped at a breakpoint **inside**
the function they just edited. it is refused, and the refusal names the frame and
says to let it return first

that refusal is the **default**, and the default is the guarantee. the
alternative — apply, and report which frames are still running the old version —
is more useful and a strictly weaker promise, so it is not a change of behaviour
but a thing a caller asks for by name: `even_under_a_live_frame` on the request,
`evenUnderALiveFrame` on `bpd/replaceCode`, `even_under_a_live_frame` on the MCP
tool. it is not a flag on the engine's method either — there it is a second
method, `replace_code_even_under_a_live_frame`, because a bare `true` at a call
site says nothing about what it buys

what comes back is `applied`, with every frame that will finish on the old code
named in `still_running`. both front ends put it where a refusal's reason goes —
an `important` console event under DAP, the first note of the answer under MCP —
because this is the one case where *succeeding* costs what failing usually saves,
and a caller who read an unqualified success would have been told the process is
on one version of the code when it is on two

**that list is true when it is made and not afterwards**, which is the part worth
saying twice. the frames on it return on their own schedule and nothing reports
when one has, so it says which frames were on the old code at the instant of the
replacement. a client polling it as the state of the process now is reading a
list that has been going out of date since it was written. that is why the
ordinary answer is still a refusal: the honest version of this feature is
strictly less useful than it first looks, and a caller has to want it knowing
that

a non-boolean is refused by name rather than read as truthy. the value decides
whether the process ends up running two versions of one function, and an adapter
that guessed would make that trade on a typo

a frame that will run the code is not only one on a thread's stack. a generator,
a coroutine or an async generator that is **suspended** holds a frame nothing is
executing, and it will run the old code the moment anything sends into it. those
are refused too, including one that was created and never advanced

what is not a blocker is a frame in a traceback. it has already returned and will
never run again, which is why the heap is not searched for frames at all — the
two kinds that will run are asked for directly, from `sys._current_frames()` and
from the suspendable objects themselves

## a whole build at once, and the map with it

a replacement takes a **list** of files. it is applied at once or not at all:
every refusal of every file is collected before anything is written, and one
refusal anywhere leaves the process untouched. that is the rule one file already
had — a process half way between two versions produces evidence about neither —
one level up

what needs it is basedpython. the program runs out of a tree `by run` transpiled
the project into, so nothing the user edits is the file the interpreter compiled:
a `.by` because it was transpiled, a hand-written `.py` because it was copied
there. giving that program an edit means `by` staging the file into the tree
again — and one edit can change the python emitted for more than one module,
because the transpile is type-directed

so a `.by` may be named on the request now. it is resolved to the generated
python through the map the session already holds, which is the same translation a
breakpoint, a frame and a source read go through. **transpiling has not moved
here and will not**: by the time the request arrives, `by` has already written the
bytes, and what this compiles is the generated python it always compiled

### `remap`, and why it is not its own request

staging one file of a build again rewrites `_by_sourcemap.py` beside the
generated python. so the tables the session holds describe the tree it used to
be, and every `.by` breakpoint is armed on a generated line that came out of
them. both have to land before any `__code__` is assigned

they land in **one message** because of what the agent is. it holds the GIL for
the whole of one message and for no longer, so a debugger that sent the tables,
the breakpoints and the replacement as three would leave two windows in between —
and in either of them another thread's logpoint is mapped through a table
describing code it is not running. one message has no window in it

the order inside it is: install the tables, translate and re-arm the whole
breakpoint set, then replace. the order **outside** it is the reverse — the
engine reads the new map first, because everything it sends depends on which
tables they are, and adopts it last, because a refused replacement installs
nothing and a session whose map had moved on while the process had not would
report every line of the build out of a table describing code nothing is running

the whole breakpoint set is translated rather than the breakpoints of the files
being replaced: a table that moved moves every breakpoint of the build

## what a replacement reports

the same standard `Jumped` is held to: a user has to be able to see what is now
different about their process

- **`changed`** — one entry per code object that moved, with `co_qualname`, the
    line it began on before, the line it begins on now, and **how many function
    objects held it**. that count is not decoration: two is a closure a factory
    handed out twice, and it is how a client sees that the decorator's captured
    original was rebound too
- **`unchanged`** — what the file holds that did not move at all. `changed` being
    empty is "nothing needed replacing", which is a different fact from "nothing
    could be replaced" and must not be rendered as one
- **`rebound`** — every breakpoint whose binding the replacement changed

### breakpoints move, and are told to

binding walks down from the file's registered root code object, and after a
replacement every live function of that file runs the new tree. so the old root
describes code nothing will execute, and a breakpoint bound through it would be
armed where no thread will ever arrive — set, visible, and never firing

the root is swapped and the whole breakpoint set is resolved again. a breakpoint
is a **line of a file**, so an edit above it means the same request now names a
different statement: a breakpoint on what was a `return` can come back bound to
the `def` above it, because that is where the request lands now. that is reported
rather than left to be discovered

## the threads it did not hold

everything the refusals say about a thread `bpd` is not holding is a **sample**,
and the answer carries the mode it was taken in, like every other answer in this
project. it is the conservative direction — a sighting refuses — and stopping the
world first is what turns the absence of one into a reading of every thread

## reaching it

|             | DAP                                 | MCP            |
| ----------- | ----------------------------------- | -------------- |
| replacement | `bpd/replaceCode`, a custom request | `replace_code` |

DAP has no request of its own, and its `restart` is the opposite thing — it
throws the process away, and the whole point of this is that the process stays.
so it is an extension, which DAP provides for and a client sends with its own
`customRequest`. an editor is where the file gets edited, so an editor is where a
replacement is worth offering, and the parity rule does not let it be an agent's
alone

both carry the whole answer. a client given only "yes" cannot show what is now
different about the process, and one given only "no" cannot show which of the
user's edits to undo. the DAP adapter additionally writes each refusal to the
`output` stream a user is already looking at
