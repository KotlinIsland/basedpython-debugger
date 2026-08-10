---
name: bpd
description: debug a running python program with bpd instead of adding print statements — set breakpoints that bind to real code objects, read locals and evaluate expressions at a stop, step, and run whole investigations in one call. use when a python program produces a wrong value, raises somewhere unclear, hangs, or behaves differently than the code reads.
---

# debugging python with bpd

`bpd` is a debugger for cpython 3.13 and newer, driven over MCP. it is not part
of MCP that a client reads a skill — this file is a **client** feature. a client
that has skills should be given it; one that does not still gets everything that
matters from the tool schemas and the errors, which is where the semantics live

## when this is the right tool

reach for it when the question is *what was the program actually doing*, and the
answer is not in the source:

- a value is wrong and it is not obvious where it stopped being right
- an exception comes from somewhere the traceback does not explain
- a program hangs, or a thread stops making progress
- a branch is taken that should not be, or a handler is entered with an argument
  nobody expected

do not reach for it to read code, to run tests, or to check that a function
returns what it says. it starts a process and holds a thread of it, which is
worth doing when the alternative is guessing

## setting it up

`bpd mcp` is a stdio MCP server with no flags — everything a session needs
arrives in the `launch` tool call:

```json
{ "mcpServers": { "bpd": { "command": "bpd", "args": ["mcp"] } } }
```

## the shape of a session

1. **`launch`** the program. it is held **before its first statement**, which is
   where a breakpoint binds against a real interpreter rather than a guess about
   one. name the `python` explicitly: the default is `python3`, which on a great
   many machines is older than 3.13 and is refused
2. **`set_breakpoints`** while something is held. it replaces the **whole** set,
   so everything that should stay set goes in the same array. read the answer —
   it says the line each one really bound to, and a breakpoint that did not bind
   says why and is never reported as set
3. **`continue_`** with a `deadline_ms`. the answer **is** the stop: why it
   stopped, which thread, and the top of its stack. there is no event to wait for
4. **`state`** to describe the stop in one call — frames, scopes, expressions,
   and the source around each line. the tree walk is still there as `stack`,
   `variables` and `evaluate`, and answers identically; `state` removes the round
   trips
5. **`step_over` / `step_in` / `step_out`**, each one call and one answer, or
   **`run_script`** for a whole investigation in one

## the five things that are easy to get wrong

- **a timeout is not a location.** `outcome: "timed_out"` carries no thread, no
  frames and no reason, because everything the agent inside the debuggee answers
  it answers on a thread it is **holding** — a running program cannot even be
  asked what its threads are doing. resuming again with a larger deadline gives
  the same answer later. call `pause` instead, which holds the next thread that
  reaches a line and makes everything askable again
- **a stop holds one thread, not the program.** everything else keeps running, so
  several stops can be outstanding and a tool that is about one names it. the
  held thread's own frame chain is a snapshot; every value reached through it is
  a sample, and `stop_the_world` is how to ask for the other mode
- **do not step n times to reach the nth call.** put the count in the breakpoint:
  `"hits": {"hits": "exactly", "count": 3}`, with a `condition` when only some
  hits should qualify. the debuggee evaluates both, so it costs one round trip
  instead of n
- **do not step and evaluate in turn.** `run_script` takes a tree of steps with
  its own branching and returns the transcript of every one of them. a
  `while` loop over `step_over` with a python predicate is one call
- **an expression that raises is an answer, not a failure.** it comes back
  carrying the exception, because the interpreter is the authority on what an
  expression is. so is one that does not compile

## when something is refused

read the refusal rather than retrying. every one names a cause and an action:
which stops are held now, which scope a name is really in, why a breakpoint did
not bind, what the program exited with. a call that is refused for an argument
names the tool and the argument, and no argument is ever accepted that the schema
does not name — a misspelling is refused rather than silently defaulted

## the deeper model

the server also offers two MCP **resources**, for a host that pulls them:
`bpd://model/stops` is what a stop claims and does not claim, and
`bpd://model/values` is what a value read claims and what it left out. four
**prompts** carry canonical investigations — `nth_call`, `step_until`,
`what_changed` and `why_wont_it_stop`
