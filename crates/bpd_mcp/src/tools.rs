//! the tools, and the schemas that are the documentation for them
//!
//! MCP has three primitives and only **tools** are model-controlled: whether a
//! resource is ever read is the host application's choice, and whether a prompt
//! is invoked is the user's. so the semantics live here and in the errors, and
//! nowhere that an agent may never load
//!
//! every schema sets `additionalProperties: false`, and the structs the server
//! parses into set `deny_unknown_fields` to match. a misspelled argument is
//! refused by name rather than silently taking its default — which for
//! `deadline_ms` would be a call that hangs and for `depth` would be a value
//! quietly read shallower than asked for

/// one tool, as `tools/list` reports it
#[derive(Debug, Clone)]
pub struct Tool {
    /// the name a `tools/call` uses
    pub name: &'static str,
    /// a short human label
    pub title: &'static str,
    /// what it does, and what its answer claims
    pub description: String,
    /// the JSON Schema for its arguments
    pub schema: serde_json::Value,
}

impl Tool {
    /// this tool as the object `tools/list` carries
    pub fn listing(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": self.schema,
        })
    }
}

/// what every control tool says about its deadline
const DEADLINE: &str = "how long to wait for the program to stop, in \
    milliseconds. when it passes the answer is `outcome: \"timed_out\"`: the \
    program is **still running**, no thread is held, and bpd reports nothing \
    about where it is — everything the agent inside the debuggee answers, it \
    answers on a thread it is holding, so a running program cannot even be \
    asked what its threads are doing. `wait` keeps waiting; `pause` holds the \
    next thread that reaches a line";

/// what every control tool says about the frames it returns
const FRAMES: &str = "how many frames of the resulting stop to return, counting \
    from the one that stopped. 0 for none, and 5 when it is left out. the answer \
    says how deep the stack really is either way";

/// what a tool that is about one held thread says about naming it
const STOP: &str = "which stop this is about. a stop holds one thread and the \
    rest of the program keeps running, so several can be held at once. omit it \
    when exactly one is — when several are, the call is refused and the refusal \
    lists them";

/// what a tool that is about one session says about naming it
const SESSION: &str = "which session this is about. a debuggee holds one \
    ordinarily, and a **debugged fork** is a second — `sessions` lists them. \
    omit it when there is one; when there are several, a call that names none \
    is refused and the refusal lists them. a call that is about a stop needs \
    none either way: the stop carries the session it was reported from, and a \
    `session` that disagrees with it is refused rather than believed";

/// what a tool that is about one frame says about naming it
const FRAME: &str = "how far down that stop's stack, with 0 the frame the \
    program is in now. a frame belongs to the stop it was reported at: once \
    that thread has run on, asking about it is refused rather than answered \
    about whatever is at that depth now";

fn object(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "object",
        "required": required,
        "additionalProperties": false,
    });
    schema["properties"] = properties;
    schema
}

fn integer(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "integer", "minimum": 0, "description": description })
}

/// the bounds on how much of a value a read may carry
///
/// every field is a field of `bpd_core::Detail`, and every one of them that
/// bites is named in the answer with what it cut and which of these to raise
pub(crate) fn detail() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "how much of a value to read. every bound that bites is \
                        named in the answer, with how much there was and which \
                        of these to raise",
        "properties": {
            "depth": integer("how many levels of container or object to open. 0 \
                              reports a value's type and size and opens nothing"),
            "children": integer("how many children of one container to read"),
            "text": integer("how many characters of one string, or bytes of one \
                             `bytes`, to read"),
            "budget": integer("the byte budget for the whole answer"),
            "attributes": { "type": "boolean", "description":
                "read an object's instance dictionary. on by default, because it \
                 is storage rather than behaviour — it never reaches \
                 `__getattr__` or a property. a type is free to make `__dict__` \
                 its own code, which is why it can be turned off" },
            "repr": { "type": "boolean", "description":
                "call `__repr__` on a value with no structural representation. \
                 **off** by default: `__repr__` is arbitrary user code, bpd \
                 cannot interrupt it once it has started, and one that hangs \
                 hangs the debuggee" },
        },
        "additionalProperties": false,
    })
}

/// the arguments a tool about one frame's value shares
fn frame_properties() -> serde_json::Value {
    serde_json::json!({
        "session": integer(SESSION),
        "stop": integer(STOP),
        "frame": integer(FRAME),
        "detail": detail(),
    })
}

