# why the ui recomposed

the runtime of basedpython-ui keeps a record of why every scope ran, what every
state write did and what every frame cost. `bpd` reads it at a stop, and
forwards it while a client watches: `bpd/recompositions` and
`bpd/watchRecompositions` over DAP, `recompositions` and `watch_recompositions`
over MCP

```json
{ "command": "bpd/recompositions", "arguments": {} }
```

```json
{
  "format": 1,
  "runtimes": 1,
  "tracing": true,
  "records": {
    "kept": [
      {
        "record": "write",
        "runtime": 0,
        "frame": 3,
        "cause": {
          "cause": "state",
          "cell": 4401,
          "kind": "state",
          "op": "set",
          "at": null,
          "old": "0",
          "new": "2",
          "declared": {
            "file": "/app/counter.by",
            "line": 12,
            "generated": { "file": "/tmp/build/counter.py", "line": 44 }
          },
          "declared_name": "count",
          "written": {
            "file": "/app/counter.by",
            "line": 14,
            "generated": { "file": "/tmp/build/counter.py", "line": 46 }
          },
          "thread": 8674,
          "posted": false,
          "readers": 1
        }
      },
      {
        "record": "run",
        "runtime": 0,
        "frame": 3,
        "scope": 5,
        "parent": 0,
        "name": "Counter",
        "defined": {
          "file": "/app/counter.by",
          "line": 9,
          "generated": { "file": "/tmp/build/counter.py", "line": 41 }
        },
        "called": {
          "file": "/app/counter.by",
          "line": 24,
          "generated": { "file": "/tmp/build/counter.py", "line": 90 }
        },
        "key": null,
        "origin": "self",
        "causes": [{ "cause": "state", "...": "the write above, whole" }],
        "skipped": [7, 8],
        "disposed": [{ "scope": 9, "name": "Row", "key": 2 }],
        "elapsed_ns": 12345
      },
      {
        "record": "frame",
        "runtime": 0,
        "frame": 3,
        "runs": 2,
        "skips": 4,
        "compose_ns": 15000,
        "commit_ns": 300000
      }
    ],
    "dropped": 0
  },
  "mode": { "mode": "non_stop" }
}
```

the runtime wrote every one of those fields; `bpd` added nothing but the
mapping. that is the whole design, and the rest of this page is what it costs to
keep it true

## a record is a tuple the runtime wrote, read by its layout

the runtime keeps a ring of tuples. the first slot of each is its kind, and the
kinds are the five above: a **run** (a scope ran, with every reason it ran for),
a **write** (a state cell was written, or a derived recomputed), a **frame**
(one finished, and what it cost), an **error** (a scope raised) and a
**refused** (a write during composition was refused). every other slot is an
exact builtin of one documented type — an `int`, a `str`, a `bool`, `None`, a
tuple — and the layouts are fixed under a format number the runtime declares as
`TRACE_FORMAT`

`bpd` compares the format before reading anything, and reads every slot by the
layout of that format. a slot holding anything else refuses the whole answer,
naming the record kind, the slot and what was there:

```text
the trace record could not be read: slot 1 (`frame`) of a `run` record is a str and the layout says a non-negative int
```

a `bool` in an `int` slot is refused too. `True` is an `int` to `isinstance`
and it is not a frame number, so every check is against the **exact** type —
which is also what keeps a subclass with its own `__index__` out of a number

**it runs none of the program.** the module is found in `sys.modules` and never
imported, because importing a package the program never asked for is the
debugger changing the program — and `sys.modules` itself is read through
`PySys_GetObject`, a dictionary lookup on the `sys` module, never through the
import machinery. every object on the way to the ring — the module's globals,
each runtime, its trace — is read through its instance dictionary with
`PyObject_GenericGetDict`, never as an attribute, because an attribute read is
the class's `__getattribute__`. the runtime promises ordinary instance
dictionaries and exact lists for exactly this reason

