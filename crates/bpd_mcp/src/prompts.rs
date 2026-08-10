//! canonical investigations, as `prompts/get` returns them
//!
//! a prompt is invoked by the **user**, usually as a slash command, so nothing
//! here may be the only place something is said. the bar for one is narrower
//! than "a thing you can do with bpd": it has to be an investigation a competent
//! agent would otherwise get **wrong** or do the long way, which is why there
//! are four and not fifteen. a prompt that restates a tool name is a slash
//! command that costs a user a keystroke and teaches nothing
//!
//! each one carries the whole investigation, with the arguments substituted —
//! `crates/bpd_mcp/tests/teaching.rs` requires that every argument a prompt
//! declares really reaches its text, because a parameter accepted and ignored is
//! the placeholder ban applied to a workflow

use std::collections::BTreeMap;

/// one argument a prompt takes
#[derive(Debug, Clone, Copy)]
pub struct Argument {
    /// the name `prompts/get` passes it under
    pub name: &'static str,
    /// what it is
    pub description: &'static str,
    /// whether the investigation can be written without it
    pub required: bool,
    /// what stands in for it when it is not given
    ///
    /// `None` for a required one. an optional argument with nothing to fall back
    /// on would be a branch in the text nobody wrote
    pub fallback: Option<&'static str>,
}

/// one canonical investigation
#[derive(Debug, Clone, Copy)]
pub struct Prompt {
    /// the name a `prompts/get` uses
    pub name: &'static str,
    /// a short human label
    pub title: &'static str,
    /// what it investigates, and why doing it by hand goes wrong
    pub description: &'static str,
    /// what it takes
    pub arguments: &'static [Argument],
    /// the investigation, with `{name}` where an argument goes
    pub body: &'static str,
    /// every tool this investigation names, so that a renamed one breaks a test
    ///
    /// checked both ways by `crates/bpd_mcp/tests/teaching.rs`, exactly as
    /// [`crate::resources::Resource::mentions`] is
    pub mentions: &'static [&'static str],
}

impl Prompt {
    /// this prompt as the object `prompts/list` carries
    pub fn listing(&self) -> serde_json::Value {
        let arguments: Vec<serde_json::Value> = self
            .arguments
            .iter()
            .map(|argument| {
                serde_json::json!({
                    "name": argument.name,
                    "description": argument.description,
                    "required": argument.required,
                })
            })
            .collect();
        serde_json::json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "arguments": arguments,
        })
    }

    /// this prompt filled in, as the object `prompts/get` carries
    ///
    /// # errors
    ///
    /// when a required argument was not given. an investigation with a hole in
    /// it would be a workflow that names a file and does not say which
    pub fn filled(&self, given: &BTreeMap<String, String>) -> Result<serde_json::Value, String> {
        let mut text = self.body.to_string();
        for argument in self.arguments {
            let value = match (given.get(argument.name), argument.fallback) {
                (Some(value), _) => value.clone(),
                (None, Some(fallback)) => fallback.to_string(),
                (None, None) => {
                    return Err(format!(
                        "`{}` needs `{}`: {}. without it this investigation \
                         would be written with a hole in it",
                        self.name, argument.name, argument.description
                    ));
                }
            };
            text = text.replace(&format!("{{{}}}", argument.name), &value);
        }

        Ok(serde_json::json!({
            "description": self.description,
            "messages": [ {
                "role": "user",
                "content": { "type": "text", "text": text },
            } ],
        }))
    }
}

/// a required argument
const fn needed(name: &'static str, description: &'static str) -> Argument {
    Argument {
        name,
        description,
        required: true,
        fallback: None,
    }
}

/// an optional argument, and what stands in for it
const fn optional(
    name: &'static str,
    description: &'static str,
    fallback: &'static str,
) -> Argument {
    Argument {
        name,
        description,
        required: false,
        fallback: Some(fallback),
    }
}

/// what `nth_call` takes
const NTH_CALL_ARGS: &[Argument] = &[
    needed("file", "the file the line is in, as a path"),
    needed("line", "the line to stop at"),
    needed("count", "which matching hit to stop on, counting from one"),
    optional(
        "condition",
        "a python expression that has to be true for a hit to count. omit to \
         count every hit of the line",
        "true for every hit",
    ),
];

/// what `step_until` takes
const STEP_UNTIL_ARGS: &[Argument] = &[
    needed("file", "the file the line is in, as a path"),
    needed("line", "the line to start stepping from"),
    needed(
        "predicate",
        "a python expression that is true while stepping should go on. it has \
         to produce a `bool`",
    ),
    optional(
        "limit",
        "the most steps the loop may take before it gives up",
        "200",
    ),
];

