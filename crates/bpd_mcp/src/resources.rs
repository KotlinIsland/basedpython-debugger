//! the model behind the tools, for a host that pulls it
//!
//! a resource is read at the **host application's** discretion, so nothing here
//! may be load bearing: an agent that never sees one still has to be able to
//! use bpd correctly from the tool schemas and the errors alone. what belongs
//! here is the part those two cannot carry — not what a call takes, but what
//! its answer *claims*, and where the claim stops
//!
//! two pages, and no more, because a third would be padding. every sentence in
//! them is a fact about what this session does, and [`Resource::mentions`] is
//! how that
//! stays true: every tool a page names is checked against the tool table, in
//! both directions

/// one resource, as `resources/list` reports it
#[derive(Debug, Clone, Copy)]
pub struct Resource {
    /// the uri a `resources/read` names
    pub uri: &'static str,
    /// the name a host shows
    pub name: &'static str,
    /// a short human label
    pub title: &'static str,
    /// what is in it, so a host can decide whether to pull it
    pub description: &'static str,
    /// the page itself
    pub text: &'static str,
    /// every tool this page names, so that a renamed one breaks a test
    ///
    /// checked both ways by `crates/bpd_mcp/tests/teaching.rs`: each entry has
    /// to be a tool this server offers *and* has to appear in `text`, and a
    /// tool named in `text` has to be here. what it cannot catch is a name that
    /// never existed, which is a typo rather than a drift
    pub mentions: &'static [&'static str],
}

impl Resource {
    /// this resource as the object `resources/list` carries
    pub fn listing(&self) -> serde_json::Value {
        serde_json::json!({
            "uri": self.uri,
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "mimeType": MIME,
        })
    }

    /// this resource as the object `resources/read` carries
    pub fn contents(&self) -> serde_json::Value {
        serde_json::json!({
            "uri": self.uri,
            "mimeType": MIME,
            "text": self.text,
        })
    }
}

/// what every resource here is
const MIME: &str = "text/markdown";

/// every resource this server offers
pub fn resources() -> Vec<Resource> {
    vec![
        Resource {
            uri: "bpd://model/stops",
            name: "what-a-stop-claims",
            title: "what a stop claims, and what it does not",
            description: "the thread model behind every answer: why a stop holds one \
                 thread and not the program, what stop-the-world adds and what \
                 it still cannot reach, why a timeout carries no location at \
                 all, and what a stop number, a frame depth and a snapshot id \
                 each stay valid for",
            text: STOPS,
            mentions: &[
                "continue_",
                "diff",
                "pause",
                "resume",
                "stack",
                "state",
                "stop_the_world",
                "threads",
                "wait",
            ],
        },
        Resource {
            uri: "bpd://model/values",
            name: "what-a-value-read-claims",
            title: "what a value read claims, and what it left out",
            description: "how to read an answer about a value: why the four scopes are \
                 never merged, why an `int` arrives as text, what every bound \
                 that bit is called where it bit, why `repr` is off, why source \
                 is proved rather than read from disk, and what a diff refuses \
                 to call unchanged",
            text: VALUES,
            mentions: &["diff", "evaluate", "set_variable", "state", "variables"],
        },
    ]
}

/// what a stop claims, and what it does not
const STOPS: &str = r#"# what a stop claims, and what it does not

every answer bpd gives is taken on a thread it is **holding**. that one sentence
decides everything below, including several things that look like omissions and
are not

## a stop holds one thread, not the program

a stop holds the thread that reached it. every other thread in the program goes
on running, on every build — the agent releases the GIL for the duration of a
stop and takes it back only to answer a request. that is bpd's default because a
live server should go on serving while one of its handlers is inspected

two consequences an agent has to hold:

- **several stops can be outstanding at once.** a second thread reaching a
  breakpoint reports its own stop straight away rather than queueing behind the
  first. so a tool that is about one held thread names it, and when several are
  held and none is named the call is refused with the list
- **a stop number ends when its thread is resumed.** `continue_` and `resume`
  end every stop they let go. asking about one afterwards is refused by name
  rather than answered against whatever is at that depth now, and the refusal
  says to ask `stack` again at the stop that is held

what a held thread still holds is reported where it is knowable: a thread inside
the import system holds cpython's per-module lock, and any other thread
importing the same module is blocked until this one is resumed. cpython makes
nothing else about that knowable, so nothing else is claimed

## a snapshot, a sample, and which one you have

in the default mode the held thread's **own frame chain** is a snapshot: it is
inside a monitoring callback and cannot return, so its frames cannot go away
underneath the walk. every *value* reached through those frames is a **sample** —
another thread can change a list between its length being read and its contents
being read, and an answer that pretended otherwise would describe a state the
program was never in

`stop_the_world` asks for the other mode, until the stop it names is resumed. it
holds every thread it can and names the ones it could not: a thread parked in a C
call has released the GIL and reaches no monitoring event, so nothing available
here can stop it. **only an empty `native` is a whole-program claim.** every read
carries the mode it was taken in, so an answer never has to be guessed about

## a timeout is not a stop, and carries no location

every control tool requires a `deadline_ms`, and when it passes the answer is
`outcome: "timed_out"`. it carries no thread, no frames and no reason — not
even a sampled one

that is the thread model rather than an omission. everything the agent answers,
it answers on a thread it is holding, and that includes the thread census. a
program with nothing held cannot be asked what its threads are doing, so there is
nothing to label as stale and nothing to report. what to do about it is in the
answer: `wait` keeps waiting and touches the program in no way at all, and
`pause` arms a line event for the whole program and holds the first thread that
reaches one