/// the steps of a debug script, as a schema that refers to itself
///
/// `if` and `while` carry blocks of steps, so the definition is recursive and
/// goes across as a `$ref` — which is the whole reason the steps are **data**.
/// an MCP tool takes JSON Schema input already, so a tree of them needs no
/// parser, no grammar and no syntax errors, and this is the documentation an
/// agent reads before writing one
#[expect(
    clippy::too_many_lines,
    reason = "it is the step vocabulary, and the descriptions in it are what an \
              agent writes a script from. splitting them into helpers would put \
              a step's name and what it promises in two places"
)]
fn step_definition() -> serde_json::Value {
    let step = |name: &str, what: &str, mut properties: serde_json::Value, required: &[&str]| {
        properties["step"] = serde_json::json!({ "const": name });
        let mut wanted = vec!["step"];
        wanted.extend_from_slice(required);
        let mut schema = object(properties, &wanted);
        schema["description"] = what.into();
        schema
    };
    let block = |what: &str| {
        serde_json::json!({
            "type": "array",
            "description": what,
            "items": { "$ref": "#/$defs/step" },
        })
    };
    let predicate = serde_json::json!({
        "type": "object",
        "description": "a python expression, evaluated in a frame of the stop \
                        the script is at. it has to produce a **`bool`**: \
                        anything else halts the script naming the type it \
                        produced, because truth-testing an object means running \
                        the program's own `__bool__` or `__len__` and branching \
                        on the result. write the comparison down — \
                        `x is not None`, `len(items) > 0`",
        "properties": {
            "expression": { "type": "string", "description": "the expression, as python" },
            "frame": integer(FRAME),
        },
        "required": ["expression"],
        "additionalProperties": false,
    });

    serde_json::json!({ "oneOf": [
        step("step_over", "step the script's thread to the next line of its frame", serde_json::json!({}), &[]),
        step("step_in", "step the script's thread into the next frame it enters", serde_json::json!({}), &[]),
        step("step_out", "run the script's thread's frame to its end", serde_json::json!({}), &[]),
        step(
            "continue",
            "let the script's thread go until it stops again. only that thread: \
             a script drives the one thread its starting stop holds",
            serde_json::json!({}),
            &[],
        ),
        step(
            "run_to",
            "run until the script's thread reaches a source location. the engine \
             arms a breakpoint of its own, runs, and **takes it back off** — \
             which is why this is a step of a script and not a tool. the record \
             says the id it was armed under and what became of it. a location \
             that does not bind halts the script rather than running to nothing",
            serde_json::json!({
                "file": { "type": "string", "description": "the file, as a path" },
                "line": integer("the line to run to"),
                "condition": { "type": "string", "description":
                    "a python expression that has to be true for a hit to count \
                     — the breakpoint condition machinery, unchanged" },
                "hits": { "type": "object", "description":
                    "which of the qualifying hits to stop on, which is how *the \
                     third call with a negative amount* is written",
                    "properties": {
                        "hits": { "type": "string", "enum": ["exactly", "at_least", "every"] },
                        "count": { "type": "integer", "minimum": 1 },
                    },
                    "required": ["hits", "count"],
                    "additionalProperties": false },
            }),
            &["file", "line"],
        ),
        step(
            "eval",
            "evaluate a python expression and record what it produced. this runs \
             the program's own code, by request. one that **raises halts the \
             script**: carrying on past it would record an investigation that \
             did not happen",
            serde_json::json!({
                "expression": { "type": "string", "description": "the expression, as python" },
                "frame": integer(FRAME),
                "detail": detail(),
            }),
            &["expression"],
        ),
        step(
            "stack",
            "record the script's thread's frame chain",
            serde_json::json!({ "top": integer(
                "how many frames, from the one that stopped. omit for all of them"
            ) }),
            &[],
        ),
        step(
            "log",
            "record a note of the script's own. nothing reaches the debuggee — \
             it is text the script wrote, so that a transcript of fifty records \
             says what the script thought it was doing. recording a *value* is \
             `eval`, which costs the program an evaluation",
            serde_json::json!({ "note": { "type": "string" } }),
            &["note"],
        ),
        step(
            "if",
            "run one of two blocks, according to a python predicate",
            serde_json::json!({
                "predicate": predicate,
                "then": block("what runs when it is true"),
                "otherwise": block("what runs when it is false"),
            }),
            &["predicate"],
        ),
        step(
            "while",
            "run a block while a python predicate is true, at most `limit` \
             times. reaching the limit with the predicate still true **stops \
             the script**: the loop did not finish what it was for, and the \
             steps after it would run somewhere they did not expect",
            serde_json::json!({
                "predicate": predicate,
                "limit": { "type": "integer", "minimum": 1, "description":
                    "the most passes of the body there may be. it is required \
                     and cannot be zero — a loop without a bound is a hung \
                     session, so a script that cannot be shown to terminate is \
                     refused at submission rather than discovered at runtime" },
                "body": block("what runs on each pass"),
            }),
            &["predicate", "limit"],
        ),
        step(
            "finish",
            "end the script here, with a reason",
            serde_json::json!({ "because": { "type": "string" } }),
            &["because"],
        ),
    ] })
}

/// what a script may spend
fn budget() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "what the whole script may spend. there is no default and \
                        no way to leave it out: a script without one is a \
                        session that can hang. the **byte** budget is usually \
                        the first to bite — a value read inside a loop spends it \
                        long before fifty steps have run",
        "properties": {
            "steps": { "type": "integer", "minimum": 1, "description":
                "how many steps may run — one per record, including the test of \
                 an `if` and each test of a `while`" },
            "wall_ms": { "type": "integer", "minimum": 1, "description":
                "how long the whole script may take. it is also the deadline \
                 every control step waits under, so a script waiting for a \
                 program that never stops spends exactly this" },
            "bytes": { "type": "integer", "minimum": 1, "description":
                "how many bytes of transcript may be recorded. checked after \
                 each record, so one record can carry the total past it — the \
                 transcript says how many were really made either way" },
        },
        "required": ["steps", "wall_ms", "bytes"],
        "additionalProperties": false,
    })
}

/// one step tool, which differ only in which way they go
fn step(name: &'static str, title: &'static str, what: &str) -> Tool {
    Tool {
        name,
        title,
        description: format!(
            "{what}\n\nthis is one thread's step: every other thread in the \
             program goes on running while it happens. it lets the thread go, \
             waits for where it lands, and **returns that stop** — there is no \
             event to correlate and nothing to poll."
        ),
        schema: object(
            serde_json::json!({
                "session": integer(SESSION),
                "stop": integer(STOP),
                "deadline_ms": integer(DEADLINE),
                "frames": integer(FRAMES),
            }),
            &["deadline_ms"],
        ),
    }
}

