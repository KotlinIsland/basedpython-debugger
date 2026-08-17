# where the program went

stepping backwards, for the half of it that can be afforded honestly

```json
{ "command": "bpd/record", "arguments": { "on": true } }
{ "command": "bpd/trail",  "arguments": {} }
```

a trail is where the program has been — file, line, function, thread — over a
window of the last hundred thousand steps. it says **where** and never **what**

## the two halves have very different prices

measured before any of this was built, on a loop of 300,000 iterations over
three lines:

| what is stored per line               | time     | against bare |
| ------------------------------------- | -------- | ------------ |
| nothing                               | 13.1 ms  | —            |
| the location — code object, line      | 75.1 ms  | 6×           |
| the location and a copy of the locals | 390.8 ms | 30×          |

**those are a prototype's figures and not bpd's.** they come from the same
python harness as the callback numbers below — a python callback storing into a
python ring — so what they compare is two ways of *storing*, with none of bpd's
own `LINE` path in either. they are kept here because the ratio between the two
rows is the thing that decided the design, and that ratio is what a prototype can
honestly answer

what they do **not** say is what turning a recording on costs you.
`crates/bpd_engine/tests/replay_values.rs` is the instrument that answers that,
against bpd itself and with the depths separated. no figure from it is quoted
here yet: every run of it so far was taken on a loaded machine, and it prints the
spread beside each row precisely so that a number whose noise exceeds the effect
is not mistaken for a measurement

so "where did it go" fits a fixed ring of small entries, and "what was this
variable" costs several times that again — and stored as live objects it is
unbounded in memory and perturbs the heap it copies from

a trail that says where the program went and refuses to say what anything was is
a debugger reporting what it has. one that filled the values in afterwards would
be inventing history

## asking for the values half

`depth` on `bpd/record` and on the `record` tool decides how much of each step is
kept:

| depth    | what it keeps                                     |
| -------- | ------------------------------------------------- |
| `where`  | the location — the default, and the cheap one     |
| `frame`  | reaches the frame and keeps nothing of it         |
| `locals` | materialises `f_locals` and keeps nothing of it   |
| `values` | the location and what the frame held, as **text** |

`frame` and `locals` store nothing and are not useful on their own. they exist so
the cost of `values` can be attributed: a `LINE` event carries a code object and
a line and **no frame**, so reaching one, materialising `f_locals` and rendering
each name are three separate costs, and a single number for the lot cannot say
which to argue with. what every run of the instrument has agreed on is that
**rendering dominates** — which makes *which names* the lever rather than *how
many lines*, and that is the shape a logpoint already has

### what is kept is text, and read without running the program

a value is rendered to a bounded string rather than kept as a reference. the
reference is cheaper to take and wrong twice over: it makes the window unbounded
in memory, and it keeps alive objects the program has finished with, which is bpd
perturbing the heap it is describing

the render runs **none** of the program — no `repr`, no `__len__`, no `__str__`,
which are all the program's own code, and on every line of a recording rather
than once. so the exact builtin types render as themselves and everything else
says what it is. that is a weaker answer than a repr, and it is one that cannot
be wrong

### the depth a client did not name is the cheap one

absent means `where`. the depths differ by a large multiple of a bare run, so a
front end that picked the expensive one for a client that said nothing would be
spending its user's time without being asked — and one that hard-coded the cheap
one would put the other half out of reach entirely. it is a `Facet`, so the
parity test fails if either front end stops offering it

## it is a mode, and it costs what it costs

recording is the one thing that turns off the property the rest of the design
rests on. measured, same loop:

| how the lines are watched           | time    | callbacks delivered |
| ----------------------------------- | ------- | ------------------- |
| not at all                          | 7.6 ms  | 0                   |
| watched, `DISABLE`d after the first | 11.1 ms | **6**               |
| watched, never disabled             | 33.9 ms | **900,003**         |

the middle row is [the whole architecture](breakpoints.md#what-it-costs) in one
number: six callbacks for nine hundred thousand line executions. recording needs
every one of them, and that is about **4×** a bare run for the delivery alone

so it is **off by default**, turned on for a region of a run rather than a
session, and both front ends say so when it starts — DAP on the `important`
console category, MCP in the answer's own words

## the window's edge is stated, not implied

the ring holds a fixed number of steps and counts what fell out of it. `dropped`
above zero means the oldest entry **is not where the recording began**

that is the one mistake this answer exists to prevent. a trail read as the whole
run, when it is the last hundred thousand steps of a much longer one, is a reader
concluding the program started somewhere it did not. so the count is carried on
every answer, and both front ends say it in words when it is not zero

## stopping keeps the trail

stopping is what somebody does *in order to* read it. throwing it away at that
moment would make the one thing they were about to do impossible

starting clears it, because a trail spanning two recordings has a gap in it that
nothing marks

## what it is not

**not DAP's `stepBack`.** that request means "put the program back where it
was", and a trail says only where it went. a client that drew this as `stepBack`
would be offering to undo something `bpd` cannot undo, so
`supportsStepBack` stays unadvertised

## how a place is named

the window holds the code object's **address** and the line, and resolves
filenames when the trail is read. that is the whole shape of the cost: naming a
place is paid once per trail read rather than once per line executed

an address is only an identity while the object it names is alive, so every code
object the trail refers to is held for as long as it refers to it — one insert
per code object, not per line. without that, a freed code object would leave an
address the interpreter can hand to the next one, and the trail would name the
wrong file

and it lets go again. each held object counts the steps naming it, and the last
of those to fall out of the window takes the object with it. holding them all
instead would have been a window that bounds its steps and not its memory, with
no ceiling at all in a program that compiles code as it runs — which django's
template engine does — and it would be `bpd` keeping alive something the program
had finished with