`pause` is the only thing that can be asked of a program with nothing held, and
what it catches belongs to the operating system. its `running` counts only
threads bpd is **not** already holding, so an empty one has two causes — every
other thread is parked in a C call, or the threads that would reach a line are
the ones already held — and the answer's `note` says which

## nothing here is a handle

DAP hands a client an opaque reference that looks the same before and after a
resume, so a stale one gets answered. there is nothing like that here:

| what | valid for |
| --- | --- |
| a stop number | until that thread is resumed |
| a frame depth | the stop it was reported at, and no other |
| a snapshot id | the whole session |

a snapshot id outlives its stop because it is not a handle either. it names a
reading that has **already been taken** rather than a promise to take one, so
nothing the program does afterwards can change what it resolves to — which is
why `state` and `diff` can compare two stops any distance apart. what ends with
the stop is asking that stop anything *more*: the frame depths inside a snapshot
name frames that have run on, and those are refused

## what `threads` is, and what it is not

`threads` is the only question that is about threads bpd is not holding, and
everything it says about one is a sample. two are taken `settle_ms` apart and
compared, and `still` means the thread was in the same place both times

that is where to look, not what is wrong. cpython exposes no owner for a lock, so
bpd cannot say that a thread is waiting for one another thread holds — a thread
blocked in `sock.recv` and a thread piled up behind a lock look identical from
here. and it needs a held thread to answer on, like everything else

## a program that will not exit

`continue_` can answer `finishing`: the program ran to its end with threads still
held. it cannot exit, because the interpreter finalizes by joining its non-daemon
threads and a held thread cannot be joined. the answer names them, and `resume`
is what lets it finish
"#;

/// what a value read claims, and what it left out
const VALUES: &str = r#"# what a value read claims, and what it left out

## the four scopes are never merged

python resolves a name by **which scope it is in**, decided at compile time. so
`variables` and `state` read the four separately and never merge them:

| scope | what it is |
| --- | --- |
| `local` | the frame's own locals |
| `cell` | the locals a nested function captures |
| `free` | the variables this frame captures from an enclosing one |
| `global` | the module namespace |

a captured argument is in `local` **and** `cell`, because cpython says it is
both. a merged mapping would have to pick one and would be reporting a scope the
compiler did not give the name

each read separates three things a mapping would lose: what the scope holds,
names the scope has that hold nothing at this line (`unbound`), and names whose
value the frame does not expose (`unreadable`). the third is real: a value can
live in a cell that only the function object holds, which is how a class body
sees a variable of the function around it

## a value says what kind of number it is

a value arrives as `{"kind": ..., "content": {...}}`, and an `int` is carried
as **text** inside it. a python `int` has no width, and a json number that
silently became a float would be a different value

two bounds are off or on for a reason rather than for taste:

- **`repr` is off.** calling `__repr__` runs arbitrary user code, bpd cannot
  interrupt it once it has started, and one that hangs hangs the debuggee
- **`attributes` is on.** reading an instance dictionary is storage rather than
  behaviour — it never reaches `__getattr__` or a property. a type is free to
  make `__dict__` its own code, which is the only reason it can be turned off

## every bound that bit is named where it bit

an agent cannot see an elision that a person would notice, so nothing is quietly
shorter than it looks. a bound that cut something says so at the place it cut,
with how much there was and which field to raise: `left_out` on a scope read or a
whole `state` query, `frames_omitted` on a bounded stack walk, `omitted` inside a
value

`detail.budget` bounds the **whole** of a `state` query rather than each read in
it — a query of twenty parts under eight kilobytes would otherwise spend a
hundred and sixty. the parts are read in a fixed order: the stack, then the
expressions, then frame by frame the source and the scopes. so what a spent
budget cuts is the open ended part, and what it cut is named rather than absent

## source is proved, not read

the file on disk is not evidence of what the interpreter compiled — files are
edited while programs run, and `linecache` has exactly this bug. so `state` reads
source **in the debuggee** and proves it: the file is compiled there and the
frame's own code object has to be in what comes out, line table included. a file
that has been edited since says so instead of showing a line one off, and the
window is clamped to the code object that was verified because nothing outside it
was checked

## an expression is the program running

`evaluate`, a breakpoint condition, a script predicate and the `expressions` of a
`state` query all run the program's own code, by request, on the held thread.
that thread's breakpoints are suppressed while it runs

an expression that raises, and one that does not compile, are **answers** rather
than failures: they carry the exception, because the interpreter is the authority
on what an expression is. a debugger that turned a raise into `None` would be
inventing a value

`set_variable` writes only a name the scope **already has**. a name the code
object does not have is refused, because `f_locals` accepts such a write and
keeps it while the compiled function goes on reading its fast locals — bpd would
be reporting a change the program never received. writing something *inside* a
value is not offered at all: that means running the program's own `__setattr__`
or `__setitem__`, which is the program rather than the debugger

## what a diff refuses to call unchanged

`diff` compares two snapshot ids and answers with the difference. three rules keep
it from lying:

- a value that a bound cut short in **either** snapshot is `not_compared`, never
  `unchanged`. half a list is not evidence that a list did not change
- a depth of the stack running different code in the two is not compared either.
  depth is a position, not an identity
- something only one of the two read is unknown rather than absent, so it is
  never reported as added or removed

each side says the mode it was read in. in the default mode the rest of the
program was running, so each state is a sample and the difference is a difference
between two samples
"#;
