//! the server: MCP messages in, [`Request`]s out, one answer per call
//!
//! ## why there is only one thread here
//!
//! the DAP adapter needs two, because DAP answers a `continue` immediately and
//! the *stop* arrives later as an event — so something has to be able to reach a
//! program that is running. this front end has no events at all: a control tool
//! resumes the program and **returns the stop it produced**, and the deadline
//! every one of them carries is what makes that safe. there is nothing to
//! correlate, nothing to poll, and nothing that can block for ever
//!
//! ## what it does not do
//!
//! it makes no decision about the program. every question becomes a
//! [`Request`], and the answer is rendered rather than interpreted. two things
//! that look like decisions are not:
//!
//! - **a tool that names no stop.** the rule for that is
//!   [`bpd_core::only_stop`], in the core, because every front end has to apply
//!   it and two of them applying their own would make the same call mean two
//!   things
//! - **breakpoint ids.** a breakpoint's id is its position in the set the client
//!   sent, counting from one. there is no bookkeeping behind it:
//!   [`Request::SetBreakpoints`] already replaces the whole set, so unlike DAP —
//!   which replaces one *file's* breakpoints at a time and has to reassemble the
//!   union — there is nothing here to accumulate

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bpd_core::{
    Detail, HitCondition, LogRecord, Reporting, Request, Response, Running, Scope,
    SourceBreakpoint, StepKind, Stop, Threads, Which, exit_code, only_stop,
};

use crate::render;
use crate::session::{Configuration, Launcher, ProgramOutput, Session, Started, Stream, describe};
use crate::tools::tools;
use crate::wire::{Incoming, Reader, Writer, code};

/// the MCP revision this server implements
///
/// a client that asks for a revision this server understands is answered with
/// its own, since nothing in what this server speaks differs between them.
/// anything else is answered with this, which is the client's cue to decide
/// whether it can go on
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// the MCP revisions this server can be spoken to in
const UNDERSTOOD: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// what an agent is told before it has read any tool description
///
/// the host decides whether a *resource* is ever read and the user decides
/// whether a *prompt* is ever invoked, so nothing load bearing may live in
/// either. this and the tool schemas are what always arrive
const INSTRUCTIONS: &str = "bpd is a python debugger. every control tool — \
    `continue_`, `step_over`, `step_in`, `step_out`, `wait`, `pause` — blocks \
    until the program stops again and **returns that stop**, so there are no \
    events to correlate. each of them requires a `deadline_ms`, and when it \
    passes the answer says the program is still running rather than inventing a \
    location for it.\n\n\
    start with `launch`, which holds the program before its first statement. \
    set breakpoints while something is held: the agent binds one on a python \
    thread it is holding, so a running program has to be paused first.\n\n\
    a stop holds **one thread** and the rest of the program keeps running, so \
    several stops can be outstanding at once and a tool that is about one names \
    it. every read says which mode it was taken in, because a value read while \
    the rest of the program runs is a sample rather than a snapshot.";

/// how many frames of a stop a control tool returns when it is not told
const FRAMES_BY_DEFAULT: u32 = 5;

/// how much of the program's own output is kept between calls
///
/// the most recent, because what a program printed just before it stopped is
/// what the stop is about. whatever fell off the front is counted and reported:
/// an agent cannot see an elision a person would
const OUTPUT_KEPT: usize = 64 << 10;

/// how many logpoint records are kept between calls
///
/// there is no bound on how many a logpoint produces — one on a hot line
/// produces millions — so this keeps the first and counts the rest
const LOGS_KEPT: usize = 200;