the value slots — `old` and `new` of a state cause, of an args cause and of a
derived cause — are whatever the program stored, and they are the one place a
reader could run the program to describe it. they are rendered by exact type
instead, and never by calling anything of the object's: an exact builtin scalar
as itself (`"2"`, `"'abc'"`, `"None"`, `"True"`), an exact builtin container by
kind and size (`"list[3]"`, `"dict{2}"`, `"tuple[0]"`), anything else as
`"a Todo"`. text is cut at 64 characters with `…`. that is the trail's rule, and
it is the weaker answer that cannot be wrong —
`the_ring_is_read_slot_by_slot_with_every_kind_of_record_and_cause` stores an
object whose `__repr__`, `__str__`, `__len__` and `__bool__` all record being
called, and asserts the program's own list of calls is empty afterwards

a text slot — a name, an error, a `what` — is a `str` the layout says is one.
a `str` holding a lone surrogate (`str(OSError)` over a `surrogateescape`d file
name is one) has no UTF-8 spelling, and it is spelled the way `repr` spells it,
`\udcff` — which is what a value slot already gets through its `repr` — rather
than the record being refused with a sentence about its type that is not true.
`a_text_slot_no_utf8_can_spell_is_read_with_the_character_spelled_as_repr_would`
writes one

## every location goes through the map a frame goes through

the runtime records the generated python it runs and never maps it. `bpd` maps
every location in a record — where a composable is defined, where it was called
from, where a cell was declared, where it was written — through the build's
source map, exactly as it maps a stack frame, and reports it in the same
three-way vocabulary:

```json
{
  "file": "/app/counter.by",
  "line": 24,
  "generated": { "file": "/tmp/build/counter.py", "line": 90 }
}
```

`file` and `line` are the `.by` location when the map covers the generated file,
and the generated location itself otherwise. `generated` is always where the
interpreter was, and is `null` only when `file` already is it — nothing mapped
the file. a generated line the map marks as prelude keeps the generated location
in `file` and `line`, carries it under `generated` as well, and adds `reason`,
the map's own account of why no `.by` line is behind it. a `.by` line invented
for one would be a line the user never wrote.
`a_build_with_a_source_map_reports_every_location_of_the_ring_as_by_lines` puts
all three shapes in one record

## the causes

a run carries every reason it ran for, in the order they happened. a scope
popped from the dirty heap carries every state and derived cause recorded for it
since its last run, so a handler that wrote three cells is three causes on one
run — the runtime records the cause before its own de-duplication

| `cause`       | what it carries                                                                                                                                     |
| ------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `created`     | nothing: first composition of a new scope                                                                                                           |
| `state`       | `cell`, `kind`, `op`, `at`, `old`, `new`, `declared`, `declared_name`, `written`, `thread`, `posted`, `readers` — a cell the scope read was written |
| `derived`     | `derived`, `declared`, `declared_name`, `old`, `new`, `changed`, and `because`, the state or derived cause that made it recompute                   |
| `invalidated` | nothing: `Runtime.invalidate` was called with no cause                                                                                              |
| `args`        | `parameter`, `old`, `new`, and `compared` — false when the argument's type is unstable and the two were never compared                              |
| `inline`      | nothing: the scope takes a content block, so it runs whenever its parent does                                                                       |
| `recovery`    | `error`, what the previous run raised                                                                                                               |
| `uncommitted` | nothing: the scope was created in a frame whose commit did not happen                                                                               |
| `dirty`       | `causes`, the ones that had already made it dirty when its parent reached it                                                                        |

`declared` and `declared_name` are `null` for a cell created outside
composition. `readers` of zero on a write record is the record that says nothing
depended on the cell — the answer to "why did this **not** rerender"

