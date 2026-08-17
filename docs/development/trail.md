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

so "where did it go" fits a fixed ring of small entries, and "what was this
variable" costs five times that again, is unbounded in memory, and perturbs the
heap it is copying from — it is a copy of live objects per line

a trail that says where the program went and refuses to say what anything was is
a debugger reporting what it has. one that filled the values in afterwards would
be inventing history

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