/// serve one MCP client over `input` and `output`
///
/// returns when the client hangs up. the debuggee never outlives it: a client
/// that vanishes leaves a program running with nothing watching it, which is the
/// state the agent itself refuses to be in
pub fn serve(
    launcher: &mut dyn Launcher,
    input: Box<dyn std::io::Read + Send>,
    output: Box<dyn std::io::Write + Send>,
) -> Result<(), crate::wire::Error> {
    let mut reader = Reader::new(input);
    let mut writer = Writer::new(output);
    let mut server = Server::new(launcher);

    let served = loop {
        match reader.next_message() {
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
            // a notification carries no id, so JSON-RPC allows no answer to
            // one and there is nothing to say about an unknown one. nothing is
            // gated on `notifications/initialized` either: MCP lets a client
            // send requests before the handshake finishes, and a server that
            // refused them would be refusing a conforming client
            Ok(Some(Incoming::Notification { .. })) => {}
            Ok(Some(Incoming::Unusable { id, reason })) => {
                // answered under a null id when there was none, which is what
                // JSON-RPC says about a message it could not read. a client
                // whose messages are silently ignored cannot tell this server
                // apart from one that has hung
                let id = id.unwrap_or(serde_json::Value::Null);
                if let Err(error) = writer.failure(&id, code::INVALID_REQUEST, &reason) {
                    break Err(error);
                }
            }
            Ok(Some(Incoming::Request { id, method, params })) => {
                let answered = match server.answer(&method, &params) {
                    Ok(result) => writer.result(&id, result),
                    Err(Refused { code, reason }) => writer.failure(&id, code, &reason),
                };
                if let Err(error) = answered {
                    break Err(error);
                }
            }
        }
    };

    server.end();
    served
}

/// a request this server will not answer, as JSON-RPC carries one
///
/// distinct from a **tool** that failed, which is a successful call whose
/// content says what went wrong — that is the shape an agent actually reads. a
/// `Refused` is for the protocol going wrong: a method that does not exist,
/// arguments that are not the shape the schema says
struct Refused {
    code: i64,
    reason: String,
}

/// what a tool call answered: json, or the reason it could not
type Answered = Result<serde_json::Value, String>;

/// how the MCP server answers one client
struct Server<'a> {
    launcher: &'a mut dyn Launcher,
    session: Option<Box<dyn Session>>,
    /// what the program has written to its own stdout and stderr
    program: Arc<Captured>,
    /// what the debuggee said that answers nothing — logpoints, pause acks
    said: Said,
    /// the breakpoints the last `set_breakpoints` asked for
    ///
    /// kept so that a rebinding arriving later can be reported against what was
    /// asked for. it is what the *client* said and nothing about the program
    requested: Vec<SourceBreakpoint>,
}

impl<'a> Server<'a> {
    fn new(launcher: &'a mut dyn Launcher) -> Self {
        Self {
            launcher,
            session: None,
            program: Arc::new(Captured::default()),
            said: Said::default(),
            requested: Vec::new(),
        }
    }

    fn answer(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, Refused> {
        match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(serde_json::json!({})),
            "tools/list" => Ok(serde_json::json!({
                "tools": tools().iter().map(crate::tools::Tool::listing).collect::<Vec<_>>(),
            })),
            "tools/call" => self.call(params),
            other => Err(Refused {
                code: code::METHOD_NOT_FOUND,
                reason: format!(
                    "bpd's MCP server does not implement `{other}`. it offers \
                     `initialize`, `ping`, `tools/list` and `tools/call`, and \
                     declares only the `tools` capability — there are no \
                     resources or prompts to list"
                ),
            }),
        }
    }

    fn call(&mut self, params: &serde_json::Value) -> Result<serde_json::Value, Refused> {
        let name = params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Refused {
                code: code::INVALID_PARAMS,
                reason: "a `tools/call` arrived with no `name`".to_string(),
            })?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let arguments = if arguments.is_null() {
            serde_json::json!({})
        } else {
            arguments
        };

        let known: Vec<&str> = tools().iter().map(|tool| tool.name).collect();
        if !known.contains(&name) {
            return Err(Refused {
                code: code::INVALID_PARAMS,
                reason: format!("`{name}` is not a tool bpd offers. it offers: {known:?}"),
            });
        }