a key change has no cause of its own, because whether an old key was really
given up is known only when the parent's run ends: the new scope carries
`created`, and the parent's run record names the old key under `disposed`. a
runtime writing a `key` cause is writing a layout this `bpd` does not have, and
it is refused as one. `at` of a state cause is `null`, an integer or a string:
the runtime writes a dict key of any other type as its `repr` before the slot
is written, and refuses any `key(...)` value that is not an `int` or a `str`, so
neither the `at` slot nor a `key` slot ever holds a program object

## the bounds, and the rule about them

the runtime's ring is bounded, and so is the answer. the runtime keeps
`Trace.limit` records — 4096 by default — and halves the ring when the append
that reaches the limit lands, counting what went in `Trace.dropped`. `bpd` keeps
at most 4096 records per answer, oldest first with the newest kept, and counts
what it left out. both counts are one number:
`records.dropped` is the runtime's own plus the answer's, and an answer whose
`dropped` is above zero does not begin where the trace did. `records` is a
`bpd_core::Kept` for the reason everything bounded here is — a front end cannot
render the list without having been handed the count.
`an_answer_holds_the_newest_records_and_counts_every_one_it_left_out` overflows
both bounds at once and reads one number back

a program with several runtimes is answered about all of them: the records are
concatenated in `live_runtimes` order, each carrying `runtime`, the index of its
own. a program ordinarily has one

## what is refused, by name

| refused               | when                                                                          |
| --------------------- | ----------------------------------------------------------------------------- |
| `no_ui_runtime`       | `basedpython_ui.runtime` is not in `sys.modules`                              |
| `ui_tracing_off`      | every runtime has `trace = None`                                              |
| `ui_trace_format`     | the module declares a `TRACE_FORMAT` this `bpd` does not read, or none at all |
| `ui_trace_unreadable` | a record slot holds a type the layout does not allow                          |

none of them is an empty answer. a program that never imported the runtime has no
ring; one whose runtime has tracing off has an empty one that would read as a ui
that never recomposed; one writing another format has slots that mean something
else. the refusal for tracing is made only when **every** runtime is off — a
second runtime that traces is answered, with the quiet one counted among
`runtimes` and nothing of it among the records. and a program that imported the
runtime and made no runtime yet is answered rather than refused: `runtimes` is
zero and `tracing` is false, which is the only case `tracing` is false in an
answer. `a_record_with_a_slot_the_layout_does_not_allow_is_refused_naming_the_slot`
and `a_trace_format_this_bpd_does_not_read_is_refused_naming_both_formats` are
the refusals, read

`ui_trace_format` for a module with no `TRACE_FORMAT` at all carries
`found: null` and a sentence of its own — `the program's basedpython_ui writes
no trace format at all — basedpython_ui.runtime has no TRACE_FORMAT — and this
bpd reads 1` — because a name that is missing is a different thing to act on
from a number that is wrong. an entry under the runtime's name in `sys.modules`
that is not a module — a lazy proxy, a test double — is `ui_trace_unreadable`
naming what was there, not `no_ui_runtime`: a program told to import something
it imported has been told something false.
`a_non_module_under_the_runtimes_name_is_refused_naming_what_was_there` puts an
`object()` there. and none of these refuses the **watch** — see below

## the stream

the runtime announces every record it appends: `sys.audit("basedpython_ui.trace", record)`,
with the record tuple as the one argument, on the thread that appended it. the
agent's native audit hook — the one that already sees every audit event the
process raises, for [child processes](subprocesses.md) — recognises the name
beside the process-making events. the whole list lives in
`bpd_core::audit::watched`, per purpose, so the parity suite reads the same names
the hook compares against;
`the_trace_event_is_watched_beside_the_process_making_ones_and_is_none_of_them`
is the guard on the two purposes staying apart

