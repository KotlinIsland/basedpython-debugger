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
    from the one that stopped. 0 for none. the answer says how deep the stack \
    really is either way";

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
fn detail() -> serde_json::Value {
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
                         cpython 3.13 or newer; anything else is refused by \
                         name rather than limped along" },
                    "args": { "type": "array", "items": { "type": "string" },
                        "description": "arguments for the program, exactly as it \
                                        receives them" },
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
                an empty `running` means nothing will arrive until some thread \
                runs python again: every one of them is parked in a C call, \
                where there is no monitoring event to hold one at."
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
            name: "evaluate",
            title: "evaluate a python expression in a frame",
            description: "compiled at the request and evaluated against the \
                frame's own globals and locals, which is what `LOAD_NAME` sees. \
                **this runs the program's own code**, by request.\n\n\
                an expression that raises, and one that does not compile, are \
                both answers carrying the exception rather than failures: the \
                interpreter is the authority on what an expression is. this \
                thread's breakpoints are suppressed while it runs."
                .to_string(),
            schema: {
                let mut properties = frame_properties();
                properties["expression"] = serde_json::json!({
                    "type": "string",
                    "description": "the expression, as python",
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