        let answered = self.perform(name, &arguments);
        Ok(match answered {
            Ok(mut result) => {
                self.attach(&mut result);
                content(&text_of(&result), false)
            }
            // a tool failure is a *successful* call whose content says what went
            // wrong. that is what an agent reads — a JSON-RPC error is for the
            // protocol, and a client is entitled to hide one from the model
            Err(reason) => content(&reason, true),
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "it is the tool table. one arm per tool, in the order \
                  `tools/list` reports them, is what makes a missing one visible"
    )]
    fn perform(&mut self, name: &str, arguments: &serde_json::Value) -> Answered {
        match name {
            "launch" => {
                let args: Launch = parse(arguments)?;
                self.launch(args)
            }
            "set_breakpoints" => {
                let args: SetBreakpoints = parse(arguments)?;
                self.set_breakpoints(args)
            }
            "set_exception_breakpoints" => {
                let args: ExceptionArgs = parse(arguments)?;
                match self.ask(Request::SetExceptionBreakpoints {
                    raised: args.raised,
                    uncaught: args.uncaught,
                })? {
                    Response::ExceptionBreakpoints(armed) => Ok(serde_json::json!({
                        // read back off the agent rather than echoed from the
                        // request: what is armed is what the agent says is armed
                        "raised": armed.raised,
                        "uncaught": armed.uncaught,
                    })),
                    other => unreachable!("exception breakpoints were answered with {other:?}"),
                }
            }
            "continue_" => {
                let args: Waiting = parse(arguments)?;
                let ran = self.ran(Request::Run {
                    deadline: Some(args.deadline()),
                })?;
                self.outcome(ran, args.frames)
            }
            "step_over" => self.step(arguments, StepKind::Over),
            "step_in" => self.step(arguments, StepKind::In),
            "step_out" => self.step(arguments, StepKind::Out),
            "wait" => {
                let args: Waiting = parse(arguments)?;
                let ran = self.ran(Request::Wait {
                    deadline: Some(args.deadline()),
                })?;
                self.outcome(ran, args.frames)
            }
            "pause" => {
                let args: Waiting = parse(arguments)?;
                let running = match self.ask(Request::Pause)? {
                    Response::Pausing { running } => running,
                    other => unreachable!("a pause was answered with {other:?}"),
                };
                let ran = self.ran(Request::Wait {
                    deadline: Some(args.deadline()),
                })?;
                let mut answer = self.outcome(ran, args.frames)?;
                answer["running"] = serde_json::json!(running);
                if running.is_empty() {
                    answer["note"] = "the pause is armed and nothing is going to \
                         arrive until some thread runs python again: every \
                         thread was parked in a C call, where there is no \
                         monitoring event to hold one at"
                        .into();
                }
                Ok(answer)
            }
            "resume" => {
                let args: Resume = parse(arguments)?;
                let which = match args.threads {
                    Some(threads) => Which::Named { threads },
                    None => Which::All,
                };
                match self.ask(Request::Resume { which })? {
                    Response::Resumed { threads } => Ok(serde_json::json!({
                        "resumed": threads,
                        "held": self.held_now(),
                    })),
                    other => unreachable!("a resume was answered with {other:?}"),
                }
            }
            "stack" => {
                let args: StackArgs = parse(arguments)?;
                let stop = self.stop_of(args.stop, "the stack")?;
                match self.ask(Request::Stack {
                    stop,
                    top: args.top,
                })? {
                    Response::Stack(walked) => Ok(render::stack(&walked)),
                    other => unreachable!("a stack walk was answered with {other:?}"),
                }
            }
            "variables" => {
                let args: VariablesArgs = parse(arguments)?;
                let frame = self.frame_of(args.stop, args.frame, "the variables of a scope")?;
                match self.ask(Request::Variables {
                    frame,
                    scope: args.scope,
                    detail: args.detail,
                })? {
                    Response::Variables(read) => Ok(render::variables(&read)),
                    other => unreachable!("a scope read was answered with {other:?}"),
                }
            }
            "evaluate" => {
                let args: EvaluateArgs = parse(arguments)?;
                let frame = self.frame_of(args.stop, args.frame, "evaluating an expression")?;
                match self.ask(Request::Evaluate {
                    frame,
                    expression: args.expression,
                    detail: args.detail,
                })? {
                    Response::Evaluated(result) => Ok(render::evaluated(&result)),
                    other => unreachable!("an evaluation was answered with {other:?}"),
                }
            }
            "set_variable" => {
                let args: SetVariableArgs = parse(arguments)?;
                let frame = self.frame_of(args.stop, args.frame, "writing a variable")?;
                match self.ask(Request::SetVariable {
                    frame,
                    scope: args.scope,
                    name: args.name,
                    value: args.value,
                    detail: args.detail,
                })? {
                    Response::Evaluated(result) => Ok(render::evaluated(&result)),
                    other => unreachable!("a variable write was answered with {other:?}"),
                }
            }
            "threads" => {
                let args: ThreadsArgs = parse(arguments)?;
                match self.ask(Request::Threads {
                    settle: args.settle(),
                })? {
                    Response::Threads(census) => Ok(render::threads(&census)),
                    other => unreachable!("a thread census was answered with {other:?}"),
                }
            }
            "stop_the_world" => {
                let args: WorldArgs = parse(arguments)?;
                let stop = self.stop_of(args.stop, "stopping the world")?;
                match self.ask(Request::StopTheWorld {
                    stop,
                    settle: args.settle(),
                })? {
                    Response::WorldStopped(stopped) => Ok(render::world(&stopped)),
                    other => unreachable!("stopping the world was answered with {other:?}"),
                }
            }
            "run_script" => {
                let args: RunScript = parse(arguments)?;
                let stop = self.stop_of(args.stop, "running a debug script")?;
                match self.ask(Request::RunScript {
                    stop,
                    script: bpd_core::Script {
                        steps: args.steps,
                        budget: args.budget,
                    },
                })? {
                    Response::Transcript(ran) => Ok(render::transcript(&ran)),
                    other => unreachable!("a debug script was answered with {other:?}"),
                }
            }
            "terminate" => {
                let _: Empty = parse(arguments)?;
                let session = self
                    .session
                    .as_mut()
                    .ok_or_else(|| NO_PROGRAM.to_string())?;
                session
                    .terminate()
                    .map_err(|error| describe(error.as_ref()))?;
                self.session = None;
                Ok(serde_json::json!({ "terminated": true }))
            }
            other => unreachable!("`{other}` was accepted as a tool and has no implementation"),
        }
    }

