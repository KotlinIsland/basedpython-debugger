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
    Addressed, Detail, Exit, Forwarded, HitCondition, LogRecord, Reporting, Request, Response,
    Running, Scope, SessionId, SourceBreakpoint, StepKind, Stop, Threads, Which, exit_code,
    only_stop,
};

use crate::prompts::prompts;
use crate::render;
use crate::resources::resources;
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

/// how many child processes are kept between calls
///
/// a program that starts children in a loop has no bound on this either, and
/// far fewer are worth reading than log records: they say the same thing about
/// the same program. whatever falls off is counted and reported
const CHILDREN_KEPT: usize = 50;

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
/// `Refused` is for the protocol going wrong: a method that does not exist, a
/// `tools/call` with no `name`, a tool nobody offers
///
/// arguments that are not the shape the schema says are deliberately **not**
/// here. they are the client's own mistake and the model is the one that has to
/// correct it, so they are answered as a tool failure that the model is certain
/// to see rather than down a channel a host is entitled to hide from it
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
            "resources/list" => Ok(serde_json::json!({
                "resources": resources()
                    .iter()
                    .map(crate::resources::Resource::listing)
                    .collect::<Vec<_>>(),
            })),
            "resources/read" => read_resource(params),
            "prompts/list" => Ok(serde_json::json!({
                "prompts": prompts()
                    .iter()
                    .map(crate::prompts::Prompt::listing)
                    .collect::<Vec<_>>(),
            })),
            "prompts/get" => get_prompt(params),
            other => Err(Refused {
                code: code::METHOD_NOT_FOUND,
                reason: format!(
                    "bpd's MCP server does not implement `{other}`. it offers \
                     `initialize`, `ping`, `tools/list`, `tools/call`, \
                     `resources/list`, `resources/read`, `prompts/list` and \
                     `prompts/get`"
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
                let args: Launch = parse(name, arguments)?;
                self.launch(args)
            }
            "set_breakpoints" => {
                let args: SetBreakpoints = parse(name, arguments)?;
                self.set_breakpoints(args)
            }
            "set_exception_breakpoints" => {
                let args: ExceptionArgs = parse(name, arguments)?;
                match self.ask_in(
                    args.session,
                    Request::SetExceptionBreakpoints {
                        raised: args.raised,
                        uncaught: args.uncaught,
                    },
                )? {
                    Response::ExceptionBreakpoints(armed) => Ok(serde_json::json!({
                        // read back off the agent rather than echoed from the
                        // request: what is armed is what the agent says is armed
                        "raised": armed.raised,
                        "uncaught": armed.uncaught,
                    })),
                    other => unreachable!("exception breakpoints were answered with {other:?}"),
                }
            }
            "debug_children" => {
                let args: DebugChildrenArgs = parse(name, arguments)?;
                match self.ask_in(args.session, Request::DebugChildren { on: args.on })? {
                    Response::DebuggingChildren { on } => Ok(serde_json::json!({
                        // read back off the agent rather than echoed from the
                        // request: what is set is what the process that will
                        // fork says is set
                        "debugging_children": on,
                        "note": if on {
                            "a child of this program will open a session of its \
                             own and arrive **held** — a fork at the line that \
                             forked, and one that was `exec`'d at its own \
                             interpreter startup, before its program has been \
                             compiled. it is a second session: `sessions` lists \
                             it, and every tool takes its `session`. it has to \
                             be resumed like any other stop"
                        } else {
                            "a child of this program runs undebugged: a forked \
                             one gives the session up before `os.fork()` \
                             returns, and one that was `exec`'d is never \
                             reached. the child is still reported"
                        },
                    })),
                    other => unreachable!("debugging children was answered with {other:?}"),
                }
            }
            "sessions" => {
                let _: Empty = parse(name, arguments)?;
                if self.session.is_none() {
                    return Err(NO_PROGRAM.to_string());
                }
                Ok(serde_json::json!({ "sessions": render::sessions(&self.joined()) }))
            }
            "continue_" => {
                let args: Waiting = parse(name, arguments)?;
                let ran = self.ran_in(
                    args.session,
                    Request::Run {
                        deadline: Some(args.deadline()),
                    },
                )?;
                self.outcome(ran, args.session, args.frames)
            }
            "step_over" => self.step(name, arguments, StepKind::Over),
            "step_in" => self.step(name, arguments, StepKind::In),
            "step_out" => self.step(name, arguments, StepKind::Out),
            "wait" => {
                let args: Waiting = parse(name, arguments)?;
                let ran = self.ran_in(
                    args.session,
                    Request::Wait {
                        deadline: Some(args.deadline()),
                    },
                )?;
                self.outcome(ran, args.session, args.frames)
            }
            "pause" => {
                let args: Waiting = parse(name, arguments)?;
                let running = match self.ask_in(args.session, Request::Pause)? {
                    Response::Pausing { running } => running,
                    other => unreachable!("a pause was answered with {other:?}"),
                };
                let ran = self.ran_in(
                    args.session,
                    Request::Wait {
                        deadline: Some(args.deadline()),
                    },
                )?;
                let mut answer = self.outcome(ran, args.session, args.frames)?;
                answer["running"] = serde_json::json!(running);
                if running.is_empty() {
                    // `running` is what the agent saw *excluding* what it is
                    // holding, so an empty one has two causes and naming the
                    // wrong one would tell a client its program is stuck in
                    // native code when bpd is what is holding it still
                    let held = self.held_stops();
                    answer["note"] = if held.is_empty() {
                        "the pause is armed and no thread was running python \
                         when it went on. bpd is holding nothing either, so \
                         every thread of the program is parked in a C call, \
                         where there is no monitoring event to hold one at — \
                         nothing will arrive until one of them comes back into \
                         python"
                            .into()
                    } else {
                        format!(
                            "the pause is armed and no thread that bpd is not \
                             already holding was running python. it is holding \
                             {}, and a held thread reaches no line until it is \
                             resumed — so `resume` one of them, or wait for a \
                             thread parked in a C call to come back into python",
                            held.len()
                        )
                        .into()
                    };
                }
                Ok(answer)
            }
            "resume" => {
                let args: Resume = parse(name, arguments)?;
                let which = match args.threads {
                    Some(threads) => Which::Named { threads },
                    None => Which::All,
                };
                match self.ask_in(args.session, Request::Resume { which })? {
                    Response::Resumed { threads } => Ok(serde_json::json!({
                        "resumed": threads,
                        "held": self.held_now(),
                    })),
                    other => unreachable!("a resume was answered with {other:?}"),
                }
            }
            "stack" => {
                let args: StackArgs = parse(name, arguments)?;
                let stop = self.stop_of(args.stop, args.session, "the stack")?;
                match self.ask_in(
                    args.session,
                    Request::Stack {
                        stop,
                        top: args.top,
                    },
                )? {
                    Response::Stack(walked) => Ok(render::stack(&walked)),
                    other => unreachable!("a stack walk was answered with {other:?}"),
                }
            }
            "variables" => {
                let args: VariablesArgs = parse(name, arguments)?;
                let frame = self.frame_of(
                    args.stop,
                    args.session,
                    args.frame,
                    "the variables of a scope",
                )?;
                match self.ask_in(
                    args.session,
                    Request::Variables {
                        frame,
                        scope: args.scope,
                        detail: args.detail,
                    },
                )? {
                    Response::Variables(read) => Ok(render::variables(&read)),
                    other => unreachable!("a scope read was answered with {other:?}"),
                }
            }
            "facts" => {
                let args: FactsArgs = parse(name, arguments)?;
                let frame = self.frame_of(
                    args.stop,
                    args.session,
                    args.frame,
                    "what is provable about a frame's names",
                )?;
                match self.ask_in(
                    args.session,
                    Request::Facts {
                        frame,
                        names: args.names,
                        limit: args.limit,
                    },
                )? {
                    Response::Facts(facts) => Ok(render::facts(&facts)),
                    other => unreachable!("a fact request was answered with {other:?}"),
                }
            }
            "template_context" => {
                let args: TemplateContextArgs = parse(name, arguments)?;
                let frame = self.frame_of(
                    args.stop,
                    args.session,
                    args.frame,
                    "the template context of a frame",
                )?;
                match self.ask_in(
                    args.session,
                    Request::TemplateContext {
                        frame,
                        detail: args.detail,
                    },
                )? {
                    Response::TemplateContext(context) => Ok(render::template_context(&context)),
                    other => unreachable!("a template context was answered with {other:?}"),
                }
            }
            "evaluate" => {
                let args: EvaluateArgs = parse(name, arguments)?;
                let frame = self.frame_of(
                    args.stop,
                    args.session,
                    args.frame,
                    "evaluating an expression",
                )?;
                match self.ask_in(
                    args.session,
                    Request::Evaluate {
                        frame,
                        expression: args.expression,
                        detail: args.detail,
                    },
                )? {
                    Response::Evaluated(result) => Ok(render::evaluated(&result)),
                    other => unreachable!("an evaluation was answered with {other:?}"),
                }
            }
            "set_variable" => {
                let args: SetVariableArgs = parse(name, arguments)?;
                let frame =
                    self.frame_of(args.stop, args.session, args.frame, "writing a variable")?;
                match self.ask_in(
                    args.session,
                    Request::SetVariable {
                        frame,
                        scope: args.scope,
                        name: args.name,
                        value: args.value,
                        detail: args.detail,
                    },
                )? {
                    Response::Evaluated(result) => Ok(render::evaluated(&result)),
                    other => unreachable!("a variable write was answered with {other:?}"),
                }
            }
            "set_next_statement" => {
                let args: SetNextStatementArgs = parse(name, arguments)?;
                let frame = self.frame_of(
                    args.stop,
                    args.session,
                    args.frame,
                    "setting the next statement",
                )?;
                match self.ask_in(
                    args.session,
                    Request::SetNextStatement {
                        frame,
                        line: args.line,
                    },
                )? {
                    Response::Jumped(jumped) => Ok(render::jumped(&jumped)),
                    other => unreachable!("a jump was answered with {other:?}"),
                }
            }
            "restart_frame" => {
                let args: RestartFrameArgs = parse(name, arguments)?;
                let frame =
                    self.frame_of(args.stop, args.session, args.frame, "restarting a frame")?;
                match self.ask_in(
                    args.session,
                    Request::RestartFrame {
                        frame,
                        again: args.again,
                    },
                )? {
                    Response::Restarted(restarted) => Ok(render::restarted(&restarted)),
                    other => unreachable!("a restart was answered with {other:?}"),
                }
            }
            "record" => {
                let args: RecordArgs = parse(name, arguments)?;
                match self.ask_in(
                    args.session,
                    Request::Record {
                        on: args.on,
                        depth: args.depth,
                    },
                )? {
                    Response::Recording { on, held, dropped } => {
                        Ok(render::recording(on, held, dropped))
                    }
                    other => unreachable!("a recording was answered with {other:?}"),
                }
            }
            "trail" => {
                let args: SessionOnly = parse(name, arguments)?;
                match self.ask_in(args.session, Request::Trail)? {
                    Response::Trail(trail) => Ok(render::trail(&trail)),
                    other => unreachable!("a trail was answered with {other:?}"),
                }
            }
            "retainers" => {
                let args: RetainersArgs = parse(name, arguments)?;
                let frame = self.frame_of(
                    args.stop,
                    args.session,
                    args.frame,
                    "what is holding an object",
                )?;
                match self.ask_in(
                    args.session,
                    Request::Retainers {
                        frame,
                        expression: args.expression,
                    },
                )? {
                    Response::Retainers(retainers) => Ok(render::retainers(&retainers)),
                    other => unreachable!("a retainer walk was answered with {other:?}"),
                }
            }
            "replace_code" => {
                let args: ReplaceCodeArgs = parse(name, arguments)?;
                if args.files().is_empty() {
                    // an empty set asks for nothing to change, which is not
                    // something a replacement can answer. said here rather than
                    // left to the engine, because the engine would be answering
                    // about a process nobody asked it about
                    return Err(
                        "`replace_code` needs `file` or `files` — the path of a file \
                                whose code to replace, on the debuggee's own filesystem. an \
                                empty set asks for nothing to change, which is not something a \
                                replacement can answer"
                            .to_string(),
                    );
                }
                match self.ask_in(
                    args.session,
                    Request::ReplaceCode {
                        files: args.files(),
                        remap: args.remap,
                        even_under_a_live_frame: args.even_under_a_live_frame,
                    },
                )? {
                    Response::Replaced(replaced) => Ok(render::replaced(&replaced)),
                    other => unreachable!("a code replacement was answered with {other:?}"),
                }
            }
            "threads" => {
                let args: ThreadsArgs = parse(name, arguments)?;
                match self.ask_in(
                    args.session,
                    Request::Threads {
                        settle: args.settle(),
                    },
                )? {
                    Response::Threads(census) => Ok(render::threads(&census)),
                    other => unreachable!("a thread census was answered with {other:?}"),
                }
            }
            "stop_the_world" => {
                let args: WorldArgs = parse(name, arguments)?;
                let stop = self.stop_of(args.stop, args.session, "stopping the world")?;
                match self.ask_in(
                    args.session,
                    Request::StopTheWorld {
                        stop,
                        settle: args.settle(),
                    },
                )? {
                    Response::WorldStopped(stopped) => Ok(render::world(&stopped)),
                    other => unreachable!("stopping the world was answered with {other:?}"),
                }
            }
            "run_script" => {
                let args: RunScript = parse(name, arguments)?;
                let stop = self.stop_of(args.stop, args.session, "running a debug script")?;
                match self.ask_in(
                    args.session,
                    Request::RunScript {
                        stop,
                        script: bpd_core::Script {
                            steps: args.steps,
                            budget: args.budget,
                        },
                    },
                )? {
                    Response::Transcript(ran) => Ok(render::transcript(&ran)),
                    other => unreachable!("a debug script was answered with {other:?}"),
                }
            }
            "state" => {
                let args: StateArgs = parse(name, arguments)?;
                let session = args.session;
                let stop = self.stop_of(args.stop, session, "the state of a stop")?;
                match self.ask_in(
                    session,
                    Request::Query {
                        stop,
                        query: args.query(),
                    },
                )? {
                    Response::State(snapshot) => Ok(render::state(&snapshot)),
                    other => unreachable!("a state query was answered with {other:?}"),
                }
            }
            "diff" => {
                let args: DiffArgs = parse(name, arguments)?;
                match self.ask_in(
                    args.session,
                    Request::Diff {
                        before: args.before,
                        after: args.after,
                    },
                )? {
                    Response::Difference(difference) => Ok(render::difference(&difference)),
                    other => unreachable!("a difference was answered with {other:?}"),
                }
            }
            "terminate" => {
                let args: SessionOnly = parse(name, arguments)?;
                let named = self.session_named(args.session, "ending the program")?;
                let last = self.session_ids().len() <= 1;
                let held = self
                    .session
                    .as_mut()
                    .ok_or_else(|| NO_PROGRAM.to_string())?;
                held.terminate(named)
                    .map_err(|error| describe(error.as_ref()))?;
                // the debuggee goes when its last session does. ending one of
                // several leaves the others, and a server that forgot the whole
                // debuggee would strand every process it still holds
                if last {
                    self.session = None;
                }
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
                self.with_frames(&mut answer, None, entry.stop, args.frames)?;
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
        let session = args.session;
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
                    after: wanted.after,
                }
            })
            .collect();
        self.requested.clone_from(&breakpoints);

        match self.ask_in(
            session,
            Request::SetBreakpoints {
                breakpoints: breakpoints.clone(),
            },
        )? {
            Response::BreakpointsResolved { resolved } => Ok(serde_json::json!({
                "breakpoints": render::breakpoints(&resolved, &breakpoints),
            })),
            other => unreachable!("a breakpoint set was answered with {other:?}"),
        }
    }

    fn step(&mut self, name: &str, arguments: &serde_json::Value, kind: StepKind) -> Answered {
        let args: Stepping = parse(name, arguments)?;
        let stop = self.stop_of(args.stop, args.session, "stepping a thread")?;
        match self.ask_in(args.session, Request::Step { stop, kind })? {
            Response::Resumed { .. } => {}
            other => unreachable!("a step was answered with {other:?}"),
        }
        let ran = self.ran_in(
            args.session,
            Request::Wait {
                deadline: Some(args.deadline()),
            },
        )?;
        self.outcome(ran, args.session, args.frames)
    }

    // ---- what the program did --------------------------------------------

    fn ran_in(&mut self, session: Option<u64>, request: Request) -> Result<Running, String> {
        match self.ask_in(session, request)? {
            Response::Ran(running) => Ok(running),
            other => unreachable!("a wait was answered with {other:?}"),
        }
    }

    /// what a control tool answers: the stop it produced, or why there is none
    fn outcome(&mut self, running: Running, session: Option<u64>, frames: u32) -> Answered {
        let rebound = match &running {
            Running::Stopped { rebound, .. }
            | Running::Exited { rebound, .. }
            | Running::Ended { rebound }
            | Running::Finishing { rebound, .. }
            | Running::StillRunning { rebound, .. } => rebound.clone(),
        };

        let mut answer = match running {
            Running::Stopped { stop, .. } => {
                let mut stopped = render::stop(&stop);
                stopped["outcome"] = "stopped".into();
                self.with_frames(&mut stopped, session, stop.stop, frames)?;
                stopped
            }
            // `output_complete` is on every exit rather than only the unusual
            // one, because an agent reading a field that appears only when
            // something is wrong cannot tell "nothing was wrong" from "this
            // server does not report it". the note is what carries the reason,
            // and it is only there when there is one
            Running::Exited { status, output, .. } => {
                let mut exited = serde_json::json!({
                    "outcome": "exited",
                    "exit_code": exit_code(status),
                    "output_complete": output == Forwarded::Everything,
                });
                if output == Forwarded::StillHeldOpen {
                    // what bpd waited for and did not get, rather than the
                    // reason it did not get it. naming a cause bpd did not
                    // watch for would be the invention this field exists to
                    // avoid
                    exited["note"] = "the program has exited and its output is still being \
                                      written: bpd waited for the stream it wrote to and it \
                                      did not end, which is what a child outliving the \
                                      program looks like. output after this point was \
                                      written by whatever still holds that stream and not \
                                      by the program that just ended, so it cannot be read \
                                      as part of this run"
                        .into();
                }
                exited
            }
            // a separate outcome from `exited`, and deliberately **without** an
            // `exit_code` field rather than with a null one: the program is
            // over and the number is not bpd's to give
            Running::Ended { .. } => serde_json::json!({
                "outcome": "ended",
                "note": "the program is over and bpd cannot say what it exited \
                         with. bpd did not start that process — it connected to \
                         bpd's listener — so bpd is not its parent, cannot reap \
                         it and never learns its exit status. every stop it had \
                         has ended with it",
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
        session: Option<u64>,
        stop: u64,
        frames: u32,
    ) -> Result<(), String> {
        if frames == 0 {
            return Ok(());
        }
        match self.ask_in(
            session,
            Request::Stack {
                stop,
                top: Some(frames),
            },
        )? {
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
    ///
    /// the request is addressed before it is sent, and the rule for what to
    /// address it to is [`bpd_core::Addressed::of`] — in the core, because both
    /// front ends have to apply it. a tool that is about a stop goes to the
    /// session that stop was reported from, and one that is about the program
    /// names none, which is this server's only session
    /// ask one session for something, rendering a failure as a tool failure
    ///
    /// the address comes from two places and they are not equals. a request
    /// that is about a **stop** belongs to the session that stop was reported
    /// from, and the stop carries it — which is unforgeable, because the engine
    /// names a stop on the connection it arrived on. a `session` argument is
    /// what a request about the *program* has instead, and it is the only thing
    /// there is when there are two sessions and nothing else says which
    ///
    /// so the two are checked against each other rather than one overriding the
    /// other. an argument that disagrees with the stop is a caller that believes
    /// something false about which program it is looking at, and answering
    /// either way would confirm half of it
    fn ask_in(&mut self, named: Option<u64>, request: Request) -> Result<Response, String> {
        let wanted = request.name();
        let named = self.session_named(named, wanted)?;
        let asked = Addressed::of(request, &self.held_stops());
        let asked = match (asked.session, named) {
            (Some(by_stop), Some(named)) if by_stop != named => {
                return Err(format!(
                    "{wanted} names {named} and is about a stop that was \
                     reported from {by_stop}. a stop belongs to the session it \
                     arrived on and that is not something an argument can \
                     change — drop the `session`, or name the stop of {named}"
                ));
            }
            (Some(by_stop), _) => Addressed::to(by_stop, asked.request),
            (None, Some(named)) => Addressed::to(named, asked.request),
            (None, None) => asked,
        };
        let Self { session, said, .. } = self;
        let session = session.as_mut().ok_or_else(|| NO_PROGRAM.to_string())?;
        session
            .dispatch(asked, said)
            .map_err(|error| describe(error.as_ref()))
    }

    /// the session a tool named, refused when this debuggee holds no such one
    ///
    /// naming none is the ordinary case and stays `None`, which is what
    /// [`bpd_core::only_session`] answers against — refusing rather than picking
    /// when there is more than one. an id nothing holds is refused here rather
    /// than resolved to the nearest, for the reason that rule refuses one
    fn session_named(
        &self,
        given: Option<u64>,
        wanted: &'static str,
    ) -> Result<Option<SessionId>, String> {
        let Some(given) = given else {
            return Ok(None);
        };
        let open = self.session_ids();
        let named = std::num::NonZeroU64::new(given)
            .map(SessionId::new)
            .ok_or_else(|| {
                format!(
                    "session 0 is not a session — they are numbered from one. \
                     what is open: {open:?}"
                )
            })?;
        bpd_core::only_session(&open, Some(named), wanted)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    fn session_ids(&self) -> Vec<SessionId> {
        self.joined()
            .into_iter()
            .map(|joined| joined.session)
            .collect()
    }

    fn joined(&self) -> Vec<bpd_core::Joined> {
        self.session
            .as_ref()
            .map(|session| session.sessions())
            .unwrap_or_default()
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

    /// how one session's program ended, or `None` while there is one
    fn ended(&self, session: Option<SessionId>) -> Option<Exit> {
        self.session.as_ref().and_then(|held| held.ended(session))
    }

    /// the stop a tool is about, or the rule for why it cannot be decided
    ///
    /// naming a session narrows the stops it is decided among, which is the
    /// whole use of the argument on a tool that is about one thread: two
    /// sessions each holding one stop is two stops, and "the only one" is then
    /// a question about a program rather than about the debugger
    fn stop_of(
        &self,
        given: Option<u64>,
        session: Option<u64>,
        wanted: &'static str,
    ) -> Result<u64, String> {
        if let Some(stop) = given {
            return Ok(stop);
        }
        let named = self.session_named(session, wanted)?;
        let held: Vec<Stop> = match named {
            Some(named) => self
                .held_stops()
                .into_iter()
                .filter(|stop| stop.session == named)
                .collect(),
            None => self.held_stops(),
        };
        only_stop(&held, self.ended(named), wanted).map_err(|error| error.to_string())
    }

    fn frame_of(
        &self,
        stop: Option<u64>,
        session: Option<u64>,
        depth: u32,
        wanted: &'static str,
    ) -> Result<bpd_core::FrameId, String> {
        Ok(bpd_core::FrameId {
            stop: self.stop_of(stop, session, wanted)?,
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
        if let Some(children) = self.said.children() {
            answer["spawned"] = children;
        }
        if let Some(joined) = self.said.joined() {
            answer["attached"] = joined;
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
        let Some(held) = self.session.as_mut() else {
            return;
        };
        // every session, not the only one: a debugged fork is a second process
        // and one left running with nothing watching it is exactly the state
        // this exists to prevent. one bpd did not start refuses, which is not a
        // failure to report here — there is nobody left to report it to
        for joined in held.sessions() {
            let ended = held.terminate(Some(joined.session));
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
        // resources are pulled at the host's discretion and prompts are invoked
        // by the user, so neither is a surface an agent can be relied on to
        // see. they carry the deeper model and the canonical investigations,
        // and nothing that is only said there — an agent that never receives
        // one still has the tool schemas and the errors, which is where the
        // semantics live
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false, "listChanged": false },
            "prompts": { "listChanged": false },
        },
        "serverInfo": {
            "name": "bpd",
            "title": "bpd — a debugger for python",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// answer a `resources/read`
///
/// a uri nobody offers is a JSON-RPC failure rather than a document saying so:
/// the host chose the uri, the model never sees it, and a page of prose in place
/// of the page that was asked for is a thing an agent could read as true
fn read_resource(params: &serde_json::Value) -> Result<serde_json::Value, Refused> {
    let uri = params
        .get("uri")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Refused {
            code: code::INVALID_PARAMS,
            reason: "a `resources/read` arrived with no `uri`".to_string(),
        })?;

    let offered = resources();
    let found = offered
        .iter()
        .find(|resource| resource.uri == uri)
        .ok_or_else(|| Refused {
            code: code::RESOURCE_NOT_FOUND,
            reason: format!(
                "bpd offers no resource at `{uri}`. it offers: {:?}",
                offered
                    .iter()
                    .map(|resource| resource.uri)
                    .collect::<Vec<_>>()
            ),
        })?;

    Ok(serde_json::json!({ "contents": [found.contents()] }))
}

/// answer a `prompts/get`
fn get_prompt(params: &serde_json::Value) -> Result<serde_json::Value, Refused> {
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Refused {
            code: code::INVALID_PARAMS,
            reason: "a `prompts/get` arrived with no `name`".to_string(),
        })?;

    let offered = prompts();
    let found = offered
        .iter()
        .find(|prompt| prompt.name == name)
        .ok_or_else(|| Refused {
            code: code::INVALID_PARAMS,
            reason: format!(
                "bpd offers no prompt called `{name}`. it offers: {:?}",
                offered.iter().map(|prompt| prompt.name).collect::<Vec<_>>()
            ),
        })?;

    // an argument that is not a string is refused rather than rendered through
    // `Display`, which would put `true` or `1.0` into an investigation as though
    // someone had written it there
    let mut given = std::collections::BTreeMap::new();
    if let Some(arguments) = params.get("arguments")
        && !arguments.is_null()
    {
        let arguments = arguments.as_object().ok_or_else(|| Refused {
            code: code::INVALID_PARAMS,
            reason: "the `arguments` of a `prompts/get` are an object of strings".to_string(),
        })?;
        for (key, value) in arguments {
            let value = value.as_str().ok_or_else(|| Refused {
                code: code::INVALID_PARAMS,
                reason: format!("`{key}` is not a string, and a prompt argument is text"),
            })?;
            given.insert(key.clone(), value.to_string());
        }
    }

    found.filled(&given).map_err(|reason| Refused {
        code: code::INVALID_PARAMS,
        reason,
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
///
/// the tool is named in the refusal because an agent that made several calls in
/// one turn is otherwise told which *argument* was wrong without being told
/// which call it belonged to
fn parse<T: serde::de::DeserializeOwned>(
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<T, String> {
    serde_json::from_value(arguments.clone()).map_err(|error| {
        format!(
            "`{tool}` was not called with the arguments it takes: {error}. its \
             schema is in `tools/list`, it requires everything that schema \
             lists under `required`, and it accepts nothing the schema does not \
             name — a misspelled argument that quietly took its default would be \
             a setting asked for and never applied"
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
    children: Vec<bpd_core::Spawn>,
    children_dropped: usize,
    blind: Vec<bpd_core::Blindspot>,
    joined: Vec<SessionId>,
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

    /// the sessions that joined since the last answer, if any
    ///
    /// its own key, and the one an agent must not miss: a debugged child arrives
    /// **held**, so a session listed here is a process waiting for this agent to
    /// do something about it
    fn joined(&mut self) -> Option<serde_json::Value> {
        if self.joined.is_empty() {
            return None;
        }
        let sessions: Vec<u64> = std::mem::take(&mut self.joined)
            .iter()
            .map(|session| session.get())
            .collect();
        Some(serde_json::json!({
            "sessions": sessions,
            "note": "the program made a child and the child opened a debug \
                     session of its own. it is **held** — a fork at the line \
                     that forked, and one that was `exec`'d at its own startup, \
                     where it has no line and no stack because nothing of its \
                     program has run — and it stays there until something \
                     resumes it. `sessions` lists them all; every tool takes the \
                     `session` of one",
        }))
    }

    /// the children the program started since the last answer, if any
    ///
    /// its own key rather than part of the logs, because it is not something
    /// the program was asked to say. an agent that found it under `logged`
    /// would reasonably read it as a logpoint firing
    fn children(&mut self) -> Option<serde_json::Value> {
        if self.children.is_empty() && self.blind.is_empty() {
            return None;
        }
        let started: Vec<serde_json::Value> = self.children.iter().map(render::spawned).collect();
        self.children.clear();
        let dropped = std::mem::take(&mut self.children_dropped);

        let mut rendered = serde_json::json!({ "started": started });
        if !self.blind.is_empty() {
            // beside the children rather than instead of them: an agent that
            // read `started: []` without this would conclude there were none
            rendered["cannot_see"] = serde_json::json!(
                std::mem::take(&mut self.blind)
                    .iter()
                    .map(render::blind_to)
                    .collect::<Vec<_>>()
            );
        }
        if dropped > 0 {
            rendered["dropped"] = dropped.into();
            rendered["says"] = format!(
                "the program started more than the {CHILDREN_KEPT} children bpd \
                 keeps between calls, so {dropped} later ones are gone. these \
                 are the first"
            )
            .into();
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

    fn spawned(&mut self, child: bpd_core::Spawn) {
        if self.children.len() < CHILDREN_KEPT {
            self.children.push(child);
        } else {
            self.children_dropped += 1;
        }
    }

    /// this interpreter hides a whole way of starting a child
    ///
    /// unbounded, unlike the children beside it, because there is a fixed and
    /// very small number of blind spots and each is announced once per program.
    /// dropping this one to save room would be dropping the message that keeps
    /// the rest honest
    fn blind_to(&mut self, blindspot: bpd_core::Blindspot) {
        self.blind.push(blindspot);
    }

    /// a debugged fork opened a session of its own
    ///
    /// unbounded, for the reason a blind spot is and more so: every one of these
    /// is a **held** process. one that was dropped to save room would be a
    /// stopped program nothing was ever told about, which is the hang this
    /// whole feature is arranged to avoid
    fn attached(&mut self, session: SessionId) {
        self.joined.push(session);
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

/// a tool that takes nothing but the session it is about
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionOnly {
    #[serde(default)]
    session: Option<u64>,
}

/// what a forked child of the program should do
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DebugChildrenArgs {
    #[serde(default)]
    session: Option<u64>,
    on: bool,
}

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
    #[serde(default)]
    session: Option<u64>,
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
    /// the breakpoint that has to be hit before this one is armed
    ///
    /// **the position of one in this same list, counting from 1.** an agent
    /// does not choose breakpoint ids here — the server numbers them by where
    /// they appear — so the thing to name is where the earlier one is
    #[serde(default)]
    after: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExceptionArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    raised: bool,
    #[serde(default)]
    uncaught: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Waiting {
    #[serde(default)]
    session: Option<u64>,
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
    #[serde(default)]
    session: Option<u64>,
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
    session: Option<u64>,
    #[serde(default)]
    threads: Option<Vec<u64>>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StackArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    top: Option<u32>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VariablesArgs {
    #[serde(default)]
    session: Option<u64>,
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
struct FactsArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    names: Vec<String>,
    #[serde(default)]
    limit: bpd_core::Limit,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateContextArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    #[serde(default)]
    detail: Detail,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateArgs {
    #[serde(default)]
    session: Option<u64>,
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
    session: Option<u64>,
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetNextStatementArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    line: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartFrameArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    /// which of the two ways to run the frame again, defaulting to either
    #[serde(default)]
    again: bpd_core::Again,
}

/// which file's code to replace
///
/// no stop and no frame: a replacement is about the process rather than about
/// one held thread, and it names the file the same way a breakpoint does
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArgs {
    #[serde(default)]
    session: Option<u64>,
    /// whether to record
    on: bool,
    /// how much of each step to keep
    ///
    /// absent is the cheap one. the depths differ by hundreds of times a bare
    /// run, and an agent that said nothing has not asked to pay for the
    /// expensive one
    #[serde(default)]
    depth: bpd_core::Depth,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainersArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default)]
    frame: u32,
    expression: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceCodeArgs {
    #[serde(default)]
    session: Option<u64>,
    /// the one file whose code to replace
    ///
    /// kept beside `files` because one file is what this is nearly always about,
    /// and an agent naming a single path should not have to wrap it in a list
    #[serde(default)]
    file: Option<PathBuf>,
    /// several files, replaced together or not at all
    ///
    /// what staging a basedpython build again produces: one edit can change the
    /// python emitted for more than one module, and applying some of them would
    /// leave the process half way between two versions of the build
    #[serde(default)]
    files: Vec<PathBuf>,
    /// read `_by_sourcemap.py` again before replacing anything
    ///
    /// for a basedpython build whose tree was just staged again: the map beside
    /// the generated python was rewritten too, so the generated lines every `.by`
    /// breakpoint is armed on came out of a table that no longer describes the
    /// tree. off for a program that is not one, which has no map to read
    #[serde(default)]
    remap: bool,
    /// apply it even where a frame is running the code being replaced
    ///
    /// defaults to off, and that default is the guarantee rather than a
    /// convenience: with it on the process runs two versions of one function
    /// until those frames return, and an agent that had not asked for that must
    /// not be handed it
    #[serde(default)]
    even_under_a_live_frame: bool,
}

impl ReplaceCodeArgs {
    /// the files to replace, however they were named
    ///
    /// both spellings at once is not refused: they are one set, and an agent that
    /// sent a file twice meant it once.
    fn files(&self) -> Vec<PathBuf> {
        let mut files = self.files.clone();
        if let Some(file) = &self.file
            && !files.contains(file)
        {
            files.insert(0, file.clone());
        }
        files
    }
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
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    steps: Vec<bpd_core::Step>,
    budget: bpd_core::Budget,
}

/// a whole state query, as its own arguments
///
/// flat rather than a nested `query` object, because every field of it is a
/// thing the agent is choosing and a level of nesting is a level of schema an
/// agent has to hold in its head. what it builds is `bpd_core::StateQuery`, so
/// there is one definition of what a query is
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StateArgs {
    #[serde(default)]
    session: Option<u64>,
    #[serde(default)]
    stop: Option<u64>,
    #[serde(default = "one_frame")]
    frames: u32,
    #[serde(default)]
    scopes: Vec<Scope>,
    #[serde(default)]
    expressions: Vec<bpd_core::Wanted>,
    #[serde(default)]
    source: Option<u32>,
    #[serde(default)]
    detail: Detail,
}

/// how many frames a state query describes when the client does not say
fn one_frame() -> u32 {
    bpd_core::StateQuery::default().frames
}

impl StateArgs {
    fn query(self) -> bpd_core::StateQuery {
        bpd_core::StateQuery {
            frames: self.frames,
            scopes: self.scopes,
            expressions: self.expressions,
            source: self.source,
            detail: self.detail,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    #[serde(default)]
    session: Option<u64>,
    before: bpd_core::SnapshotId,
    after: bpd_core::SnapshotId,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadsArgs {
    #[serde(default)]
    session: Option<u64>,
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
    session: Option<u64>,
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
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn a_client_asking_for_a_version_this_server_knows_is_answered_in_it() {
        let known = initialize(&serde_json::json!({ "protocolVersion": "2024-11-05" }));
        assert_eq!(known["protocolVersion"], "2024-11-05");

        // and one it has never heard of is answered with what this server
        // speaks, rather than agreed to
        let unknown = initialize(&serde_json::json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(unknown["protocolVersion"], PROTOCOL_VERSION);
        // a capability is declared because it is implemented, and for no other
        // reason. an agent that asked for something advertised and got a
        // "method not found" would be reading a manifest that is a lie
        let mut nothing = Nothing;
        let mut server = Server::new(&mut nothing);
        for (capability, method) in [
            ("tools", "tools/list"),
            ("resources", "resources/list"),
            ("prompts", "prompts/list"),
        ] {
            assert!(
                unknown["capabilities"][capability].is_object(),
                "`{capability}` is implemented and not declared"
            );
            assert!(
                server.answer(method, &serde_json::json!({})).is_ok(),
                "`{capability}` is declared and `{method}` is not answered"
            );
        }
    }

    /// a launcher that is never asked to launch anything
    struct Nothing;

    impl Launcher for Nothing {
        fn launch(
            &mut self,
            configuration: &Configuration,
            _output: Arc<dyn ProgramOutput>,
        ) -> Result<Started, crate::session::Failed> {
            panic!("nothing here launches a program, and one asked for {configuration:?}")
        }
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
        let refused = parse::<Waiting>("continue_", &serde_json::json!({ "deadlineMs": 100 }))
            .expect_err("`deadlineMs` is not an argument of this tool");
        assert!(refused.contains("deadlineMs"), "said {refused}");
        assert!(
            refused.contains("`continue_`"),
            "an agent that called several tools has to be told which one this \
             was about, and it said {refused}"
        );

        let taken: Waiting = parse("continue_", &serde_json::json!({ "deadline_ms": 100 }))
            .expect("that is what it is called");
        assert_eq!(taken.deadline(), Duration::from_millis(100));
        assert_eq!(taken.frames, FRAMES_BY_DEFAULT);
    }

    /// the arguments one tool really parses, asked of serde rather than listed
    ///
    /// the same trick `crates/bpd_dap/tests/vscode.rs` uses on the vs code
    /// manifest, for the same seam: a schema and a struct are two descriptions
    /// of one thing, and nothing makes them agree
    macro_rules! parsed_by {
        ($($tool:literal => $args:ty),* $(,)?) => {
            fn arguments_of(tool: &str) -> BTreeSet<String> {
                match tool {
                    $($tool => fields_of::<$args>(),)*
                    other => panic!(
                        "`{other}` is a tool this server offers and nothing here \
                         says what it parses, so nothing checks that its schema \
                         and its arguments agree"
                    ),
                }
            }
        };
    }

    parsed_by! {
        "launch" => Launch,
        "set_breakpoints" => SetBreakpoints,
        "set_exception_breakpoints" => ExceptionArgs,
        "debug_children" => DebugChildrenArgs,
        "sessions" => Empty,
        "continue_" => Waiting,
        "step_over" => Stepping,
        "step_in" => Stepping,
        "step_out" => Stepping,
        "wait" => Waiting,
        "pause" => Waiting,
        "resume" => Resume,
        "stack" => StackArgs,
        "variables" => VariablesArgs,
        "facts" => FactsArgs,
        "template_context" => TemplateContextArgs,
        "evaluate" => EvaluateArgs,
        "set_variable" => SetVariableArgs,
        "set_next_statement" => SetNextStatementArgs,
        "restart_frame" => RestartFrameArgs,
        "record" => RecordArgs,
        "trail" => SessionOnly,
        "retainers" => RetainersArgs,
        "replace_code" => ReplaceCodeArgs,
        "threads" => ThreadsArgs,
        "stop_the_world" => WorldArgs,
        "run_script" => RunScript,
        "state" => StateArgs,
        "diff" => DiffArgs,
        "terminate" => SessionOnly,
    }

    #[test]
    fn every_tools_schema_names_exactly_the_arguments_it_parses() {
        // this is the seam the whole "tool schemas are the teaching surface"
        // design rests on. an argument the struct reads and the schema omits is
        // a capability no agent can find — `launch`'s `frames` was exactly that
        // — and one the schema names and the struct does not read is a setting
        // asked for and never applied
        for tool in tools() {
            let declared: BTreeSet<String> = tool.schema["properties"]
                .as_object()
                .expect("a tool schema declares its properties")
                .keys()
                .cloned()
                .collect();
            assert_eq!(
                declared,
                arguments_of(tool.name),
                "`{}`'s schema and its arguments disagree",
                tool.name
            );
        }
    }

    #[test]
    fn the_bounds_on_a_value_are_offered_exactly_as_the_core_reads_them() {
        // `detail` is nested inside four schemas rather than being one of them,
        // so the check above never reaches it
        let detail = crate::tools::detail();
        let declared: BTreeSet<String> = detail["properties"]
            .as_object()
            .expect("the detail schema declares its properties")
            .keys()
            .cloned()
            .collect();
        assert_eq!(declared, fields_of::<Detail>());
    }

    #[test]
    fn a_breakpoint_is_offered_exactly_as_the_server_reads_one() {
        // the breakpoint object is nested inside `set_breakpoints` rather than
        // being a tool's own arguments, so the check above never reaches it —
        // the same hole `detail` has its own test for. measured: with `after`
        // taken out of the schema and left in the struct, every other test in
        // this file still passed
        let breakpoints = tools()
            .into_iter()
            .find(|tool| tool.name == "set_breakpoints")
            .expect("`set_breakpoints` is a tool");
        let declared: BTreeSet<String> =
            breakpoints.schema["properties"]["breakpoints"]["items"]["properties"]
                .as_object()
                .expect("the breakpoint schema declares its properties")
                .keys()
                .cloned()
                .collect();
        assert_eq!(
            declared,
            fields_of::<Wanted>(),
            "a breakpoint field the server reads and the schema omits is one no \
             agent can find, and one the schema names and the server does not \
             read is a setting asked for and never applied"
        );
    }

    /// the field names a `Deserialize` implementation reads
    ///
    /// asked of serde rather than written down, so a struct that gains a field
    /// gains an entry here without anyone remembering to add one
    fn fields_of<'de, T: serde::Deserialize<'de>>() -> BTreeSet<String> {
        let mut found = Vec::new();
        let Err(error) = T::deserialize(Fields { found: &mut found }) else {
            panic!("this deserializer answers a struct with an error, always")
        };
        assert_eq!(
            error.to_string(),
            CAPTURED,
            "the field list was never asked for, so `{}` is not a struct serde reads by name",
            std::any::type_name::<T>()
        );
        found.into_iter().collect()
    }

    /// what the capturing deserializer says once it has the field list
    const CAPTURED: &str = "the field list is the whole of what this wanted";

    /// a deserializer that answers nothing and records the field list it is
    /// offered
    struct Fields<'a> {
        found: &'a mut Vec<String>,
    }

    impl<'de> serde::Deserializer<'de> for Fields<'_> {
        type Error = serde::de::value::Error;

        fn deserialize_struct<V: serde::de::Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            self.found
                .extend(fields.iter().map(|field| (*field).to_owned()));
            Err(serde::de::Error::custom(CAPTURED))
        }

        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _visitor: V,
        ) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom(
                "this deserializer only answers structs, and was asked for something else",
            ))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }
}
