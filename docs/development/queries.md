# the state query, and the difference between two of them

reading one local through the tree walk is `stackTrace`, then `scopes`, then
`variables`, then `variables` again for each nested object. four round trips,
each one a call and a chunk of a context window spent on protocol scaffolding
rather than on the program

a **state query** is one call that says what is wanted and is answered with it.
a **diff** is one more call that says what changed between two of those answers,
rather than shipping both and leaving the comparison to the reader

both are capabilities of `bpd_core` — `Request::Query` and `Request::Diff` — so
both adapters have them, which the
[parity](architecture.md#the-session-core-and-adapter-parity) rule requires. an
agent calls the `state` and `diff` tools; an editor sends the custom DAP
requests `bpd/state` and `bpd/diff`

## the query is composed of the walk, not a second opinion

it issues the requests the tree walk is made of — a stack walk, a scope read, an
evaluation, a source read — and assembles the answers. so a client that asks
declaratively and one that walks the tree cannot be told different things about
a value. what the query removes is the round trips, not the machinery

`a_query_answers_in_one_call_what_the_tree_walk_answers_in_four` reads the same
stop both ways and compares them frame for frame, entry for entry

```json
{
  "frames": 2,
  "scopes": ["local"],
  "expressions": [{ "expression": "amount * 2", "frame": 0 }],
  "source": 2,
  "detail": { "depth": 3, "budget": 8192 }
}
```

`frames` defaults to **one** — the frame the program is in. every other frame is
a scope read nobody asked for, and the byte budget is spending somebody's
context window. the answer says how deep the stack really is either way

## the budget is one budget

`detail.budget` bounds the **whole** query rather than each read inside it. a
query of twenty parts under eight kilobytes spends eight kilobytes, not a
hundred and sixty

it is checked before each part and charged after, so one part can carry the
total past it — by at most that part, whose own bound is the same `budget`. that
is the rule [the debug script](scripts.md#the-budget-and-what-partial-means)
follows for its transcript, and `bytes` on the answer says what was really spent
either way

a part that did not fit is **not read at all**, and is named:

```text
the global scope of frame 0 was not read: the request's byte budget of 200 ran
out here. ask again with a larger `budget`, or for less of the graph
```

the order decides what a spent budget cuts, so it is fixed rather than
incidental:

1. **the stack**, which is what everything else is addressed by. it is walked
    even for `frames: 0`, because it is also what says how deep the stack is and
    what mode the answer was read in
1. **the expressions**, which are what the client asked for by name
1. then frame by frame, **the source** and **the scopes** — the scopes last
    because they are the open ended part, and one module namespace can spend a
    whole budget on its own

## source is shown only when it can be proved

a debugger that read the bytes on disk and called them the program's source
would be inventing the thing its reader reasons about. files are edited while
programs run, and cpython keeps no copy of what it compiled — `linecache`, which
is what a traceback uses, has exactly this bug

so `source` is answered in the **debuggee**, on the filesystem the interpreter
read the file from, and the file is checked before a line of it is shown: it is
compiled, and the frame's own code object has to be in what comes out. same
qualified name, same first line, same argument count, same names, same variable
names, and the same **line table** — which is the thing that maps an offset to a
line and therefore the thing being relied on

compiling runs none of the program. it is the compiler, on bytes, and a module
that would raise on import raises nothing here. what it costs is a compile of
that one file, per query that asks for source

what is shown is clamped to the verified code object's own lines. an edit
further down a file leaves this code object identical, so its lines are still
proven and lines outside it are not

every way it cannot answer says which:

| why                 | what it means                                                                                                                            |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `not_a_file`        | `co_filename` is not a file — `<string>`, a frozen module, a module in a zip                                                             |
| `does_not_compile`  | what is on disk cannot be what the interpreter compiled, because the interpreter compiled it                                             |
| `not_the_same_code` | the file compiles and this code object is not in it: it has been edited since                                                            |
| `not_utf8`          | it compiled under an encoding it declared, and deciding that encoding again here would be a second implementation of a rule cpython owns |

`a_file_edited_since_the_interpreter_read_it_shows_no_lines_and_says_why` edits
the fixture underneath a held program and asserts that the answer stops showing
lines. with the check removed it shows the **new** lines, one off, which is the
failure this exists to prevent

## a snapshot is a value, and does not go stale

[the agent interface](agent-interface.md#still-open) left open how a snapshot is
addressed across turns without reintroducing DAP's stale handle problem. the
answer is that it is not the same problem

DAP's variable reference is a **promise to read something later**, and that is
why it goes stale the moment the program runs on. a snapshot is the reading,
already taken. nothing the program does afterwards can change it, so its id
stays valid for the life of the session, across any number of resumes

the id is the stop it was taken at and a digest of everything in it —
`2:719ffdc19b892d23…`. both halves earn their place:

- the **stop** makes it self-describing, the same way a
    [frame id](state.md#a-frame-identity-says-which-stop-it-belongs-to) carries
    the stop it was minted at. that is the precedent this follows
- the **digest** makes it content addressed: the same state read twice has the
    same id, so an agent that asked one question twice is told it is holding one
    answer rather than two that happen to agree, and an id names one state or no
    state at all

nothing evicts one. an id that resolved earlier in a session and not later would
be the stale handle problem arriving by another route, so the only way an id
fails is being one this session never gave out — and that is refused by name,
with what it does hold

**every query is kept**, rather than only the ones a client asks to keep.
whether a state is worth comparing is not knowable when it is read

what *does* end with the stop is asking that stop anything more. the
`FrameId`s inside a snapshot name frames that have run on, and using one is
refused by the existing rule. the snapshot goes on being true; what it points at
cannot be asked again

## the diff is the answer

```json
{
  "before": {
    "snapshot": "2:719f…",
    "stop": 2,
    "mode": "non-stop…",
    "stop_has_ended": true
  },
  "after": { "snapshot": "3:bf52…", "stop": 3, "stop_has_ended": false },
  "changed": [
    {
      "subject": {
        "subject": "variable",
        "frame": 0,
        "scope": "local",
        "name": "total"
      },
      "before": { "seen": "value", "…": "…" },
      "after": { "seen": "value", "…": "…" }
    }
  ],
  "added": [],
  "removed": [],
  "unchanged": ["`items` in the local scope of frame 0"],
  "not_compared": []
}
```

four states a name can be in are compared, not one: a **value**, an evaluation
that **raised**, a name that is in the scope and **unbound** at this line, and
one the frame does not **expose**. a local that was unbound and now holds five
has changed, and reporting it as having appeared would be a different claim

three rules keep it from lying, and each of them is a thing it refuses to say:

- **a reading a bound cut short is `not_compared`**, never `unchanged`.
    "unchanged" is a claim, and half a list is not evidence for it. the check is
    recursive — an elision four levels down still counts, because the value
    around it looks whole
- **a depth of the stack running different code is `not_compared`.** depth is a
    position rather than an identity, and comparing `x` of `f` against `x` of
    `g` because both are frame 0 would be a difference about two different
    variables
- **something only one snapshot read is `not_compared`.** what the other holds
    is unknown rather than absent, so it is never reported as added or removed.
    for the same reason, a name missing from a scope read that was **cut** is
    not a removal — nobody looked

everything that was compared and is the same is named rather than carried:
`unchanged` is a list of subjects, not of values, because a diff that shipped
both states is the thing this exists instead of

### what a diff can claim about the world

each side carries the mode it was read in. in `non_stop` a stop holds one thread
and the rest of the program keeps running, so each state is a **sample** and the
difference is between two samples — a moment that may never have been one whole
state. [stopping the world](threads.md#stopping-the-world) is what makes a
reading a whole-program one, and even that names the threads it could not hold

## what this page does not offer

- **no diff of source.** a snapshot's source is the file rather than the
    program's state, and two snapshots whose source differs are two snapshots of
    a file that was edited — which the source check already reports where it
    matters
- **no query of a thread bpd is not holding.** its frames are moving, and the
    rule is [state](state.md#every-read-says-what-was-moving-while-it-was-taken)'s
    rather than this page's
- **no eviction, and so no bound on how many snapshots a session keeps.** each
    is bounded by the byte budget of the query that read it, and the alternative
    is an id that used to work

## how it is tested

`crates/bpd_engine/tests/queries.rs`, against a real interpreter, with a fixture
that calls one function twice with different arguments and writes both results
to disk — so what a diff says changed is asserted against **what the program
computed**, not against the diff. `crates/bpd/tests/mcp.rs` and
`crates/bpd/tests/dap.rs` carry one acceptance each, through the real transport