    // ---- the tools that need more than one request -----------------------

    fn launch(&mut self, args: Launch) -> Answered {
        if self.session.is_some() {
            return Err("this session already has a program. `terminate` it \
                        before launching another"
                .to_string());
        }

        let configuration = Configuration {
            program: args.program,
            python: args.python,
            args: args.args,
        };
        let started = self
            .launcher
            .launch(
                &configuration,
                Arc::clone(&self.program) as Arc<dyn ProgramOutput>,
            )
            .map_err(|error| describe(error.as_ref()))?;

        match started {
            Started::Stopped(session) => {
                self.session = Some(session);
                let held = self.held_stops();
                let entry = held.into_iter().next().ok_or_else(|| {
                    "the program was launched and reported no stop, so there is \
                     nothing held to bind a breakpoint against"
                        .to_string()
                })?;
                let mut answer = render::stop(&entry);
                answer["outcome"] = "stopped".into();
                self.with_frames(&mut answer, entry.stop, args.frames)?;
                Ok(answer)
            }
            Started::ExitedBeforeStopping { code } => Ok(serde_json::json!({
                "outcome": "exited_before_stopping",
                "exit_code": code,
                "note": "the program never reached its first statement — it did \
                         not compile, or the interpreter refused to start it. \
                         what it said is in `output`, in the interpreter's own \
                         words",
            })),
        }
    }

    fn set_breakpoints(&mut self, args: SetBreakpoints) -> Answered {
        // the id is the position in the set, counting from one. there is nothing
        // to remember: the request replaces the whole set, so an id means the
        // same thing to the client and to the agent without a table in between
        let breakpoints: Vec<SourceBreakpoint> = args
            .breakpoints
            .into_iter()
            .enumerate()
            .map(|(index, wanted)| {
                let id = u32::try_from(index + 1)
                    .expect("a set of more than four billion breakpoints cannot be sent");
                SourceBreakpoint {
                    id,
                    file: wanted.file,
                    line: wanted.line,
                    condition: wanted.condition,
                    hits: wanted.hits,
                    log: wanted.log,
                }
            })
            .collect();
        self.requested.clone_from(&breakpoints);

        match self.ask(Request::SetBreakpoints {
            breakpoints: breakpoints.clone(),
        })? {
            Response::BreakpointsResolved { resolved } => Ok(serde_json::json!({
                "breakpoints": render::breakpoints(&resolved, &breakpoints),
            })),
            other => unreachable!("a breakpoint set was answered with {other:?}"),
        }
    }