with the watch **off**, which is the default, an announced record costs the
program one name comparison and one atomic load. with it on, the agent reads the
tuple off the event on the thread that appended it, with the GIL held, by the
same layout and under the same bounds as a read at a stop — and hands the
rendered record to a queue. **it never writes the connection from that thread.**
the engine reads the connection only inside a request, so a client that resumes
the program and then thinks for a while leaves the socket full, and a write on
the ui thread would then wait for the client's next call: measured before the
queue existed, a ui that recomposed while an agent was idle froze for exactly as
long as the agent was idle. a thread of the agent's own takes records off the
queue and writes them, polling for room first, and the queue's lock is held for
one push or one pop and never across the write

the queue holds 1024 records. when it is full the **oldest** is dropped and
counted, and the count rides the next record that gets through as
`dropped_before` — zero when nothing was dropped — so a client reading the
stream knows where its gaps are, record by record. oldest rather than newest,
because the newest is the one a client watching the ui recompose is waiting for,
and the ring read at a stop is where the oldest can still be found.
`a_watched_program_nobody_reads_runs_at_its_own_pace_and_the_gap_is_counted`
turns the watch on, resumes, reads nothing for three seconds while the program
writes forty thousand records, and then asserts on the program's own clock that
its loop took a fraction of that — and that what was forwarded plus what was
counted is exactly forty thousand, in order. the drop-oldest-and-count rule on
its own is
`a_full_queue_drops_the_oldest_and_counts_it_onto_the_next_record_taken`

which runtime wrote a record is found by identity: the runtime appends before it
announces, so the announced tuple is the **last** entry of that runtime's ring,
and nothing but the last entry is compared. a tuple that is not the last entry
of any live runtime's ring, or that does not read by the layout, is not
forwarded — the stream carries the runtime's own records, and a program raising
the event by hand is not the runtime. a stale tuple re-raised from the front of
the ring would show a client the ui going back in time;
`a_record_the_program_announces_by_hand_is_not_forwarded` raises one, and a
tuple in no ring, and reads neither. that is a stated limit rather than a
silence: the ring read at a stop is where such a record is refused by name

the hook runs none of the program while it does this. `sys.modules` is read
through `PySys_GetObject`, a dictionary lookup on the `sys` module;
`python.import("sys")` would be `PyImport_Import`, which calls
`builtins.__import__` — a function the program can replace and would then see
the debugger calling once per record, from inside `sys.audit`.
`forwarding_a_record_runs_no_import_machinery_in_the_program` wraps
`builtins.__import__` with a spy while five records are forwarded and reads the
spy's empty list back