/// every tool this server offers, in the order an agent meets them
#[expect(
    clippy::too_many_lines,
    reason = "it is the tool table, and the descriptions in it are the surface \
              an agent learns bpd from. splitting them into helpers would put \
              a tool's name and what it promises in two places"
)]
pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "launch",
            title: "launch a program under the debugger",
            description: "start a python program with bpd's agent attached and \
                hold it **before its first statement**, which is where a \
                breakpoint binds against a real interpreter rather than against \
                a guess about one. returns the entry stop. set breakpoints \
                after this and before `continue_`."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "program": { "type": "string", "description":
                        "path to the script to run" },
                    "python": { "type": "string", "description":
                        "the interpreter, resolved on PATH like any command. \
                         `python3` when it is left out, which on a great many \
                         machines is older than bpd's minimum — name the one \
                         the program is meant to run under. cpython 3.13 or \
                         newer; anything else is refused by name rather than \
                         limped along" },
                    "args": { "type": "array", "items": { "type": "string" },
                        "description": "arguments for the program, exactly as it \
                                        receives them" },
                    "frames": integer(FRAMES),
                }),
                &["program"],
            ),
        },
        Tool {
            name: "set_breakpoints",
            title: "replace the whole breakpoint set",
            description: "replace **every** breakpoint with this set — it is not \
                a delta, because a debugger that accumulates edits has two ideas \
                of what is set. ids are the position in this list, counting from \
                one, and every later report names a breakpoint by that id.\n\n\
                the answer says what is really behind each one: the line it \
                bound to (which may not be the line asked for — a blank line, a \
                comment or an elided `pass` moves to the next executable line), \
                every code object it was armed in, and for one that did not bind, \
                why. a breakpoint is never reported as set unless there is a code \
                object and an offset behind it.\n\n\
                only while a thread is held. the agent binds a breakpoint on a \
                python thread it is holding, so a running program has to be \
                paused first."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "breakpoints": {
                        "type": "array",
                        "description": "the whole set. an empty array clears them all",
                        "items": object(
                            serde_json::json!({
                                "file": { "type": "string", "description":
                                    "the file, as a path. bpd identifies it by \
                                     what the filesystem says it is, so a \
                                     symlink and an editable install both work \
                                     and a module that is not on disk — a zip, \
                                     a frozen module, a string passed to `exec` \
                                     — is refused with that as the reason" },
                                "line": integer("the line to stop on"),
                                "condition": { "type": "string", "description":
                                    "a python expression that has to be true \
                                     before anything happens, compiled once when \
                                     the breakpoint is set and evaluated in the \
                                     frame that reached the line. one that does \
                                     not compile leaves the breakpoint unbound \
                                     rather than failing later. one that raises \
                                     **stops**, carrying the exception — it has \
                                     not said `false`" },
                                "hits": {
                                    "type": "object",
                                    "description": "which of the qualifying hits \
                                        to act on. a hit qualifies when the \
                                        condition was true, or when there is no \
                                        condition; one whose condition raised \
                                        does not count. this is typed rather \
                                        than a string, because `>5`, `=5`, `%5` \
                                        and a bare `5` mean different things in \
                                        different debuggers and a debugger that \
                                        guessed would stop on the wrong pass",
                                    "properties": {
                                        "hits": { "type": "string",
                                            "enum": ["exactly", "at_least", "every"],
                                            "description":
                                                "`exactly` the nth hit and \
                                                 nothing after it, `at_least` the \
                                                 nth and every one after, `every` \
                                                 nth hit" },
                                        "count": { "type": "integer", "minimum": 1 },
                                    },
                                    "required": ["hits", "count"],
                                    "additionalProperties": false,
                                },
                                "log": { "type": "string", "description":
                                    "produce a log record instead of stopping. \
                                     `{...}` is a python expression evaluated in \
                                     the frame and converted with `str()`; `{{` \
                                     and `}}` are a literal brace. the records \
                                     come back on the next control tool's answer" },
                                "after": integer(
                                    "arm this breakpoint only once another one \
                                     has been hit — the **position** of that one \
                                     in this same list, counting from 1. until \
                                     then it is bound and not armed: its line \
                                     carries no events at all, so waiting costs \
                                     nothing where a condition costs an \
                                     expression on every pass. the answer says \
                                     `armed: false` and `waiting_for` while it \
                                     waits. what arms it is the earlier \
                                     breakpoint **acting** — stopping or logging \
                                     — rather than merely being passed, and it \
                                     is permanent and per process",
                                ),
                            }),
                            &["file", "line"],
                        ),
                    },
                }),
                &["breakpoints"],
            ),
        },
        Tool {
            name: "set_exception_breakpoints",
            title: "stop where an exception is raised or leaves the program",
            description: "the whole setting rather than a delta. `raised` stops \
                where an exception is raised whether or not anything catches it, \
                reported **once**, in the frame that raised it. `uncaught` stops \
                as an exception leaves the program's outermost frame, which is \
                the first moment it is knowable that nothing caught it.\n\n\
                both are paid for process-wide while they are on: the events \
                behind them cannot be armed per code object."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "raised": { "type": "boolean", "description":
                        "stop where an exception is raised, caught or not" },
                    "uncaught": { "type": "boolean", "description":
                        "stop where an exception leaves the outermost frame" },
                }),
                &[],
            ),
        },
        Tool {
            name: "debug_children",
            title: "debug the program's children",
            description: "**off by default.** with this on, a child this \
                program starts opens a debug session of its own and arrives \
                **held** — a second session, with its own stops and its own \
                numbering, which `sessions` lists and the `session` argument \
                reaches. with it off a child runs exactly as it would have \
                without a debugger, and is reported rather than debugged.\n\n\
                it covers both ways a child comes into being, and they are held \
                differently because they are different things. a **forked** \
                child is a copy of this process and is held at the line that \
                forked. a child that was **exec'd** — `subprocess`, \
                `multiprocessing` with `spawn` or `forkserver`, django's \
                `runserver` reloader — is a fresh interpreter, so it is held at \
                its own startup, before its program has been compiled: there is \
                no line on that stop and no stack under it, because nothing of \
                the program has run. set breakpoints there and resume.\n\n\
                it must be set **before** the child is made. a fork reads it \
                inside `os.fork()` with nothing left to ask, and an exec reads \
                the environment this writes, which `subprocess` copies before \
                bpd is told anything.\n\n\
                **this is the one setting a program can notice.** an exec'd \
                child is reached through `PYTHONPATH`, appended — so with this \
                on the program's environment gains `PYTHONPATH` and three \
                `BPD_CHILD_*` names, and its `sys.path` gains one last entry. \
                nothing else, and turning it off puts all of it back. a child \
                that is not python inherits the variables and ignores them; a \
                python **grandchild** inherits them and attaches.\n\n\
                a fork on a platform that has none is refused rather than \
                accepted and never acted on. the answer is what the agent says \
                is set, read back rather than echoed."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "on": { "type": "boolean", "description":
                        "whether a child of this session's program opens a \
                         session of its own" },
                }),
                &["on"],
            ),
        },
        Tool {
            name: "sessions",
            title: "every session this debuggee holds",
            description: "list the debugged processes. one is ordinary; a second \
                appears when the program made a **child** and `debug_children` \
                was on — this server writes nothing that is not an answer, so \
                this is how a session that arrived while the program was running \
                is found.\n\n\
                each says whether bpd started that process. one bpd did not \
                start — a debugged child — cannot be terminated and has no exit \
                code to read: bpd is not its parent, so what it exited with is \
                not bpd's to give, and both are refused by name rather than \
                invented.\n\n\
                every other tool takes the `session` of one of these. a call \
                that is about a stop needs none."
                .to_string(),
            schema: object(serde_json::json!({}), &[]),
        },
        Tool {
            name: "continue_",
            title: "let every held thread go, and return the next stop",
            description: "resume everything bpd is holding and wait for what the \
                program does next — **the stop is the return value**. the answer \
                is one of: `stopped` (with the stop and its top frames), \
                `exited` (with the exit code), `ended` (the program is over and \
                bpd cannot say what it exited with, because bpd did not start \
                that process), `finishing` (the program ran to its end with \
                threads still held, so it cannot exit until they are resumed), \
                or `timed_out`."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "deadline_ms": integer(DEADLINE),
                    "frames": integer(FRAMES),
                }),
                &["deadline_ms"],
            ),
        },
        step(
            "step_over",
            "step over a line",
            "run to the next line of this frame, whatever it calls on the way. a \
             frame that **suspends** is not left: a `yield` or an `await` hands \
             control away and comes back, so this lands on the next line of the \
             same frame rather than in the generator's consumer or in the event \
             loop.",
        ),
        step(
            "step_in",
            "step into the next frame entered",
            "stop at the first line of the next frame this thread enters — a \
             function called, a generator or coroutine resumed, or one thrown \
             into. a line that enters nothing behaves as `step_over`.",
        ),
        step(
            "step_out",
            "run this frame to its end",
            "run until this frame is finished and stop at the next line of its \
             caller. finished, not suspended: a generator that yields is resumed \
             later and is still the frame being stepped, so this runs it to its \
             end.",
        ),
        Tool {
            name: "wait",
            title: "wait for the program without touching it",
            description: "wait for the next thing the program does, resuming \
                nothing and arming nothing. this is what to call after a \
                `timed_out` answer to keep waiting, and it is the only tool that \
                does not perturb the program at all."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "deadline_ms": integer(DEADLINE),
                    "frames": integer(FRAMES),
                }),
                &["deadline_ms"],
            ),
        },
        Tool {
            name: "pause",
            title: "hold the next thread that reaches a line",
            description: "the one thing that can be asked of a program with \
                nothing held. there is nothing in cpython that suspends a \
                thread, so this arms a line event for the whole program and \
                holds whichever thread arrives first — which thread that is \
                belongs to the operating system, and the answer says which \
                threads were running python when it went on.\n\n\
                `running` counts only threads bpd is **not** already holding, \
                because a held thread reaches no line until it is resumed. so an \
                empty `running` means nothing will arrive until either a held \
                thread is let go or a thread parked in a C call — where there is \
                no monitoring event to hold one at — comes back into python, and \
                the answer's `note` says which of those two it is."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "deadline_ms": integer(DEADLINE),
                    "frames": integer(FRAMES),
                }),
                &["deadline_ms"],
            ),
        },
        Tool {
            name: "resume",
            title: "let held threads go without waiting",
            description: "let threads go and return as soon as they have been \
                let go, without waiting for what they do next. what to use when \
                several threads are held and only some should run on; naming a \
                thread that is not held refuses the whole request rather than \
                half performing it."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "threads": { "type": "array", "items": integer("a thread identity"),
                        "description": "the interpreter's thread identities to \
                                        let go, as reported on a stop. omit for \
                                        every thread that is held" },
                }),
                &[],
            ),
        },
        Tool {
            name: "stack",
            title: "one held thread's stack",
            description: "walk the frame chain of a held thread. the held \
                thread's stack is a **snapshot** in either thread mode — it is \
                inside a monitoring callback and cannot return — while \
                everything the frames point at is a sample. no stack is \
                available for a thread bpd is not holding: its frames are moving."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "top": integer("how many frames to report, from the one that \
                                    stopped. omit for all of them"),
                }),
                &[],
            ),
        },
        Tool {
            name: "variables",
            title: "read one scope of one frame",
            description: "python resolves a name by **which scope it is in**, \
                decided at compile time, so the four are read separately and \
                never merged: `local` is the frame's own locals, `cell` are the \
                locals a nested function captures, `free` are the variables this \
                frame captures from an enclosing one, and `global` is the module \
                namespace. a captured argument is in `local` **and** `cell`, \
                because cpython says it is both.\n\n\
                the answer separates three things a merged mapping would lose: \
                what the scope holds, names the scope has that hold nothing at \
                this line (`unbound`), and names whose value the frame does not \
                expose (`unreadable`). `left_out` names every bound that bit."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frame": integer(FRAME),
                    "scope": { "type": "string", "enum": ["local", "cell", "free", "global"],
                        "description": "which scope. `global` is expensive: a \
                                        module namespace begins with \
                                        `__builtins__`" },
                    "detail": detail(),
                }),
                &["scope"],
            ),
        },
        Tool {
            name: "facts",
            title: "what is provable about a frame's names, and for how long",
            description: "`variables` says what a scope holds **right now**. this says what is true of a name *and* how far past this line that can be carried — which is the half you need to reason about code that has not run yet, and the half you cannot work out from a value.

                every fact carries a `stability`. `permanent` means nothing the program can do makes it false, short of rebinding the name —                 which is in the source you are reading, not in the object. `until` names what would have to happen: the object's contents changing, its attributes being assigned, or its `__class__`                 being reassigned. the judgement comes from cpython itself —                 whether the type is a heap type, whether instances keep a dictionary, and whether the type is one whose storage is its value — so `x == 3` on an `int` is permanent and the same reading of an `int` subclass is not.

                **it runs none of the program.** a reading that would need `__bool__`, `__len__`, a property or a `__getattr__` is not taken and not guessed at: that name comes back in `silent`                 naming what would have run. use `evaluate` when you want the program's own answer and are willing to pay for it.

                a name may be a dotted path — `self.limit` — and every segment is read out of an object's own storage. every name asked about comes back in exactly one of `proved` and `silent`."
                .to_string(),
            schema: {
                // not `frame_properties`: those carry a `detail`, which bounds
                // how much of a value **graph** is read. a fact carries one
                // value and is bounded by `limit` instead, and a schema
                // advertising an argument the tool does not take is an
                // instruction an agent would follow and bpd would refuse
                let mut properties = serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frame": integer(FRAME),
                });
                properties["names"] = serde_json::json!({
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "the names to prove things about, each a name or a dotted path. \
                                    named rather than the whole scope because you are the one \
                                    reading the source that mentions them",
                });
                properties["limit"] = serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "integer",
                            "description": "how many characters of a value one fact may carry. a \
                                            longer value produces no fact rather than a cut one, \
                                            because a cut value is a different claim" },
                        "depth": { "type": "integer",
                            "description": "how many segments of a dotted path to follow" },
                    },
                    "additionalProperties": false,
                    "description": "how much one fact may cost. omit for the defaults",
                });
                object(properties, &["names"])
            },
        },
        Tool {
            name: "template_context",
            title: "read a django template frame's context, layer by layer",
            description: "the template equivalent of `variables`, and the only \
                thing that reads a django template frame — one is \
                **synthesised**, the interpreter has no frame for it, and it \
                has no python scopes at all. `stack` says which frames are \
                which: a frame whose `kind` is `template` is one of these, and \
                its `python_frame` is the frame underneath where python is \
                read.\n\n\
                `django.template.Context` is a **stack of dicts** and is \
                reported as one, never merged: django resolves a name by \
                walking the layers from the last backwards, so a name in two \
                layers is a shadowing that decides what the template renders — \
                and that is usually what is being debugged. layer 0 is django's \
                builtins, layer 1 is the dictionary the render was given, and \
                every `{% block %}`, `{% with %}`, `{% for %}` or \
                `{% include ... with %}` that is open adds one."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frame": integer(FRAME),
                    "detail": detail(),
                }),
                &[],
            ),
        },
        Tool {
            name: "evaluate",
            title: "evaluate an expression in a frame",
            description: "compiled at the request and evaluated against the \
                frame's own globals and locals, which is what `LOAD_NAME` sees. \
                **this runs the program's own code**, by request.\n\n\
                an expression that raises, and one that does not compile, are \
                both answers carrying the exception rather than failures: the \
                interpreter is the authority on what an expression is. this \
                thread's breakpoints are suppressed while it runs.\n\n\
                **the frame decides the language.** against a frame whose `kind` \
                is `template` the text is django template syntax, not python, \
                and django resolves it: `a.b` is a dictionary key before it is \
                an attribute, a name holding a callable is **called**, and \
                `x|upper` is a filter. that is what the same text means where \
                the user is looking. for python in a template frame, name the \
                `python_frame` the stack reports beside it."
                .to_string(),
            schema: {
                let mut properties = frame_properties();
                properties["expression"] = serde_json::json!({
                    "type": "string",
                    "description": "the expression. python in a python frame, \
                                    and django template syntax in a template one",
                });
                object(properties, &["expression"])
            },
        },
        Tool {
            name: "set_variable",
            title: "write a name of a frame's scope",
            description: "write a name the scope **already has**, and read back \
                what the frame holds afterwards. a name the code object does not \
                have is refused: `f_locals` accepts such a write and keeps it, \
                the compiled function goes on reading its fast locals, and the \
                debugger would be reporting a change the program never received.\n\n\
                writing something *inside* a value is not offered at all — that \
                means running the program's own `__setattr__` or `__setitem__`, \
                which is the program rather than the debugger."
                .to_string(),
            schema: {
                let mut properties = frame_properties();
                properties["scope"] = serde_json::json!({
                    "type": "string",
                    "enum": ["local", "cell", "free", "global"],
                    "description": "which scope the name is in",
                });
                properties["name"] = serde_json::json!({
                    "type": "string",
                    "description": "the name to write",
                });
                properties["value"] = serde_json::json!({
                    "type": "string",
                    "description": "a python expression, evaluated in that frame, \
                                    for the new value",
                });
                object(properties, &["scope", "name", "value"])
            },
        },
        Tool {
            name: "set_next_statement",
            title: "move the held frame to another line",
            description: "move where the program will carry on from, **without \
                running anything**: the thread stays held, at the line it moved \
                to. the lines between where it was and where it is now are not \
                executed.\n\n\
                only in the frame the thread is executing — `frame: 0`, and the \
                frame under a template frame is not it either. a frame further \
                down is suspended in a call, and cpython *accepts* a move in one \
                rather than refusing it: the frame then runs on with a value \
                stack that no longer matches where it is, and returns something \
                it never computed. bpd refuses instead.\n\n\
                what the answer tells you that nothing else would:\n\n\
                - `at` is read off the frame, because **no line event is \
                  delivered for the line a jump moves to**. a debugger that \
                  waited to be told would report the line after it\n\
                - `unannounced` names breakpoints on the destination line that \
                  will **not** fire for this pass, for the same reason. they are \
                  still set, and fire the next time the line runs\n\
                - `bound_to_none` names locals that held nothing and hold `None` \
                  now. cpython binds every unbound local of the frame as part of \
                  a jump — it is a change to the program the debugger caused\n\n\
                it does **not** run the cleanup of a block it leaves: jumping out \
                of a `with` does not call `__exit__` and jumping out of a `try` \
                does not run its `finally`. cpython does not, and bpd does not \
                pretend to.\n\n\
                a line cpython will not move to is refused with cpython's own \
                reason — `can't jump into the body of a for loop`, `can only jump \
                from a 'line' trace event` — and the frame does not move."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frame": integer(FRAME),
                    "line": integer("the line of that frame's file to move to. it \
                                     has to be a line of the code object the \
                                     frame is running, and cpython decides \
                                     whether it can be reached from where the \
                                     frame is"),
                }),
                &["line"],
            ),
        },
        Tool {
            name: "restart_frame",
            title: "run the held frame again, with the locals a call binds",
            description: "**two ways, and `again` picks.** they are different \
                operations rather than two implementations of one. the default \
                (`either`) prefers the first and falls back to the second.\n\n\
                `in_place` — **run the frame again where it stands.** the frame \
                is moved back to the top of its own body and its locals are put \
                back to what a call that had just started would hold. its \
                **caller is never touched**: it stays suspended in the `CALL`, \
                nothing on its line runs a second time, and when the frame does \
                return its value goes where the program was always going to put \
                it. so `x = f(f2())` restarts `f` without `f2` running twice, and \
                `x = f(obj.attr)` restarts `f` without the property's getter \
                running twice — not because either is analysed and found safe, \
                but because neither is re-executed at all. the same holds for a \
                frame called from C and one reached by an attribute lookup.\n\n\
                **the thread is not resumed and no second stop follows.** this \
                stop is the restart; it stays current and still answers \
                questions, and the frame it names is the restarted frame at its \
                first line. it is the **same frame object**, not a new one — a \
                call would make a new frame and this does not, so anything the \
                program holds it by still holds it.\n\n\
                **it runs no block cleanup either.** sending the frame back to \
                its first line is an `f_lineno` jump, so a `with` it was inside \
                gets no `__exit__` and a `try` gets no `finally` — and the body \
                then re-enters that block from the top, so the program has two \
                `__enter__` against one `__exit__` and the first context manager \
                is still open. the answer says `inside_a_block` when this \
                happened.\n\n\
                **a frame that is not the one executing** is reached by forcing \
                out everything above it first, innermost outward. that is the one \
                reset that **does** resume the thread — a frame leaves by \
                returning — so it answers `unwinding` and the reset arrives as a \
                `frame_reset` stop, or `restart_abandoned` if the frame left \
                before the unwinding reached it. between each link the rest of \
                the calling line runs with a value the program never computed, so \
                every frame in the chain is read first and the **whole** request \
                is refused if any of those remainders would call something or \
                write a global, a cell or a name.\n\n\
                refused, by name, when: the frame **writes over one of its own \
                parameters** (the parameter slots are the only place what the \
                call passed still exists, so it has been lost); the code object \
                **closes over names of its own** (`MAKE_CELL` carries no line, so \
                no jump reaches it, and reusing the old cells would let a closure \
                the first pass created see the second pass's writes); it is a \
                generator or coroutine; or bpd could not establish where this \
                interpreter keeps a frame's locals.\n\n\
                `through_the_caller` — **force the frame out and rewind the \
                caller**, so the interpreter builds a frame that has never run. \
                ask for this when a genuinely new frame is what you want. it \
                **resumes the thread**: do not ask this stop anything more; wait \
                for the next stop, which is `restarted` at the first line of the \
                fresh frame, or `restart_abandoned` when it could not be finished \
                — the frame gone and the call not made again, with that stop \
                carrying which of several reasons it was. a **third** thing can \
                happen and is a known gap: another stop (a breakpoint, an \
                exception, a pause, a stopped world) reaching this thread first \
                takes the restart off, and neither of those two arrives.\n\n\
                **it runs no block cleanup.** forcing the frame out is an \
                `f_lineno` jump, so a `with` the frame was inside gets no \
                `__exit__` and a `try` gets no `finally` — measured with a plain \
                class context manager: two `__enter__`, one `__exit__`. what the \
                jump does not run, the frame **dying** can: anything it was the \
                last holder of is finalised at that moment — a `__del__`, or the \
                `GeneratorExit` thrown into a suspended generator, which runs its \
                `finally` and the `__exit__` of any `with` inside it. a \
                `@contextlib.contextmanager` is exactly that shape, so its \
                cleanup **does** run, at a point the program never reached.\n\n\
                **its unit is the caller's line, not the call.** everything on \
                that line runs a second time, so a line carrying anything besides \
                the one call is refused by name: an attribute, a subscript, an \
                operator, a store into an attribute, or a second call. so are a \
                frame with no clean exit, a call the caller has no statement \
                after, and a generator or coroutine. **most of these are exactly \
                what `in_place` does not have to ask**, which is why it is the \
                default.\n\n\
                every refusal is decided off the bytecode **before anything is \
                attempted**, and names what stood in the way — of whichever ways \
                were tried. neither is `undo`: side effects the frame already \
                performed are not undone."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frame": integer(FRAME),
                    "again": {
                        "type": "string",
                        "enum": ["either", "in_place", "through_the_caller"],
                        "description": "which of the two ways to run the frame \
                            again. `either` (the default) prefers running it \
                            where it stands and falls back to rewinding the \
                            caller. `in_place` keeps the same frame object and \
                            never touches the caller. `through_the_caller` builds \
                            a genuinely new frame and resumes the thread",
                    },
                }),
                &[],
            ),
        },
        Tool {
            name: "record",
            title: "record where the program goes",
            description: "start or stop recording the program's path.\n\n\
                **this is the one mode that turns off what makes bpd fast.** a \
                line is normally watched once and then disabled — six callbacks \
                for nine hundred thousand line executions — and a recorder needs \
                every one of them. measured at about 4x a bare run. it is off by \
                default and worth turning on for a region of a run, not a \
                session.\n\n\
                `depth` decides whether it records **where** only or what the \
                frame held as well. the prices are very different and measured \
                — see the trail documentation — so the cheap one is the \
                default and the expensive one is asked for.\n\n\
                what it keeps of a value is **text**, read without running any \
                of the program: no `repr`, no `__len__`, no `__str__`. that is \
                a weaker answer than a repr and it is one that cannot be wrong, \
                and it is what keeps the window bounded — a reference would \
                keep alive objects the program had finished with.\n\n\
                stopping keeps the trail, because stopping is what you do in \
                order to read it."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "on": { "type": "boolean", "description":
                        "whether to record. starting clears any earlier trail, \
                         because one spanning two recordings has a gap in it \
                         that nothing marks" },
                    "depth": {
                        "type": "string",
                        "enum": ["where", "frame", "locals", "values"],
                        "description":
                            "how much of each step to keep. `where` is the \
                             location and is the default. `values` also keeps \
                             what the frame held, rendered as text, and costs \
                             several times as much again. `frame` and `locals` \
                             keep neither and exist so the cost of the other \
                             two can be told apart",
                    },
                }),
                &["on"],
            ),
        },
        Tool {
            name: "trail",
            title: "where the program has been",
            description: "the window of places the program went while recording, \
                oldest first, with the thread that was there.\n\n\
                `dropped` is not decoration: the window holds a fixed number of \
                steps, and anything that fell out of it is counted. a trail \
                whose `dropped` is above zero does **not** begin where the \
                recording did, and reading its oldest entry as the start is the \
                one mistake this answer exists to prevent.\n\n\
                there are no values here. see `record`."
                .to_string(),
            schema: object(
                serde_json::json!({ "session": integer(SESSION) }),
                &[],
            ),
        },
        Tool {
            name: "retainers",
            title: "what is holding an object",
            description: "why an object is still alive. name it with an \
                expression in a frame, the way `evaluate` does — an object has \
                no id of its own that outlives being asked about.\n\n\
                the answer says what holds it and, where the shape can be read, \
                **where inside** each holder it sits: the value under a dict \
                key, an index of a list, an attribute of an object. `through` \
                being absent means the holder's shape could not be read, not \
                that it holds it nowhere.\n\n\
                `coverage` is on every answer and is not a footnote. this walk \
                is the collector's referent graph, which is blind to untracked \
                objects — an int, a str — and to holders that are not python \
                objects at all, **bpd's own included**. a list of holders \
                without that answers a narrower question than the one you asked."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frame": integer("which frame of the stack to evaluate in, \
                                      counting from 0 at the top"),
                    "expression": { "type": "string", "description":
                        "a python expression naming the object to ask about, \
                         evaluated in that frame. it runs the program's own \
                         code exactly as `evaluate` does" },
                }),
                &["expression"],
            ),
        },
        Tool {
            name: "replace_code",
            title: "make the running process run the code on disk",
            description: "you edited a file and the process is still running \
                what was there when it was imported. this replaces the code of \
                every function of that file, in place, **without restarting \
                anything** — `function.__code__` is rebound, so a method is \
                caught with it and every instance that already exists sees the \
                new one immediately.\n\n\
                the top level is **not** re-run. no name is bound or unbound and \
                no object is created, so every reference the program already \
                holds is the one it held before.\n\n\
                it is applicable exactly when every difference between the file \
                on disk and the code that is running is inside the body of a \
                function that exists in both and takes the same arguments. \
                anything else is refused — a changed module body, a function or \
                class added or removed, a changed class body, a changed \
                signature, or a frame anywhere in the process that is running \
                code about to change.\n\n\
                **nothing is ever applied partially.** a refusal changes nothing \
                at all and carries *every* reason it had rather than the first, \
                so one call tells you the whole of what to fix.\n\n\
                a frame running the code is refused for honesty rather than \
                safety: cpython accepts the assignment and the frame in flight \
                would run the old code to completion, which means the process \
                would be running two versions of one function until it returned. \
                let it return first.\n\n\
                what tells you this is worth calling is `source` answering \
                `not_the_same_code`: bpd compiles the file and requires the \
                frame's own code object to be in the result, so that answer \
                means the file on disk is no longer what is running."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "file": {
                        "type": "string",
                        "description": "the file whose code to replace, on the \
                                        debuggee's own filesystem. it is matched \
                                        by filesystem identity rather than by \
                                        path text, so a symlink and the file it \
                                        points at are the same file. a `.by` may \
                                        be named: the interpreter never compiled \
                                        one, so it is resolved to the generated \
                                        python through the build's own source \
                                        map. give this or `files`",
                    },
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "several files, replaced together or not \
                                        at all — every refusal of every one of \
                                        them is collected before anything is \
                                        written. that is what staging a \
                                        basedpython build again produces: the \
                                        transpile is type-directed, so one edit \
                                        can change the python emitted for more \
                                        than one module, and applying some of \
                                        them would leave the process half way \
                                        between two versions of the build. give \
                                        this or `file`",
                    },
                    "remap": {
                        "type": "boolean",
                        "description": "read the build's `_by_sourcemap.py` again \
                                        before replacing anything, and translate \
                                        the whole breakpoint set through the new \
                                        tables. for a basedpython build whose \
                                        tree was just staged again: the map \
                                        beside the generated python was rewritten \
                                        too, so every `.by` breakpoint is armed \
                                        on a generated line that came out of a \
                                        table that no longer describes the tree. \
                                        it happens in the same message as the \
                                        replacement, before any code is swapped, \
                                        because anything split across two would \
                                        leave a window in which another thread's \
                                        location is mapped through the old one. \
                                        defaults to false, which is right for a \
                                        program that is not a basedpython build \
                                        — it has no map to read",
                    },
                    "even_under_a_live_frame": {
                        "type": "boolean",
                        "description": "apply it even where a frame is running \
                                        the code being replaced. defaults to \
                                        false, and that default is a guarantee \
                                        rather than caution: a frame that is \
                                        already running keeps its own code \
                                        object and finishes on the old body, so \
                                        until it returns the process runs two \
                                        versions of one function and a stack is \
                                        evidence about neither. with this on the \
                                        replacement is applied and every frame \
                                        that will finish on the old code is \
                                        named in the answer — a list that is \
                                        true when it is made and not afterwards, \
                                        because those frames return on their own \
                                        schedule and nothing reports when one \
                                        has",
                    },
                }),
                // neither `file` nor `files` alone: one of them is needed and
                // either will do, which `required` cannot say. naming one would
                // make the other unreachable for an agent reading the schema, so
                // the pairing is said in both descriptions and an empty set is
                // refused when it arrives
                &[],
            ),
        },
        Tool {
            name: "threads",
            title: "what every thread of the program is doing",
            description: "the only question that is about threads bpd is **not** \
                holding, and everything it says about one is a sample. two \
                samples are taken `settle_ms` apart and compared: `still` means \
                the thread was in the same place both times.\n\n\
                that is deliberately not a diagnosis. cpython exposes no owner \
                for a lock, so bpd cannot say a thread is waiting for one \
                another thread holds — a thread blocked in `sock.recv` and a \
                thread piled up behind a lock look identical from here.\n\n\
                it needs a held thread to answer on, like everything else."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "settle_ms": integer("how far apart to take the two samples. \
                                          defaults to 50ms, which is long enough \
                                          that a thread going round an ordinary \
                                          python loop is seen to move"),
                }),
                &[],
            ),
        },
        Tool {
            name: "stop_the_world",
            title: "hold every thread that can be held",
            description: "bpd's default is that a stop holds **one** thread and \
                everything else keeps running, on every build — the agent gives \
                the GIL back for the duration of a stop. this asks for the \
                other mode, until the named stop is resumed.\n\n\
                it names the threads it could **not** hold: a thread parked in a \
                C call has released the GIL and reaches no monitoring event, so \
                nothing here can stop it. only an empty `native` is a \
                whole-program snapshot, and every read taken afterwards carries \
                the mode it was taken in."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "settle_ms": integer("how long to wait for the other threads \
                                          to arrive. defaults to 50ms"),
                }),
                &[],
            ),
        },
        Tool {
            name: "run_script",
            title: "run a whole investigation in one call",
            description: "submit a tree of debugger steps and get back **what \
                happened at every one of them**. this is what removes the round \
                trip per *investigation* rather than per operation: `run to the \
                third call with a negative amount, then step until the total \
                changes, and tell me the frame where it did` is one call.\n\n\
                the steps run in bpd, driving the session. only the predicates \
                reach the debuggee — python expressions evaluated in a chosen \
                frame, through the machinery a breakpoint condition uses — so \
                the program is disturbed by exactly the evaluations that were \
                asked for and nothing else.\n\n\
                **the transcript is the answer, not the final state.** every \
                record says which step of the tree it came from, where the held \
                thread was when it ran, and for a branch which way it went. the \
                same script over the same program produces the same transcript, \
                so one can be re-run to confirm a reading — nothing in it is a \
                wall clock reading.\n\n\
                a script drives **one thread**: the one the stop it names holds. \
                a step that fails halts it, and the rest does not run. a budget \
                is required on all three axes, and exhausting one returns the \
                transcript so far with `partial: true` and the bound that bit."
                .to_string(),
            schema: {
                let mut schema = object(
                    serde_json::json!({
                    "session": integer(SESSION),
                        "stop": integer(STOP),
                        "steps": {
                            "type": "array",
                            "description": "the steps, in the order they run",
                            "items": { "$ref": "#/$defs/step" },
                        },
                        "budget": budget(),
                    }),
                    &["steps", "budget"],
                );
                schema["$defs"] = serde_json::json!({ "step": step_definition() });
                schema
            },
        },
        Tool {
            name: "state",
            title: "describe a stop in one call",
            description: "say what you want to know about a stop and get it in \
                **one** answer: frames, the scopes of each of them, expressions \
                evaluated in a frame, and the source around each frame's line. \
                the tree walk — `stack`, then `variables` per scope, then \
                `variables` again for each nested object — is still there and \
                answers identically, because this is composed of the same \
                requests. what it removes is the round trips.\n\n\
                `detail.budget` bounds the **whole** query rather than each read \
                in it, and anything it did not reach is named in `left_out` \
                rather than being absent. the parts are read in this order: the \
                stack, the expressions, then frame by frame the source and the \
                scopes — so the open ended part is what a spent budget cuts.\n\n\
                the answer carries a `snapshot` id. every state is kept, under a \
                digest of itself, and `diff` compares two of them. an id does \
                **not** go stale: it names a reading that was already taken, so \
                it stays valid across any number of resumes — what ends with the \
                stop is asking that stop anything more.\n\n\
                `source` is only ever shown when bpd can prove it: the debuggee \
                compiles the file and checks that this frame's own code object \
                is in what came out. an edited file says so instead."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "stop": integer(STOP),
                    "frames": integer(
                        "how many frames to describe, from the one that stopped. \
                         the scopes and the source are read for these and no \
                         others. defaults to 1, because every extra frame is a \
                         scope read nobody asked for"
                    ),
                    "scopes": {
                        "type": "array",
                        "description": "which scopes of each described frame to \
                                        read. omit for none — a query of \
                                        expressions alone is often the whole \
                                        question",
                        "items": {
                            "type": "string",
                            "enum": ["local", "cell", "free", "global"],
                        },
                    },
                    "expressions": {
                        "type": "array",
                        "description": "expressions to evaluate. **this runs the \
                                        program's own code**, by request. one \
                                        that raises is answered with the \
                                        exception, which is what it did",
                        "items": object(
                            serde_json::json!({
                                "expression": { "type": "string", "description":
                                    "the expression, as python" },
                                "frame": integer(FRAME),
                            }),
                            &["expression"],
                        ),
                    },
                    "source": integer(
                        "how many lines either side of each frame's current line. \
                         omit for none. the window is clamped to the code object \
                         that was verified, because nothing outside it was checked"
                    ),
                    "detail": detail(),
                }),
                &[],
            ),
        },
        Tool {
            name: "diff",
            title: "what changed between two states",
            description: "compare two `snapshot` ids and get back **the \
                difference**, rather than both states to compare yourself.\n\n\
                three things keep it from lying. a value that a bound cut short \
                in either snapshot is in `not_compared`, never in `unchanged` — \
                \"unchanged\" is a claim, and half a value is not evidence for \
                it. a depth of the stack that is running different code in the \
                two is not compared either, because depth is a position rather \
                than an identity. and something only one of the two read is \
                unknown rather than absent, so it is never reported as added or \
                removed.\n\n\
                a snapshot does not expire, so two stops any distance apart can \
                be compared. what each side says is the mode it was read in: in \
                non-stop mode the rest of the program was running, so each state \
                is a sample and the difference is between two samples."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "session": integer(SESSION),
                    "before": { "type": "string", "description":
                        "the snapshot id to compare from, as `state` gave it out" },
                    "after": { "type": "string", "description":
                        "the snapshot id to compare to" },
                }),
                &["before", "after"],
            ),
        },
        Tool {
            name: "terminate",
            title: "end the debuggee",
            description: "kill the program. the last resort rather than a \
                resume: a program that is running cannot be asked anything, so \
                this is what is left when it will not stop on its own. the \
                session has no program after it."
                .to_string(),
            schema: object(serde_json::json!({ "session": integer(SESSION) }), &[]),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_tool_is_named_once_and_says_what_it_does() {
        let all = tools();
        let names: BTreeSet<&str> = all.iter().map(|tool| tool.name).collect();
        assert_eq!(names.len(), all.len(), "two tools share a name");

        for tool in &all {
            assert!(
                tool.description.len() > 80,
                "`{}` is the surface an agent learns bpd from and its \
                 description is {} characters",
                tool.name,
                tool.description.len()
            );
        }
    }

    #[test]
    fn no_schema_accepts_an_argument_it_does_not_name() {
        // a misspelled argument that took its default silently is the failure
        // this closes: `deadline_ms` missed would be a call that never returns,
        // and `depth` missed would be a value read shallower than asked for
        for tool in tools() {
            assert_eq!(
                tool.schema["additionalProperties"],
                serde_json::Value::Bool(false),
                "`{}` accepts arguments it does not name",
                tool.name
            );
        }
    }

    #[test]
    fn every_control_tool_requires_a_deadline() {
        // a control tool without one is a call that hangs on a program which
        // does not stop, and hanging is what this whole interface is not
        for name in [
            "continue_",
            "wait",
            "pause",
            "step_over",
            "step_in",
            "step_out",
        ] {
            let tool = tools()
                .into_iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("`{name}` is not a tool"));
            let required = tool.schema["required"]
                .as_array()
                .expect("a schema names what it requires");
            assert!(
                required.contains(&serde_json::json!("deadline_ms")),
                "`{name}` blocks on the program and does not require a deadline"
            );
        }
    }
}