    fn step(&mut self, arguments: &serde_json::Value, kind: StepKind) -> Answered {
        let args: Stepping = parse(arguments)?;
        let stop = self.stop_of(args.stop, "stepping a thread")?;
        match self.ask(Request::Step { stop, kind })? {
            Response::Resumed { .. } => {}
            other => unreachable!("a step was answered with {other:?}"),
        }
        let ran = self.ran(Request::Wait {
            deadline: Some(args.deadline()),
        })?;
        self.outcome(ran, args.frames)
    }

    // ---- what the program did --------------------------------------------

    fn ran(&mut self, request: Request) -> Result<Running, String> {
        match self.ask(request)? {
            Response::Ran(running) => Ok(running),
            other => unreachable!("a wait was answered with {other:?}"),
        }
    }

    /// what a control tool answers: the stop it produced, or why there is none
    fn outcome(&mut self, running: Running, frames: u32) -> Answered {
        let rebound = match &running {
            Running::Stopped { rebound, .. }
            | Running::Exited { rebound, .. }
            | Running::Finishing { rebound, .. }
            | Running::StillRunning { rebound, .. } => rebound.clone(),
        };

        let mut answer = match running {
            Running::Stopped { stop, .. } => {
                let mut stopped = render::stop(&stop);
                stopped["outcome"] = "stopped".into();
                self.with_frames(&mut stopped, stop.stop, frames)?;
                stopped
            }
            Running::Exited { status, .. } => serde_json::json!({
                "outcome": "exited",
                "exit_code": exit_code(status),
            }),
            Running::Finishing { threads, .. } => serde_json::json!({
                "outcome": "finishing",
                "held": threads,
                "note": "the program has run to its end with threads still held. \
                         it cannot exit: the interpreter finalizes by joining \
                         its non-daemon threads and a held one cannot be joined, \
                         so the process is sitting there until they are resumed",
            }),
            // never rendered as a stop. no thread is held, nothing was read off
            // the program, and the program is executing while this is read
            Running::StillRunning { waited, .. } => serde_json::json!({
                "outcome": "timed_out",
                "waited_ms": u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                "held": self.held_now(),
                "note": "the deadline passed and the program is still running. \
                         this is not a stop: nothing was held and nothing was read \
                         off the program, so bpd reports no location for it — \
                         not even a sampled one. everything the agent inside the \
                         debuggee answers, it answers on a thread it is holding, \
                         and that includes the thread census. `wait` keeps \
                         waiting without touching the program; `pause` holds the \
                         next thread that reaches a line and then it can be asked",
            }),
        };

        if !rebound.is_empty() {
            // loading a file changes what a breakpoint resolves to, and it
            // happened while the program was running. a client never told would
            // go on believing a breakpoint is unbound
            answer["rebound"] = serde_json::json!(render::breakpoints(&rebound, &self.requested));
        }
        Ok(answer)
    }

    /// add the top of a stop's stack to an answer about it
    fn with_frames(
        &mut self,
        answer: &mut serde_json::Value,
        stop: u64,
        frames: u32,
    ) -> Result<(), String> {
        if frames == 0 {
            return Ok(());
        }
        match self.ask(Request::Stack {
            stop,
            top: Some(frames),
        })? {
            Response::Stack(walked) => {
                let rendered = render::stack(&walked);
                for (key, value) in rendered
                    .as_object()
                    .expect("a rendered stack is an object")
                    .clone()
                {
                    answer[key] = value;
                }
                Ok(())
            }
            other => unreachable!("a stack walk was answered with {other:?}"),
        }
    }

    // ---- the plumbing -----------------------------------------------------