/// what `what_changed` takes
const WHAT_CHANGED_ARGS: &[Argument] = &[
    needed("file", "the file the line is in, as a path"),
    needed("line", "the line to compare two visits to"),
    optional(
        "expressions",
        "python expressions to evaluate at both stops, separated by commas. \
         omit to compare the scopes alone",
        "the scopes alone",
    ),
];

/// what `why_wont_it_stop` takes
const WHY_WONT_IT_STOP_ARGS: &[Argument] = &[optional(
    "deadline_ms",
    "how long to give the pause before giving up on it",
    "5000",
)];

/// every investigation this server offers
pub fn prompts() -> Vec<Prompt> {
    vec![
        Prompt {
            name: "nth_call",
            title: "stop on the nth call that matches, not the first",
            description: "stop the program at a line the nth time a condition holds \
                 there. done by hand this becomes a counter written into the \
                 program, or n resumes counted by the agent — both of which \
                 change or waste what is being measured. bpd carries the count \
                 as a typed hit condition the debuggee evaluates itself",
            arguments: NTH_CALL_ARGS,
            body: NTH_CALL,
            mentions: &["continue_", "launch", "set_breakpoints", "state"],
        },
        Prompt {
            name: "step_until",
            title: "step until something becomes true, in one call",
            description: "run to a line and then step until a condition stops holding, \
                 returning where it stopped holding. done by hand this is a step \
                 and an evaluate per line, which is two tool calls per line of \
                 the program and an agent's whole context spent on protocol. it \
                 is one submitted script",
            arguments: STEP_UNTIL_ARGS,
            body: STEP_UNTIL,
            mentions: &["run_script", "stack", "state"],
        },
        Prompt {
            name: "what_changed",
            title: "what changed between two times through the same line",
            description: "compare the state at one stop against the state at a later one \
                 and get back the difference. done by hand this is both states \
                 shipped to the agent and compared in its head, which spends the \
                 context twice and calls a truncated value unchanged",
            arguments: WHAT_CHANGED_ARGS,
            body: WHAT_CHANGED,
            mentions: &["continue_", "diff", "set_breakpoints", "state"],
        },
        Prompt {
            name: "why_wont_it_stop",
            title: "find out where a program that will not stop is",
            description: "work out where a program that hit its deadline actually is. \
                 the trap is treating a timeout as a location, or resuming again \
                 with a larger deadline — a timeout carries no location at all, \
                 and the program has to be held before anything can be asked \
                 about it",
            arguments: WHY_WONT_IT_STOP_ARGS,
            body: WHY_WONT_IT_STOP,
            mentions: &["continue_", "pause", "stack", "state", "threads", "wait"],
        },
    ]
}

/// stop on the nth call that matches
const NTH_CALL: &str = r#"stop at `{file}` line {line} on hit number {count} where the condition is: {condition}

do it like this, and not by counting resumes:

1. if no program is running, `launch` it first — a breakpoint binds on a python
   thread bpd is holding, and `launch` holds the program before its first
   statement, which is where a breakpoint binds against a real interpreter
2. call `set_breakpoints` with the whole set, this breakpoint included:

   ```json
   {"breakpoints": [{
     "file": "{file}",
     "line": {line},
     "condition": "<the condition above, as python, or leave this field out>",
     "hits": {"hits": "exactly", "count": {count}}
   }]}
   ```

   `set_breakpoints` replaces **every** breakpoint, so anything else that should
   stay set has to be in the same array. the answer says the line each one really
   bound to and every code object it was armed in; a breakpoint that did not bind
   says why, and is never reported as set

3. call `continue_` with a `deadline_ms` you are willing to wait. the answer
   **is** the stop — there is no event to wait for
4. call `state` at that stop for the frames, the scopes and any expression you
   want evaluated there

why this shape rather than stepping or counting: the count is a `hits` object
that the debuggee evaluates itself, so stopping on hit {count} costs one round
trip rather than that many, and the program runs at full speed in between. a hit
only counts when the condition was **true** — one whose condition raised does not
count, and stops there carrying the exception, because a condition that raised
has not said `false`

`hits` has three kinds and they mean different things: `exactly` is the nth hit
and nothing after it, `at_least` is the nth and every one after, and `every` is
every nth hit. it is typed rather than a string because `>5`, `=5`, `%5` and a
bare `5` mean different things in different debuggers
"#;

/// step until something becomes true, in one call
const STEP_UNTIL: &str = r#"run to `{file}` line {line} and then step while this stays true: {predicate}

submit the whole thing as one `run_script` call rather than stepping and
evaluating in turn:

