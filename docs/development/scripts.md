# the debug script

a **debug script** is a tree of debugger steps submitted in one call, run
against a session, answered with a transcript of what happened at every step.
it is what removes the round trip per *investigation* rather than per operation:

> run to the third call with a negative amount, then read the amount, and show
> me the stack

is one call, and what comes back says where the program was at each of those
three steps rather than only where it ended up

why it is a structured tree rather than text or submitted python is
[the interface for ai agents](agent-interface.md). this page is what was built

## the shape

it is a capability of `bpd_core` — `Request::RunScript` — so both adapters have
it, which the [parity](architecture.md#the-session-core-and-adapter-parity) rule
requires. an agent calls the `run_script` tool; an editor sends the custom DAP
request `bpd/runScript`. the same engine walks the same tree either way

```json
{
  "steps": [
    {
      "step": "run_to",
      "file": "app.py",
      "line": 40,
      "condition": "amount < 0",
      "hits": { "hits": "exactly", "count": 3 }
    },
    { "step": "eval", "expression": "amount" },
    { "step": "stack", "top": 3 }
  ],
  "budget": { "steps": 20, "wall_ms": 30000, "bytes": 65536 }
}
```

**the steps run in the engine.** only the predicates reach the debuggee — python
expressions evaluated in a chosen frame, through the machinery a breakpoint
condition already uses — so the program under test is disturbed by exactly the
evaluations that were asked for and nothing else

## it drives one thread

a stop holds one thread, so a script does too: the one the stop it names holds.
every control step resumes **that thread by name** and no other, so a script
never lets go of a thread nobody named

a stop that arrives on a different thread halts the script. the thread the step
was about is still running at that moment, and there is nothing truthful to say
about where a running thread is — so the record says which thread stopped, which
one the step was about, and stops

## the steps

| step                               | what it does                                                              |
| ---------------------------------- | ------------------------------------------------------------------------- |
| `step_over`, `step_in`, `step_out` | step the script's thread, and record where it landed                      |
| `continue`                         | let the script's thread go until it stops again                           |
| `run_to`                           | arm a breakpoint of the script's own, run to it, and **take it back off** |
| `eval`                             | evaluate a python expression in a frame, and record what it produced      |
| `stack`                            | record the thread's frame chain                                           |
| `log`                              | record a note of the script's own — nothing reaches the debuggee          |
| `if`                               | run one of two blocks, according to a python predicate                    |
| `while`                            | run a block while a predicate is true, at most `limit` times              |
| `finish`                           | end the script here, with a reason                                        |

`log` and `eval` are both in the list on purpose and they are different things:
an `eval` records a **value** and costs the program an evaluation, a `log`
records **text the script wrote** and costs the program nothing. a transcript of
fifty records is readable because the script labelled its own sections

### `run_to` lives here and nowhere else

[the MCP adapter](mcp.md#what-is-not-built) refused to build `run_to` as a tool,
and the reasoning is what puts it here. as a control tool it is either a
composition a **front end** performs — arming a breakpoint of its own and taking
it off again, which is a decision about the program made in an adapter — or a
capability of the core that DAP has no request for at all. and under a deadline
it is unsound: a one-shot breakpoint cannot be removed from a program that is
running, so a timed-out `run_to` would leave the program armed with something
nobody asked for

inside a script the engine owns the whole composition, including the removal:

- the breakpoint is armed under an id **above every one the client's set uses**,
    so a stop that names it is unambiguously the script's. the record says which id
- a location that does not bind **halts the script**, with the binding failure's
    own reason. running to a breakpoint that binds nothing would spend the whole
    wall clock budget arriving nowhere
- every record of a `run_to` says what became of the breakpoint, as `disarmed`

the one case it cannot simply be taken off is a script whose clock ran out with
the program still running: the agent binds breakpoints on a python thread it is
holding, and there is none. so bpd **arms a pause** — which holds the next
thread that reaches a line — takes the breakpoint off on whichever thread that
holds, and the transcript says it did (`paused_to_remove`, with where the pause
landed). that is bpd touching the program without being asked, and it is
reported rather than hidden

if no thread reaches a line even then, the answer is `still_armed`: it names the
file, the line, the id, and the threads that were running python, and says to
wait for the program to stop and set the breakpoints again. **the pause is still
armed too**, and it says that as well. leaving something behind quietly is the
only outcome that is not available

### a predicate is python, and it has to be a `bool`

an `if` or a `while` carries a `predicate` — an expression and the frame to
evaluate it in. it must produce a python `bool`. anything else halts the script
naming the type it produced

that is deliberate, and it is the same rule as everywhere else. truth-testing an
arbitrary object means running the program's own `__bool__` or `__len__` and
branching on the result; re-deriving cpython's truthiness in rust would be a
second implementation of a rule cpython owns; and wrapping the expression in
`bool(...)` would run whatever the program has bound that name to. writing the
comparison down — `x is not None`, `len(items) > 0` — puts it in the transcript,
where a reader can see what was actually asked

### a loop carries its bound, and reaching it stops the script

`while` requires a `limit` and it cannot be zero — it is a `NonZeroU32`, so a
script with an unbounded loop does not even deserialise. a loop without a bound
is a hung session, and a script that cannot be shown to terminate is refused at
submission rather than discovered at runtime

reaching the limit with the predicate still true **halts the script**. the loop
did not finish what it was for, so the steps after it would run somewhere they
did not expect — which is the same rule as a step that fails

## the transcript is the return value

not the final state. a client given only where a script ended cannot tell **why**
it ended there, and will guess

```json
{
  "at_most": 20,
  "bytes": 1204,
  "records": [
    {
      "step": "1",
      "at": { "stop": 1, "thread": 3, "place": null, "why": "entry" },
      "did": {
        "did": "ran_to",
        "line": 40,
        "armed_as": 1,
        "landed": { "landed": "stopped", "to": { "stop": 2, "…": "…" } },
        "disarmed": { "disarmed": "removed" }
      }
    }
  ],
  "outcome": { "outcome": "ran" },
  "partial": false
}
```

- **`step`** is the position in the submitted tree, counting from one, with a
    branch named on the way in: `3` is the third step, `3.then.1` is the first step
    of its `then` block, `4.body.2` is the second step of a `while` body. a loop's
    body records the same path on every pass, and the test before each pass says
    which pass it is
- **`at`** is where the held thread was **when the step ran**, built from a stop
    the agent reported and from nothing else. `place` is `null` only at the entry
    stop, where the program has run nothing and there is no line it is at
- a control step's landing carries `to`, which is the same shape — so a step
    says where it started and where it finished, and neither is a guess
- **`at_most`** is how many records the script could have produced, computed
    before it ran. every loop carries a bound, so this is computable — which is
    the examinability arbitrary python cannot offer

### the budget, and what partial means

`budget` is required and has no default on any axis: a script without one is a
session that can hang, which is the whole reason a step tree exists rather than
submitted python

| axis      | what it bounds                                                                                 |
| --------- | ---------------------------------------------------------------------------------------------- |
| `steps`   | how many steps run — one per record, including each `if` test and each `while` test            |
| `wall_ms` | how long the whole script may take. it is **also the deadline every control step waits under** |
| `bytes`   | how many bytes of transcript are recorded                                                      |

there is deliberately **no per-step deadline**. the wall clock budget is the
one clock: a script waiting for a program that never stops is spending exactly
that, and a second deadline per step would be a second place to say the same
thing

the **byte** budget is usually the first to bite — a value read inside a loop
spends it long before fifty steps have run. it is checked after each record, so
one record can carry the total past it, by at most one value read (whose own
bound is its `detail.budget`). `bytes` on the transcript says what was really
recorded **either way**, so a transcript that went over its bound for some other
reason still says so

exhausting any of the three ends the script and sets `partial: true`, with the
bound that bit, the step it stopped at, and how many records were made. the
steps after it did not run, and nothing in the transcript says anything about
them

### a step that fails halts the script

there is no carrying on past one, and no catch: the steps after a failure would
run somewhere the script did not intend, and the record would describe an
investigation that did not happen. the ways a script halts:

| halted                         | when                                                                             |
| ------------------------------ | -------------------------------------------------------------------------------- |
| `raised`                       | an `eval` or a predicate raised — the exception is the record, and it is the end |
| `not_a_bool`                   | a predicate produced something else                                              |
| `unbound`                      | a `run_to` named a location nothing will stop at                                 |
| `elsewhere`                    | the thread stopped, and not for the reason the step asked for                    |
| `other_thread`                 | a different thread stopped and this one is still running                         |
| `exited`, `ended`, `finishing` | the program ended                                                                |
| `bounded`                      | a loop ran its allowance and its predicate was still true                        |
| `refused`                      | the session would not answer the request the step is made of                     |

a script that expects an expression to raise should test the condition with an
`if` first. there is no `try`, because a step that swallowed a failure is a step
that reported an answer nobody got

## determinism, and its one limit

the same script over the same program state produces the same transcript, so a
script can be re-run to confirm a reading rather than trusting a memory of it.
that is a constraint on what may be *in* one: **no measured duration**, anywhere.
a wall clock reading would make every transcript unequal to every other, and the
budget's own numbers — which are what was asked for rather than what was spent —
say everything a reader needs

the one thing that is not comparable **between two processes** is the
interpreter's identity for a thread. `threading.get_ident` reports the operating
system's number; it happens to repeat between runs on some platforms and is not
something bpd may claim. within one run it is exact, which is what the claim was
about — a script advances the program, so re-running one is always a second run
over a state the first one moved

## what this page does not offer

- **no `set_breakpoints` step.** the breakpoint set belongs to the client, and a
    script that changed it would leave a session whose set is not what the client
    last asked for. a `run_to` puts the set back before it returns, which is the
    only reason the engine touches it at all
- **no nested script, and no `try`.** a nested one would be a second budget
    inside a budget; a `try` would be a failure that did not halt
- **no step for the whole-program `continue`.** a script drives one thread, and
    resuming the others would be a script letting go of threads nobody named