    /// ask the session for something, rendering a failure as a tool failure
    fn ask(&mut self, request: Request) -> Result<Response, String> {
        let Self { session, said, .. } = self;
        let session = session.as_mut().ok_or_else(|| NO_PROGRAM.to_string())?;
        session
            .dispatch(request, said)
            .map_err(|error| describe(error.as_ref()))
    }

    fn held_stops(&self) -> Vec<Stop> {
        self.session
            .as_ref()
            .map(|session| session.held())
            .unwrap_or_default()
    }

    fn held_now(&self) -> Vec<serde_json::Value> {
        self.held_stops().iter().map(render::stop).collect()
    }

    /// the stop a tool is about, or the rule for why it cannot be decided
    fn stop_of(&self, given: Option<u64>, wanted: &'static str) -> Result<u64, String> {
        match given {
            Some(stop) => Ok(stop),
            None => only_stop(&self.held_stops(), wanted).map_err(|error| error.to_string()),
        }
    }

    fn frame_of(
        &self,
        stop: Option<u64>,
        depth: u32,
        wanted: &'static str,
    ) -> Result<bpd_core::FrameId, String> {
        Ok(bpd_core::FrameId {
            stop: self.stop_of(stop, wanted)?,
            depth,
        })
    }

    /// hang the program's own output, and what a logpoint said, on an answer
    ///
    /// only when there is something. an `output` key that is always there and
    /// usually empty is noise in a context window that is being spent
    fn attach(&mut self, answer: &mut serde_json::Value) {
        if let Some(output) = self.program.take() {
            answer["output"] = output;
        }
        if let Some(said) = self.said.take() {
            answer["logged"] = said;
        }
        if !answer.is_object() {
            unreachable!("every tool answers with an object, and one answered with {answer}");
        }
    }

    /// end the debuggee, if one was ever started
    ///
    /// a failure here is not reportable: the client is going away, and the only
    /// thing left that could be told is the process that has just been asked to
    /// stop existing
    fn end(&mut self) {
        if let Some(session) = self.session.as_mut() {
            let ended = session.terminate();
            drop(ended);
        }
    }
}

/// what a tool that needs a program says when there is none
const NO_PROGRAM: &str = "no program is running. `launch` one first — bpd has no \
    attach yet, because attaching is PEP 768 and needs cpython 3.14, and bpd \
    refuses rather than injecting by another route";

/// the answer to `initialize`
fn initialize(params: &serde_json::Value) -> serde_json::Value {
    let asked = params
        .get("protocolVersion")
        .and_then(serde_json::Value::as_str);
    // a version this server understands is echoed; anything else is answered
    // with the one it speaks, which is the client's cue to decide
    let version = match asked {
        Some(asked) if UNDERSTOOD.contains(&asked) => asked,
        _ => PROTOCOL_VERSION,
    };

    serde_json::json!({
        "protocolVersion": version,
        // only `tools`. resources are pulled at the host's discretion and
        // prompts are invoked by the user, so neither is a surface an agent can
        // be relied on to see — declaring one that carried semantics would be
        // documenting an interface that does not explain itself
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "bpd",
            "title": "bpd — a debugger for python",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// one tool result, as MCP carries it
fn content(text: &str, failed: bool) -> serde_json::Value {
    serde_json::json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": failed,
    })
}

fn text_of(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).expect("a value built from json values serialises")
}

/// read a tool's arguments, refusing one it does not name
fn parse<T: serde::de::DeserializeOwned>(arguments: &serde_json::Value) -> Result<T, String> {
    serde_json::from_value(arguments.clone()).map_err(|error| {
        format!(
            "these arguments are not what this tool takes: {error}. its schema \
             is in `tools/list`, and it accepts no argument it does not name — a \
             misspelled one that quietly took its default would be a setting \
             asked for and never applied"
        )
    })
}

/// the debuggee's own stdout and stderr, kept until a tool call carries them
///
/// the server's stdout is the protocol, so the program cannot share it. what it
/// wrote is held here and reported on the next answer — most recent first to
/// fall off, because what a program printed just before it stopped is what the
/// stop is about
#[derive(Debug, Default)]
struct Captured {
    held: Mutex<Buffer>,
}