```json
{
  "steps": [
    {"step": "run_to", "file": "{file}", "line": {line}},
    {"step": "log", "note": "reached the line, stepping while: {predicate}"},
    {"step": "while",
     "predicate": {"expression": "{predicate}"},
     "limit": {limit},
     "body": [{"step": "step_over"}]},
    {"step": "stack"}
  ],
  "budget": {"steps": <a little over {limit}>, "wall_ms": <how long you will wait>, "bytes": <how much transcript you will read>}
}
```

what the answer is: **the transcript**, not the final state. every record says
which step of the tree it came from and where the held thread was when it ran, so
the line the predicate went false on is in the record, and you can see how it got
there rather than guessing

four things that decide whether this works:

- the predicate has to produce a **`bool`**. anything else halts the script
  naming the type it produced, because truth-testing an object means running the
  program's own `__bool__` or `__len__`. write the comparison down —
  `x is not None`, `len(items) > 0`, `total == 0`
- a script cannot capture a value it was not told. to step until something
  *changes*, read it first — `state` with that expression at the stop — and then
  submit the script with the value it had written into the predicate as a literal
- the `budget` is required on all three axes and the **byte** budget usually
  bites first. exhausting one returns the transcript so far with `partial: true`
  and the bound that bit
- reaching the `limit` with the predicate still true **halts** the script rather
  than carrying on: the loop did not finish what it was for, and a `stack` after
  it would be reporting a place the investigation did not reach

the `run_to` arms a breakpoint of its own and takes it back off, which is why it
is a step of a script and not a tool of its own — the record says the id it was
armed under and what became of it. a script drives **one thread**: the one the
stop it starts from holds
"#;

/// what changed between two times through the same line
const WHAT_CHANGED: &str = r#"compare two visits to `{file}` line {line}, evaluating: {expressions}

1. `set_breakpoints` with this line in the set, then `continue_` to reach it
2. call `state` there, asking for what you want compared:

   ```json
   {"frames": 1, "scopes": ["local"], "expressions": [{"expression": "<one per expression above>"}]}
   ```

   the answer carries a `snapshot` id. keep it
3. `continue_` again to the same line, and call `state` again with **exactly the
   same arguments**. keep the second `snapshot` id
4. call `diff` with `{"before": "<the first id>", "after": "<the second id>"}`

the difference is the answer; the two states are raw material. asking for both
and comparing them yourself spends the context twice and gets the hard part
wrong, which is what "unchanged" is allowed to mean

read the answer knowing three things:

- a value that a bound cut short in **either** snapshot is in `not_compared`,
  never in `unchanged`. half a list is not evidence that a list did not change,
  so if something you care about lands there, ask for both states again with a
  larger `detail.budget`
- a depth of the stack running different code in the two is not compared either.
  depth is a position, not an identity
- something only one of the two read is unknown rather than absent, which is why
  step 3 says to use the same arguments

a snapshot id does **not** go stale. it names a reading that was already taken
rather than a promise to take one, so it stays valid for the whole session across
any number of resumes — two stops any distance apart can be compared. what ends
with a stop is asking that stop anything more
"#;

/// find out where a program that will not stop is
const WHY_WONT_IT_STOP: &str = r#"the program is not stopping. find out where it is

a `timed_out` answer is **not a location**. it carries no thread, no frames and
no reason, and that is the honest shape of what bpd can see: everything the agent
inside the debuggee answers, it answers on a thread it is holding, and a program
with nothing held cannot be asked what its threads are doing. do not resume again
with a larger deadline hoping for more — that is the same answer, later

do this instead:

1. call `pause` with `{"deadline_ms": {deadline_ms}}`. it arms a line event for
   the whole program and holds whichever thread reaches one first, which is a
   **real stop** and makes everything askable again. `wait` is the alternative
   when you would rather not touch the program at all, but it only helps if the
   program was going to stop on its own
2. if `pause` also times out, read its `running` and its `note`. `running` counts
   only threads bpd is not already holding, so an empty one means either every
   other thread is parked in a C call — where there is no monitoring event to
   hold one at — or the threads that would reach a line are ones bpd already
   holds. the `note` says which, and what to do about it
3. once something is held, call `threads`. it is the only question that is about
   threads bpd is **not** holding. it takes two samples `settle_ms` apart and
   marks a thread `still` when it was in the same place both times
4. call `stack` or `state` on the held stop for where that one thread actually is

what `threads` will not tell you, and what to do about it: cpython exposes no
owner for a lock, so bpd cannot say that a thread is waiting for one another
thread holds. a thread blocked in `sock.recv` and a thread piled up behind a lock
look identical from here. `still` is where to look, not what is wrong — take the
line it names and read the code there

one thing worth checking early: if `continue_` answered `finishing` rather than
timing out, the program is not stuck at all. it ran to its end with threads still
held, and it cannot exit because the interpreter finalizes by joining its
non-daemon threads and a held thread cannot be joined. the answer names them, and
resuming them lets it finish
"#;
