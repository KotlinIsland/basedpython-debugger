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
                    "raised": { "type": "boolean", "description":
                        "stop where an exception is raised, caught or not" },
                    "uncaught": { "type": "boolean", "description":
                        "stop where an exception leaves the outermost frame" },
                }),
                &[],
            ),
        },
        Tool {
            name: "continue_",
            title: "let every held thread go, and return the next stop",
            description: "resume everything bpd is holding and wait for what the \
                program does next — **the stop is the return value**. the answer \
                is one of: `stopped` (with the stop and its top frames), \
                `exited` (with the exit code), `finishing` (the program ran to \
                its end with threads still held, so it cannot exit until they \
                are resumed), or `timed_out`."
                .to_string(),
            schema: object(
                serde_json::json!({
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
            title: "re-enter the held frame from the top",
            description: "`set_next_statement` to the first line of the frame's \
                own code object, worked out in the debuggee because the code \
                object is the only thing that knows it. the same answer, and the \
                same limits — including that only the frame the thread is \
                executing can move.\n\n\
                **it re-enters with what the parameters hold now.** a parameter \
                the frame has already assigned to holds the new value: nothing \
                captured what the call was made with, and capturing it would mean \
                copying every argument of every call in the process. so this is \
                not `undo` — side effects the frame already performed are not \
                undone, and the frames it called are gone.\n\n\
                a generator, a coroutine or an async generator frame is refused: \
                the first instruction of such a code object is the `RESUME` its \
                driver sends into rather than the top of the body, and moving \
                there ends the frame instead of running it again. \
                `set_next_statement` to a line of the body works there."
                .to_string(),
            schema: object(
                serde_json::json!({
                    "stop": integer(STOP),
                    "frame": integer(FRAME),
                }),
                &[],
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
                    "file": {
                        "type": "string",
                        "description": "the file whose code to replace, on the \
                                        debuggee's own filesystem. it is matched \
                                        by filesystem identity rather than by \
                                        path text, so a symlink and the file it \
                                        points at are the same file",
                    },
                }),
                &["file"],
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
            schema: object(serde_json::json!({}), &[]),
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