#[derive(Debug, Default)]
struct Buffer {
    text: String,
    dropped: usize,
}

impl Captured {
    fn take(&self) -> Option<serde_json::Value> {
        let mut held = self.held.lock().expect(HOLDING);
        if held.text.is_empty() && held.dropped == 0 {
            return None;
        }
        let text = std::mem::take(&mut held.text);
        let dropped = std::mem::take(&mut held.dropped);
        let mut rendered = serde_json::json!({ "text": text });
        if dropped > 0 {
            rendered["dropped_bytes"] = dropped.into();
            rendered["says"] = format!(
                "the program produced more than the {OUTPUT_KEPT} bytes bpd \
                 keeps between calls, so {dropped} earlier bytes are gone. the \
                 text here is the most recent"
            )
            .into();
        }
        Some(rendered)
    }
}

const HOLDING: &str =
    "nothing panics holding the captured output: every path through it is a push or a take";

impl ProgramOutput for Captured {
    fn wrote(&self, stream: Stream, text: &str) {
        use std::fmt::Write as _;

        let mut held = self.held.lock().expect(HOLDING);
        // categorised inline rather than in two buffers: the order the program
        // wrote them in is information, and two lists lose it
        write!(held.text, "[{stream}] {text}").expect("writing to a string cannot fail");
        if held.text.len() > OUTPUT_KEPT {
            let mut cut = held.text.len() - OUTPUT_KEPT;
            while !held.text.is_char_boundary(cut) {
                cut += 1;
            }
            held.dropped += cut;
            held.text.drain(..cut);
        }
    }
}

/// what the debuggee said that answers nothing
#[derive(Debug, Default)]
struct Said {
    logs: Vec<LogRecord>,
    dropped: usize,
    pausing: Vec<Vec<u64>>,
}

impl Said {
    fn take(&mut self) -> Option<serde_json::Value> {
        if self.logs.is_empty() && self.dropped == 0 && self.pausing.is_empty() {
            return None;
        }
        let records: Vec<serde_json::Value> = self.logs.iter().map(render::logged).collect();
        let dropped = std::mem::take(&mut self.dropped);
        self.logs.clear();
        let pausing = std::mem::take(&mut self.pausing);

        let mut rendered = serde_json::json!({ "records": records });
        if dropped > 0 {
            rendered["dropped"] = dropped.into();
            rendered["says"] = format!(
                "a logpoint produced more than the {LOGS_KEPT} records bpd keeps \
                 between calls, so {dropped} later ones are gone. these are the \
                 first"
            )
            .into();
        }
        if !pausing.is_empty() {
            rendered["pause_armed_while_running"] = serde_json::json!(pausing);
        }
        Some(rendered)
    }
}

impl Reporting for Said {
    fn logged(&mut self, record: LogRecord) {
        if self.logs.len() < LOGS_KEPT {
            self.logs.push(record);
        } else {
            self.dropped += 1;
        }
    }

    fn pausing(&mut self, running: Vec<u64>) {
        self.pausing.push(running);
    }
}

// ---- the arguments, one struct per tool ----------------------------------
//
// `deny_unknown_fields` on every one of them, matching the schemas'
// `additionalProperties: false`. a misspelled argument that took its default
// silently would be `deadline_ms` missed on a call that then never returns

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    program: PathBuf,
    #[serde(default = "python")]
    python: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "frames")]
    frames: u32,
}

fn python() -> String {
    "python3".to_string()
}