a watch is **accepted before the program has imported the runtime**: watching
is an interest in records to come, and a program that imports the runtime later
is watched from then on — an editor that turns the watch on when the session
starts, before a line has run, sees the first frame. only the read at a stop
needs a ring to exist. over DAP that is earlier still: a watch asked for before
`launch` is held by the adapter and turned on with the breakpoints — see
[the configuration phase](dap.md#the-configuration-phase-happens-before-there-is-a-program)
`watching_forwards_every_record_the_runtime_announces_and_nothing_once_it_is_off`
turns it on at the entry stop, reads the record the program announces the moment
it has a runtime, turns it off before a third stop, and the sink it runs to the
third with panics on any report

### across a fork, and at the end

the thread that writes the stream is a second thread of the agent's, and it goes
the way the control connection's reader goes — see
[subprocesses](subprocesses.md): stood down before a fork, so cpython's count of
the process's threads is the count a bare run has, and put back in the parent
afterwards, with whatever was queued still queued for it. a forked child starts
with the watch **off** and with a queue and a writer of its own, replaced without
a lock the way every other cell of the session is: a child that inherited its
parent's watch would forward its own ui's records onto the session that asked
about the parent's.
`a_forked_child_starts_with_the_watch_off_and_is_watched_on_its_own` forks under
`debug_children` with the parent watched, reads nothing from the child until the
child's own session asks, and then reads the child's next record — and the
parent's, written after the fork by a writer that was stood down for it

two things do wait, and both are about a frame rather than the program. a write
that is in flight when a fork begins is finished before the writer is joined,
because a length-prefixed frame abandoned half-written would desynchronise the
whole connection — so a fork begun at that instant waits for the engine's next
read. and at the program's end what is still queued is written before the end is
reported, because a record dropped on the way out would be one nothing counted;
that holds the **exit** for the engine's next read, and nothing of the program,
which is over by then

## the record is the same thing however it arrives

the parity rule holds in both directions here. the ring is reached by both front
ends — `bpd/recompositions` and the `recompositions` tool — and so is the
watch. what the debugger **says** while watching is `bpd_core::Told::Recomposed`,
and each front end carries it the way it carries everything unasked:

| front end | how a record arrives                                                                                                                                                                                                                                                                                |
| --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| DAP       | a `bpd/recomposition` event carrying `{ "record": …, "dropped_before": N }` when the client named it in `bpd/understands`; otherwise one `output` line on the `console` category per **run** record, nothing for a write or a frame, and one on `important` whenever `dropped_before` is above zero |
| MCP       | a `recompositions` key on the answer to whichever call the program was running during: `{ "records": [ … ], "dropped": N, "says": "…" }`, keeping at most two hundred between calls. `dropped` counts both what that bound left out and what the agent's queue dropped, and `says` names which      |

a DAP client that reads the data is not narrated at, because a client shown both
shows every record twice — the `bpd/restarting` shape, and
`a_client_that_opts_into_the_recomposition_event_is_handed_the_record_as_data`
drives both routes in one conversation and asserts every narration precedes the
first event. MCP's key is its own rather than part of `logged`, because an agent
that found a trace record there would read it as a logpoint firing;
`the_ring_and_its_edge_reach_an_agent_and_a_watched_record_rides_the_next_answer`
reads both the ring and the key back off a driven server. there is no `JUSTIFIED`
or `SILENT` entry for any of it: both protocols carry both halves

MCP adds `says` beside the structure of the ring, naming the count and what fell
off, the way the `trail` tool does; DAP says the same on the `important` category
when anything did

## how it is tested

everything above is checked against a real interpreter, in
`crates/bpd_engine/tests/recompositions.rs`. the ring is written by a
**stand-in** runtime the fixture ships beside itself — a `basedpython_ui.runtime`
of the documented layouts and nothing else, halving its ring exactly as the real
`Trace.append` does — so each test writes the records it means to read back, one
of every record kind and every cause kind among them, and the answer is compared
field by field. a stand-in is what makes a test of "a `str` in an `int` slot is
refused by name" a test somebody runs, and the stand-in tests run unconditionally

the one test over the framework itself is
`the_real_runtime_writes_a_run_whose_cause_is_the_state_write_a_handler_made`.
it composes a counter through the real `basedpython_ui.runtime.Runtime`, clicks
its button, runs a second frame, and asserts the run record's cause is the state
write of the cell bound as `count`, at the handler's own line. it runs the build
`BPD_TEST_BASEDPYTHON_UI` names — the `out` of `by build` in a checkout of
basedpython-ui — and it is `#[ignore]`d, so a plain `cargo test` on a checkout
with no framework beside it reports it as ignored rather than failing. it is run
on purpose:

```sh
BPD_TEST_BASEDPYTHON_UI=/path/to/basedpython-ui/out \
    cargo test -p bpd_engine --test recompositions -- --ignored
```

run that way with the variable unset it **fails and says what to set** rather
than skipping, for the reason the django tests do: a test that quietly passed
because nobody pointed it at a framework would be reporting success while
proving nothing

one acceptance in each front end drives the stand-in through a real `bpd dap`
and a real `bpd mcp`:
`an_editor_can_ask_why_the_ui_recomposed_and_be_handed_the_next_record_as_data`
and
`an_agent_can_ask_why_the_ui_recomposed_and_reads_the_next_record_off_the_answer_it_rode`.
what they assert is pinned against the engine first by
`the_acceptance_program_writes_the_ring_the_front_ends_read`, so a record that
moved fails where it moved rather than behind a protocol