const fn frames() -> u32 {
    FRAMES_BY_DEFAULT
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBreakpoints {
    breakpoints: Vec<Wanted>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Wanted {
    file: PathBuf,
    line: u32,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    hits: Option<HitCondition>,
    #[serde(default)]
    log: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExceptionArgs {
    #[serde(default)]
    raised: bool,
    #[serde(default)]
    uncaught: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Waiting {
    deadline_ms: u64,
    #[serde(default = "frames")]
    frames: u32,
}

impl Waiting {
    const fn deadline(&self) -> Duration {
        Duration::from_millis(self.deadline_ms)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stepping {
    deadline_ms: u64,
    #[serde(default = "frames")]
    frames: u32,
    #[serde(default)]
    stop: Option<u64>,
}

impl Stepping {
    const fn deadline(&self) -> Duration {
        Duration::from_millis(self.deadline_ms)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    #[serde(default)]
    threads: Option<Vec<u64>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StackArgs {
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    top: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VariablesArgs {
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    scope: Scope,
    #[serde(default)]
    detail: Detail,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateArgs {
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    expression: String,
    #[serde(default)]
    detail: Detail,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetVariableArgs {
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    scope: Scope,
    name: String,
    value: String,
    #[serde(default)]
    detail: Detail,
}

/// a whole investigation, submitted as data
///
/// the steps and the budget are `bpd_core`'s own types, so what the schema
/// documents and what the engine walks are one definition. a misspelled field
/// inside a step is refused by name for the reason every other argument is
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunScript {
    #[serde(default)]
    stop: Option<u64>,
    steps: Vec<bpd_core::Step>,
    budget: bpd_core::Budget,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadsArgs {
    #[serde(default)]
    settle_ms: Option<u64>,
}

impl ThreadsArgs {
    fn settle(&self) -> Duration {
        self.settle_ms
            .map_or(Threads::SETTLE, Duration::from_millis)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorldArgs {
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    settle_ms: Option<u64>,
}

impl WorldArgs {
    fn settle(&self) -> Duration {
        self.settle_ms
            .map_or(Threads::SETTLE, Duration::from_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_asking_for_a_version_this_server_knows_is_answered_in_it() {
        let known = initialize(&serde_json::json!({ "protocolVersion": "2024-11-05" }));
        assert_eq!(known["protocolVersion"], "2024-11-05");

        // and one it has never heard of is answered with what this server
        // speaks, rather than agreed to
        let unknown = initialize(&serde_json::json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(unknown["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            unknown["capabilities"]["resources"],
            serde_json::Value::Null,
            "only tools are model-controlled, and nothing is declared that is not implemented"
        );
    }

    #[test]
    fn the_programs_output_is_cut_from_the_front_and_says_how_much_went() {
        let captured = Captured::default();
        for _ in 0..1000 {
            captured.wrote(Stream::Stdout, &"x".repeat(100));
        }
        captured.wrote(Stream::Stdout, "the last thing it said\n");

        let taken = captured.take().expect("it wrote something");
        let text = taken["text"].as_str().expect("output is text");
        assert!(
            text.ends_with("the last thing it said\n"),
            "what a program printed just before it stopped is what is kept"
        );
        assert!(
            taken["dropped_bytes"].as_u64().expect("some was dropped") > 0,
            "an elision has to say it happened: {taken}"
        );

        assert!(
            captured.take().is_none(),
            "taking it twice would report the same output on two answers"
        );
    }

    #[test]
    fn a_logpoint_that_produced_more_than_is_kept_says_how_many_are_missing() {
        let mut said = Said::default();
        for hit in 0..(LOGS_KEPT as u64 + 7) {
            said.logged(LogRecord {
                breakpoint: 1,
                file: "/tmp/a.py".to_string(),
                line: 2,
                thread: 3,
                hit,
                message: "x".to_string(),
            });
        }

        let taken = said.take().expect("it logged something");
        assert_eq!(
            taken["records"]
                .as_array()
                .expect("records are an array")
                .len(),
            LOGS_KEPT
        );
        assert_eq!(taken["dropped"], 7);
    }

    #[test]
    fn an_argument_a_tool_does_not_name_is_refused_rather_than_defaulted() {
        // `deadlineMs` instead of `deadline_ms` would otherwise be a call with
        // no deadline at all, which is the one thing this interface promises
        // cannot happen
        let refused = parse::<Waiting>(&serde_json::json!({ "deadlineMs": 100 }))
            .expect_err("`deadlineMs` is not an argument of this tool");
        assert!(refused.contains("deadlineMs"), "said {refused}");

        let taken: Waiting =
            parse(&serde_json::json!({ "deadline_ms": 100 })).expect("that is what it is called");
        assert_eq!(taken.deadline(), Duration::from_millis(100));
        assert_eq!(taken.frames, FRAMES_BY_DEFAULT);
    }
}
