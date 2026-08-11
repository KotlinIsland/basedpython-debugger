//! the adapter: DAP messages in, [`Request`]s out, events back
//!
//! ## what it does not do
//!
//! it makes no decision about the program. every question a client asks becomes
//! a [`Request`], and the answer is rendered rather than interpreted — where a
//! breakpoint bound, why one did not, what a value was cut by. the two things
//! that look like decisions are not:
//!
//! - **the breakpoint set is reassembled.** DAP replaces the breakpoints of one
//!   *file*; [`Request::SetBreakpoints`] replaces the whole set, because a
//!   debugger that accumulates edits has two ideas of what is set. so the
//!   adapter keeps what each file last asked for and sends the union. it is
//!   bookkeeping about what the client said, not about the program
//! - **a reference is a handle.** DAP needs an opaque number where bpd has a
//!   [`bpd_core::FrameId`] that carries its stop. [`crate::handles`] is that
//!   table, and it is what keeps the staleness detection that DAP's model
//!   throws away
//!
//! ## the two threads, and why there are two
//!
//! everything the agent answers, it answers on a thread it is holding. so while
//! the program is running the session is blocked reading its connection, and
//! the one thing a client may reasonably ask then — stop it, or end it — cannot
//! go down that path. a reader thread owns the client's input and an
//! [`Interrupt`]; the main thread owns the session. `pause`, `disconnect` and
//! `terminate` are answered by the reader; everything else is queued and
//! answered when the program next stops
//!
//! a request that arrives while the program is running therefore waits for the
//! next stop. that is the model rather than a shortcut: the agent cannot bind a
//! breakpoint or read a frame without a python thread to do it on

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use bpd_core::{
    Binding, Detail, Evaluated, FrameId, LogRecord, Reporting, Request, Resolved, Response,
    Running, Scope, SourceBreakpoint, StepKind, Stop, StopReason, Unbound, Value, Which, exit_code,
};

use crate::capabilities::capabilities;
use crate::configuration::Configuration;
use crate::handles::{Handle, Handles, Step};
use crate::render::{expandable, summary};
use crate::session::{
    Failed, Interrupt, Launcher, ProgramOutput, Session, Started, Stream, describe,
};
use crate::wire::{Incoming, Reader, Writer};

/// the shared writing end of the client connection
type Output = Arc<Mutex<Writer<Box<dyn Write + Send>>>>;

/// the handle the reader thread uses to reach a running program
type Held = Arc<Mutex<Option<Box<dyn Interrupt>>>>;

/// serve one DAP client over `input` and `output`
///
/// returns when the client hangs up or disconnects. the debuggee never outlives
/// it: a client that vanishes leaves a program running with nothing watching
/// it, which is the state the agent itself refuses to be in
pub fn serve(
    launcher: &mut dyn Launcher,
    input: Box<dyn Read + Send>,
    output: Box<dyn Write + Send>,
) -> Result<(), crate::wire::Error> {
    let output: Output = Arc::new(Mutex::new(Writer::new(output)));
    let interrupt: Held = Arc::new(Mutex::new(None));
    let stopping = Arc::new(AtomicBool::new(false));
    let (queue, commands) = channel();

    let reader = std::thread::Builder::new()
        .name("bpd-dap-client".to_string())
        .spawn({
            let output = Arc::clone(&output);
            let interrupt = Arc::clone(&interrupt);
            let stopping = Arc::clone(&stopping);
            move || read_client(Reader::new(input), &output, &interrupt, &queue, &stopping)
        })
        .map_err(|source| crate::wire::Error::Connection { source })?;

    let mut adapter = Adapter::new(output, interrupt, stopping);
    let served = adapter.run(launcher, &commands);

    // the reader owns the client's input and ends when the client hangs up.
    // joining it means the answer to a `disconnect` is written before this
    // process goes away
    let read = reader.join();
    served?;
    match read {
        Ok(read) => read,
        Err(panicked) => std::panic::resume_unwind(panicked),
    }
}

/// read the client, answering the two things a running program can be asked
fn read_client(
    mut reader: Reader<Box<dyn Read + Send>>,
    output: &Output,
    interrupt: &Held,
    queue: &Sender<Incoming>,
    stopping: &AtomicBool,
) -> Result<(), crate::wire::Error> {
    loop {
        let message = match reader.next_message() {
            Ok(Some(message)) => message,
            Ok(None) => {
                // the client vanished without saying so. the debuggee is not
                // left running with nothing watching it
                stopping.store(true, Ordering::Relaxed);
                end_debuggee(interrupt);
                return Ok(());
            }
            Err(error) => {
                stopping.store(true, Ordering::Relaxed);
                end_debuggee(interrupt);
                return Err(error);
            }
        };

        match message.command.as_deref() {
            Some("pause") => {
                let delivered = match interrupt.lock().expect(REACHING).as_mut() {
                    Some(reaching) => reaching
                        .deliver(&Request::Pause)
                        .map_err(|error| describe(error.as_ref())),
                    None => Err("no program is running yet, so there is no thread to hold. \
                                 a pause is answered by whichever thread reaches a line first"
                        .to_string()),
                };
                let mut writer = output.lock().expect(WRITING);
                match delivered {
                    Ok(()) => writer.respond(&message, None)?,
                    Err(reason) => writer.refuse(&message, &reason)?,
                }
            }

            Some("disconnect" | "terminate") => {
                stopping.store(true, Ordering::Relaxed);
                end_debuggee(interrupt);
                let mut writer = output.lock().expect(WRITING);
                writer.respond(&message, None)?;
                writer.event("terminated", &serde_json::json!({}))?;
                return Ok(());
            }

            _ => {
                if queue.send(message).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// end the debuggee, if one was ever started
///
/// a failure here is not reportable: the client is going away, and the only
/// thing left that could be told is the process that has just been asked to
/// stop existing
fn end_debuggee(interrupt: &Held) {
    if let Some(reaching) = interrupt.lock().expect(REACHING).as_mut() {
        let ended = reaching.terminate();
        drop(ended);
    }
}

const WRITING: &str =
    "nothing panics holding the client's output: every path through it is a write";
const REACHING: &str =
    "nothing panics holding the interrupt: every path through it is a send or a kill";

/// how the DAP adapter answers one client
struct Adapter {
    output: Output,
    interrupt: Held,
    stopping: Arc<AtomicBool>,
    session: Option<Box<dyn Session>>,
    configuration: Option<Configuration>,
    /// whether the client has finished sending its configuration
    ///
    /// nothing is announced before it: a `stopped` event for the entry stop
    /// sent while the client is still setting breakpoints is a stop it is not
    /// ready to render
    configured: bool,
    exited: bool,
    handles: Handles,
    threads: ThreadIds,
    /// the stops the client has already been told about
    announced: Vec<Stop>,
    /// why each held thread stopped, for `exceptionInfo`
    reasons: BTreeMap<u64, StopReason>,
    breakpoints: FileBreakpoints,
}

impl Adapter {
    fn new(output: Output, interrupt: Held, stopping: Arc<AtomicBool>) -> Self {
        Self {
            output,
            interrupt,
            stopping,
            session: None,
            configuration: None,
            configured: false,
            exited: false,
            handles: Handles::default(),
            threads: ThreadIds::default(),
            announced: Vec::new(),
            reasons: BTreeMap::new(),
            breakpoints: FileBreakpoints::default(),
        }
    }

    /// answer requests, and wait for the program whenever nothing is held
    fn run(
        &mut self,
        launcher: &mut dyn Launcher,
        commands: &Receiver<Incoming>,
    ) -> Result<(), crate::wire::Error> {
        loop {
            if self.stopping.load(Ordering::Relaxed) {
                return Ok(());
            }

            if self.waiting() {
                let mut events = Events::new(&self.output);
                // no deadline: the answer to whatever resumed the program has
                // already been sent, so nothing is waiting on this wait and a
                // timeout would be the answer to no question
                let waited = self
                    .session
                    .as_mut()
                    .expect("the adapter only waits once a program has been launched")
                    .dispatch(Request::Wait { deadline: None }, &mut events);
                let written = events.finish();
                let outcome = match (waited, written) {
                    (_, Err(error)) => Err(Aborted::Wire(error)),
                    (Ok(Response::Ran(running)), Ok(())) => self.report(running),
                    (Ok(other), Ok(())) => unreachable!("a wait was answered with {other:?}"),
                    (Err(error), Ok(())) => {
                        if self.stopping.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        // there is no request outstanding, so there is nothing
                        // to refuse. the program is gone and the client is told
                        // in the only channel that is left
                        self.exited = true;
                        self.say(&format!(
                            "bpd stopped waiting for the program: {}\n",
                            describe(error.as_ref())
                        ))
                        .and_then(|()| self.event("terminated", &serde_json::json!({})))
                    }
                };
                self.finish(outcome)?;
                continue;
            }

            let Ok(message) = commands.recv() else {
                return Ok(());
            };
            let handled = self.handle(launcher, &message);
            match handled {
                Ok(()) => {}
                Err(Aborted::Refuse(reason)) => self.refuse(&message, &reason)?,
                Err(Aborted::Wire(error)) => return Err(error),
            }
        }
    }

    /// whether the adapter should be waiting for the program rather than for
    /// the client
    ///
    /// only when there is a program, it has been configured, nothing is held
    /// and it has not ended. anything else and the next thing to happen is the
    /// client's move
    fn waiting(&self) -> bool {
        self.session.is_some() && self.configured && !self.exited && self.announced.is_empty()
    }

    fn handle(&mut self, launcher: &mut dyn Launcher, message: &Incoming) -> Answered {
        if message.kind != "request" {
            return Err(Aborted::Refuse(format!(
                "a debug adapter is sent requests, and this was a `{}`",
                message.kind
            )));
        }

        match message.command.as_deref() {
            Some("initialize") => self.initialize(message),
            Some("launch") => self.launch(launcher, message),
            Some("attach") => Err(Aborted::Refuse(
                "bpd cannot attach to a running process yet. attaching is PEP 768, which \
                 needs cpython 3.14, and bpd refuses rather than injecting by another \
                 route. use a `launch` configuration"
                    .to_string(),
            )),
            Some("configurationDone") => self.configuration_done(message),
            Some("setBreakpoints") => self.set_breakpoints(message),
            Some("setExceptionBreakpoints") => self.set_exception_breakpoints(message),
            Some("threads") => self.threads(message),
            Some("stackTrace") => self.stack_trace(message),
            Some("scopes") => self.scopes(message),
            Some("variables") => self.variables(message),
            Some("setVariable") => self.set_variable(message),
            Some("evaluate") => self.evaluate(message),
            Some("continue") => self.continue_(message),
            Some("next") => self.step(message, StepKind::Over),
            Some("stepIn") => self.step(message, StepKind::In),
            Some("stepOut") => self.step(message, StepKind::Out),
            Some("exceptionInfo") => self.exception_info(message),
            // DAP has no request for a debug script and never will, so this is
            // an extension — which DAP provides for, and which a client sends
            // with its own `customRequest`. the parity rule is what puts it
            // here: the capability is the core's, and a capability an agent has
            // and a person does not is the thing that rule exists to prevent
            Some("bpd/runScript") => self.run_script(message),
            // the declarative state query and the difference between two of its
            // answers. DAP has no request for either — its own way of reading
            // state is the tree walk, which is still here and answers
            // identically — so they are extensions, for the reason a script is
            Some("bpd/state") => self.state(message),
            Some("bpd/diff") => self.diff(message),
            Some(command) => Err(Aborted::Refuse(format!(
                "bpd's DAP adapter does not implement `{command}`, and does not \
                 advertise a capability that would make a client send it"
            ))),
            None => Err(Aborted::Refuse(
                "a request arrived with no command".to_string(),
            )),
        }
    }

    // ---- the requests ---------------------------------------------------

    fn initialize(&mut self, message: &Incoming) -> Answered {
        // bpd reports 1-based lines and columns because that is what cpython
        // reports. a client that asked for 0-based would be shown every line
        // number one off, so it is refused rather than quietly ignored
        for (field, meaning) in [("linesStartAt1", "lines"), ("columnsStartAt1", "columns")] {
            if message.arguments[field] == serde_json::Value::Bool(false) {
                return Err(Aborted::Refuse(format!(
                    "this client asked for 0-based {meaning}. bpd reports the \
                     {meaning} cpython reports, which are 1-based, and \
                     renumbering them is not something a debugger may get \
                     wrong quietly"
                )));
            }
        }
        if let Some(format) = message.arguments["pathFormat"].as_str()
            && format != "path"
        {
            return Err(Aborted::Refuse(format!(
                "this client asked for sources as `{format}`. bpd identifies a \
                 file by what the filesystem says it is, and only understands \
                 `path`"
            )));
        }

        self.respond(message, Some(capabilities()))?;
        Ok(())
    }

    fn launch(&mut self, launcher: &mut dyn Launcher, message: &Incoming) -> Answered {
        if self.session.is_some() {
            return Err("this session already has a program running".into());
        }

        let configuration: Configuration = serde_json::from_value(message.arguments.clone())
            .map_err(|error| {
                Aborted::Refuse(format!("the launch configuration is not usable: {error}"))
            })?;

        if configuration.no_debug {
            return Err(Aborted::Refuse(
                "`noDebug` asks for the program to be run without a debugger. bpd has \
                 no such path — its agent is how a program is launched at all — so \
                 running it from here would debug it anyway. run it without bpd"
                    .to_string(),
            ));
        }

        let program = Arc::new(Console {
            output: Arc::clone(&self.output),
        });
        match launcher
            .launch(&configuration, program)
            .map_err(|error| failed(&error))?
        {
            Started::Stopped(session) => {
                let reaching = session.interrupt().map_err(|error| failed(&error))?;
                *self.interrupt.lock().expect(REACHING) = Some(reaching);
                self.session = Some(session);
                self.configuration = Some(configuration);
                self.respond(message, None)?;
                // the program is held before its first statement, so the client
                // can bind breakpoints against a real interpreter rather than
                // against a guess about one
                self.event("initialized", &serde_json::json!({}))?;
                Ok(())
            }
            Started::ExitedBeforeStopping { code } => {
                // the program has already said what went wrong, on its own
                // stderr, in the interpreter's own words. adding a line of
                // bpd's would be a line that is not there without the debugger
                self.respond(message, None)?;
                self.exited = true;
                self.event(
                    "exited",
                    &serde_json::json!({ "exitCode": code.unwrap_or(-1) }),
                )?;
                self.event("terminated", &serde_json::json!({}))?;
                Ok(())
            }
        }
    }

    fn configuration_done(&mut self, message: &Incoming) -> Answered {
        self.configured = true;
        self.respond(message, None)?;

        if self.configuration().stop_on_entry {
            self.announce()?;
        } else {
            self.resume(Which::All)?;
        }
        Ok(())
    }

    fn set_breakpoints(&mut self, message: &Incoming) -> Answered {
        let arguments = &message.arguments;
        let file = arguments["source"]["path"].as_str().ok_or(
            "a breakpoint's source has no `path`, and bpd binds a breakpoint by \
             identifying a file on disk",
        )?;
        let file = PathBuf::from(file);

        let mut wanted = Vec::new();
        let empty = Vec::new();
        let requested = arguments["breakpoints"].as_array().unwrap_or(&empty);
        for requested in requested {
            if !requested["hitCondition"].is_null() {
                return Err(Aborted::Refuse(
                    "this client sent a `hitCondition`. DAP carries one as a string whose \
                     meaning is a per-client convention — `>5`, `=5`, `%5` and a bare `5` \
                     are read differently by different debuggers — so bpd does not \
                     advertise the capability and will not guess which was meant"
                        .to_string(),
                ));
            }
            let line = requested["line"]
                .as_u64()
                .and_then(|line| u32::try_from(line).ok())
                .ok_or("a breakpoint arrived with no line")?;
            wanted.push(Wanted {
                line,
                condition: requested["condition"].as_str().map(ToString::to_string),
                log: requested["logMessage"].as_str().map(ToString::to_string),
            });
        }

        let mine = self.breakpoints.replace(file, &wanted);
        let whole = self.breakpoints.all();
        let resolved = match self.ask(Request::SetBreakpoints { breakpoints: whole })? {
            Response::BreakpointsResolved { resolved } => resolved,
            other => unreachable!("a breakpoint set was answered with {other:?}"),
        };

        let answers: Vec<serde_json::Value> = mine
            .iter()
            .map(|breakpoint| {
                let found = resolved.iter().find(|entry| entry.id == breakpoint.id);
                match found {
                    Some(found) => rendered_breakpoint(found, breakpoint),
                    // the answer names every breakpoint of the set, so one that
                    // is missing is the agent contradicting itself rather than
                    // a breakpoint that is merely unbound
                    None => unreachable!(
                        "breakpoint {} was set and the agent said nothing about it",
                        breakpoint.id
                    ),
                }
            })
            .collect();

        self.respond(message, Some(serde_json::json!({ "breakpoints": answers })))?;
        Ok(())
    }

    fn set_exception_breakpoints(&mut self, message: &Incoming) -> Answered {
        let empty = Vec::new();
        let filters = message.arguments["filters"].as_array().unwrap_or(&empty);
        let named: Vec<&str> = filters
            .iter()
            .filter_map(|filter| filter.as_str())
            .collect();

        for filter in &named {
            if !matches!(*filter, "raised" | "uncaught") {
                return Err(Aborted::Refuse(format!(
                    "`{filter}` is not one of the exception filters bpd offers, \
                     which are `raised` and `uncaught`"
                )));
            }
        }

        let armed = match self.ask(Request::SetExceptionBreakpoints {
            raised: named.contains(&"raised"),
            uncaught: named.contains(&"uncaught"),
        })? {
            Response::ExceptionBreakpoints(armed) => armed,
            other => unreachable!("exception breakpoints were answered with {other:?}"),
        };

        // read back off the agent rather than echoed from the request: what is
        // armed is what the agent says is armed
        let verified: Vec<serde_json::Value> = named
            .iter()
            .map(|filter| {
                let on = match *filter {
                    "raised" => armed.raised,
                    _ => armed.uncaught,
                };
                serde_json::json!({ "verified": on })
            })
            .collect();

        self.respond(
            message,
            Some(serde_json::json!({ "breakpoints": verified })),
        )?;
        Ok(())
    }

    fn threads(&mut self, message: &Incoming) -> Answered {
        let settle = self.configuration().settle();
        let census = match self.ask(Request::Threads { settle })? {
            Response::Threads(census) => census,
            other => unreachable!("a thread census was answered with {other:?}"),
        };

        let listed: Vec<serde_json::Value> = census
            .threads
            .iter()
            .map(|state| {
                let where_it_was = match &state.at {
                    Some(at) => at.to_string(),
                    None => "no python frame of the program's".to_string(),
                };
                let name = match state.held {
                    Some(stop) => {
                        format!("{} — held at stop {stop}, in {where_it_was}", state.thread)
                    }
                    None => format!(
                        "{} — running, {:?} over {}ms, in {where_it_was}",
                        state.thread,
                        state.progress,
                        census.settle.as_millis()
                    ),
                };
                serde_json::json!({ "id": self.threads.of(state.thread), "name": name })
            })
            .collect();

        self.respond(message, Some(serde_json::json!({ "threads": listed })))?;
        Ok(())
    }

    fn stack_trace(&mut self, message: &Incoming) -> Answered {
        let stop = self.stop_of(message)?;
        let start = usize::try_from(message.arguments["startFrame"].as_u64().unwrap_or(0))
            .map_err(|_| Aborted::Refuse("`startFrame` is not a frame number".to_string()))?;
        let levels = message.arguments["levels"].as_u64().unwrap_or(0);

        // the walk is bounded from the top, because that is what the capability
        // takes. a client paging from the middle is answered from a walk that
        // reached that far, which is why delayed loading is not advertised
        let top = if levels == 0 {
            None
        } else {
            u32::try_from(start as u64 + levels).ok()
        };

        let stack = match self.ask(Request::Stack { stop, top })? {
            Response::Stack(stack) => stack,
            other => unreachable!("a stack walk was answered with {other:?}"),
        };

        let frames: Vec<serde_json::Value> = stack
            .frames
            .iter()
            .skip(start)
            .map(|frame| {
                serde_json::json!({
                    // which handle it gets is decided here, from the frame's own
                    // kind, so nothing downstream has to guess whether a
                    // `frameId` names a frame the interpreter really has
                    "id": self.handles.add(match frame.kind {
                        bpd_core::FrameKind::Python { .. } => Handle::Frame(frame.id),
                        bpd_core::FrameKind::Template { .. } => Handle::TemplateFrame(frame.id),
                    }),
                    "name": frame.name(),
                    "line": frame.line,
                    "column": 1,
                    "source": source_of(&frame.file),
                })
            })
            .collect();

        self.respond(
            message,
            Some(serde_json::json!({
                "stackFrames": frames,
                "totalFrames": stack.depth,
            })),
        )?;
        Ok(())
    }

    /// run a whole investigation and answer with the transcript of it
    ///
    /// a custom request, because DAP has none of its own: a debug script is a
    /// capability of the core, and the parity rule does not let it be an agent's
    /// alone. for a person this is *run to the third call with a negative amount
    /// and show me the stack* — a real thing to want, and no editor puts a
    /// button on it yet
    ///
    /// the steps and the budget are `bpd_core`'s own types, read straight off
    /// the request. an adapter that had its own shape for them would be a second
    /// definition of a script, and the two would drift
    fn run_script(&mut self, message: &Incoming) -> Answered {
        let stop = self.stop_of(message)?;
        let script: bpd_core::Script = serde_json::from_value(serde_json::json!({
            "steps": message.arguments["steps"],
            "budget": message.arguments["budget"],
        }))
        .map_err(|error| {
            Aborted::Refuse(format!(
                "this is not a debug script: {error}. it takes `steps`, a tree \
                 of them, and a `budget` of `steps`, `wall_ms` and `bytes` — a \
                 script without a budget is a session that can hang"
            ))
        })?;

        let ran = match self.ask(Request::RunScript { stop, script })? {
            Response::Transcript(ran) => ran,
            other => unreachable!("a debug script was answered with {other:?}"),
        };
        // the transcript is the answer, whole. an editor given only where a
        // script ended cannot tell why, for the same reason an agent cannot
        let body = serde_json::to_value(&ran)
            .expect("a transcript is built from types whose serde is derived");
        self.respond(message, Some(body))?;
        Ok(())
    }

    /// describe a stop in one call, the way an agent's front end does
    ///
    /// the parity rule is what puts it here: the capability is the core's, and
    /// "what is the whole state at this stop" is a thing a person wants as much
    /// as an agent. an editor that would rather walk the tree still can, and is
    /// told the same values — the query is composed of the same requests
    fn state(&mut self, message: &Incoming) -> Answered {
        let stop = self.stop_of(message)?;
        let query: bpd_core::StateQuery =
            serde_json::from_value(message.arguments["query"].clone()).map_err(|error| {
                Aborted::Refuse(format!(
                    "this is not a state query: {error}. it takes `frames`, \
                 `scopes`, `expressions`, `source` and a `detail` whose `budget` \
                 bounds the whole of it"
                ))
            })?;

        let snapshot = match self.ask(Request::Query { stop, query })? {
            Response::State(snapshot) => snapshot,
            other => unreachable!("a state query was answered with {other:?}"),
        };
        let body = serde_json::to_value(&snapshot)
            .expect("a snapshot is built from types whose serde is derived");
        self.respond(message, Some(body))?;
        Ok(())
    }

    /// what changed between two states this session read
    ///
    /// it names no thread and touches no program: both states were read when
    /// they were read, and the difference between them is data over data
    fn diff(&mut self, message: &Incoming) -> Answered {
        let parse = |field: &str| -> Result<bpd_core::SnapshotId, Aborted> {
            message.arguments[field]
                .as_str()
                .ok_or_else(|| {
                    Aborted::Refuse(format!(
                        "a `bpd/diff` needs `{field}`, a snapshot id as \
                         `bpd/state` gave it out"
                    ))
                })?
                .parse()
                .map_err(Aborted::Refuse)
        };
        let before = parse("before")?;
        let after = parse("after")?;

        let difference = match self.ask(Request::Diff { before, after })? {
            Response::Difference(difference) => difference,
            other => unreachable!("a difference was answered with {other:?}"),
        };
        let body = serde_json::to_value(&difference)
            .expect("a difference is built from types whose serde is derived");
        self.respond(message, Some(body))?;
        Ok(())
    }

    fn scopes(&mut self, message: &Incoming) -> Answered {
        let reference = message.arguments["frameId"]
            .as_i64()
            .ok_or("a `scopes` request arrived with no `frameId`")?;
        let frame = match self.handles.get(reference) {
            Some(Handle::Frame(frame)) => *frame,
            // a django template frame's scopes are the layers of its template
            // context, one DAP scope each. it is the same tree walk a client
            // already does, over the thing a template frame actually has
            Some(Handle::TemplateFrame(frame)) => return self.template_scopes(message, *frame),
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names {other:?}, not a frame"
                )));
            }
            None => return Err(stale(reference)),
        };

        // all four, always. python resolves a name by which of these it is, and
        // hiding one that happens to be empty would mean reading it first —
        // which is the request the client is about to make anyway
        let scopes: Vec<serde_json::Value> = [
            (Scope::Local, "locals"),
            (Scope::Cell, "locals"),
            (Scope::Free, "locals"),
            (Scope::Global, "globals"),
        ]
        .into_iter()
        .map(|(scope, hint)| {
            serde_json::json!({
                "name": scope.to_string(),
                "presentationHint": hint,
                "variablesReference": self.handles.add(Handle::Scope { frame, scope }),
                // a module namespace begins with `__builtins__`, and a client
                // that opened it without being asked would spend the whole
                // byte budget on it
                "expensive": scope == Scope::Global,
            })
        })
        .collect();

        self.respond(message, Some(serde_json::json!({ "scopes": scopes })))?;
        Ok(())
    }

    /// the layers of a template frame's django context, as DAP scopes
    ///
    /// the context is read here rather than counted from somewhere else,
    /// because how many layers there are is a fact about the render and there is
    /// nowhere else it is written down. the values are read again when a client
    /// opens one, exactly as a python scope's are
    fn template_scopes(&mut self, message: &Incoming, frame: FrameId) -> Answered {
        let detail = self.configuration().variables;
        let context = match self.ask(Request::TemplateContext { frame, detail })? {
            Response::TemplateContext(context) => context,
            other => unreachable!("a template context was answered with {other:?}"),
        };

        let shadowed = context.shadowed();
        let scopes: Vec<serde_json::Value> = context
            .layers
            .iter()
            .map(|layer| {
                let hidden: Vec<&str> = shadowed
                    .iter()
                    .filter(|name| {
                        name.layers.contains(&layer.index)
                            && name.layers.last() != Some(&layer.index)
                    })
                    .map(|name| name.name.as_str())
                    .collect();
                let named = if hidden.is_empty() {
                    format!("template layer {}", layer.index)
                } else {
                    // a name an inner layer shadows is not the value the
                    // template renders, and DAP has nowhere else to say so
                    format!(
                        "template layer {} — shadowed here: {}",
                        layer.index,
                        hidden.join(", ")
                    )
                };
                serde_json::json!({
                    "name": named,
                    "presentationHint": "locals",
                    "variablesReference": self.handles.add(Handle::TemplateLayer {
                        frame,
                        index: layer.index,
                    }),
                    "expensive": false,
                })
            })
            .collect();

        self.respond(message, Some(serde_json::json!({ "scopes": scopes })))?;
        Ok(())
    }

    fn variables(&mut self, message: &Incoming) -> Answered {
        let reference = message.arguments["variablesReference"]
            .as_i64()
            .ok_or("a `variables` request arrived with no `variablesReference`")?;

        for paging in ["start", "count", "filter"] {
            if !message.arguments[paging].is_null() {
                return Err(Aborted::Refuse(format!(
                    "this client sent `{paging}`. bpd does not advertise variable \
                     paging: a value is read with a stated bound on how many \
                     children come back, and the answer says when the bound bit"
                )));
            }
        }

        let handle = self
            .handles
            .get(reference)
            .ok_or_else(|| stale(reference))?
            .clone();

        let variables = match handle {
            Handle::Scope { frame, scope } => self.read_scope(frame, scope, &[])?,
            Handle::Nested { frame, scope, path } => self.read_scope(frame, scope, &path)?,
            Handle::Stored { stop, value } => {
                let mut listed = Vec::new();
                for child in crate::handles::children(&value) {
                    let reference = if expandable(child.value) {
                        self.handles.add(Handle::Stored {
                            stop,
                            value: child.value.clone(),
                        })
                    } else {
                        0
                    };
                    listed.push(variable(&child.name, child.value, reference));
                }
                listed
            }
            Handle::TemplateLayer { frame, index } => self.read_template_layer(frame, index)?,
            Handle::Frame(frame) | Handle::TemplateFrame(frame) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names {frame}, which is a frame. ask for its \
                     scopes and then for the variables of one of them"
                )));
            }
        };

        self.respond(message, Some(serde_json::json!({ "variables": variables })))?;
        Ok(())
    }

    /// read one scope, opened as far as `path` goes, and list what is at `path`
    ///
    /// the depth asked for is the length of the path, so the client's own
    /// expansion is what decides how deep the read goes. a scope read at depth
    /// zero reports each name's type and size and opens nothing, which is what
    /// a collapsed tree shows
    /// what one layer of a template frame's django context holds
    ///
    /// the context is read again rather than remembered, for the reason a
    /// python scope is: the program is still running, and a value read at the
    /// stop and shown a minute later is a value that may already be something
    /// else
    ///
    /// a value inside a layer opens no further. django's context is not a scope
    /// bpd can walk a path back into — `Request::TemplateContext` reads the
    /// layers, and there is no request that re-reads one name of one of them —
    /// so what a client gets is the layer as it was read, to the depth the
    /// configured bounds allowed, and the answer says where that ran out
    fn read_template_layer(
        &mut self,
        frame: FrameId,
        index: u32,
    ) -> Result<Vec<serde_json::Value>, Aborted> {
        let detail = self.configuration().variables;
        let context = match self.ask(Request::TemplateContext { frame, detail })? {
            Response::TemplateContext(context) => context,
            other => unreachable!("a template context was answered with {other:?}"),
        };

        let layer = context
            .layers
            .iter()
            .find(|layer| layer.index == index)
            .ok_or_else(|| {
                Aborted::Refuse(format!(
                    "the template context of {frame} no longer has a layer \
                     {index}. it was read again to open it and the render has \
                     moved on since"
                ))
            })?;

        let mut listed = Vec::new();
        for entry in &layer.entries {
            let reference = if expandable(&entry.value) {
                self.stored(frame.stop, &entry.value)
            } else {
                0
            };
            listed.push(variable(&entry.name, &entry.value, reference));
        }
        for omitted in &layer.omitted {
            listed.push(serde_json::json!({
                "name": "(left out)",
                "value": omitted.to_string(),
                "variablesReference": 0,
            }));
        }
        Ok(listed)
    }

    fn read_scope(
        &mut self,
        frame: FrameId,
        scope: Scope,
        path: &[Step],
    ) -> Result<Vec<serde_json::Value>, Aborted> {
        let detail = Detail {
            depth: u32::try_from(path.len()).map_err(|_| {
                Aborted::from("a value cannot be expanded four billion levels down")
            })?,
            ..self.configuration().variables
        };

        let read = match self.ask(Request::Variables {
            frame,
            scope,
            detail,
        })? {
            Response::Variables(read) => read,
            other => unreachable!("a scope read was answered with {other:?}"),
        };

        let mut listed = Vec::new();

        if let Some((name, rest)) = path.split_first() {
            let Step::Attribute(name) = name else {
                unreachable!("the first step into a scope is one of its names, and was {name}")
            };
            let entry = read
                .entries
                .iter()
                .find(|entry| entry.name == *name)
                .ok_or_else(|| {
                    Aborted::Refuse(format!(
                        "the {scope} scope of {frame} no longer holds `{name}`. the \
                         value was read again to open it and the program has \
                         changed since — every thread but the held one keeps \
                         running"
                    ))
                })?;
            let node = crate::handles::walk(&entry.value, rest)
                .map_err(|lost| Aborted::Refuse(lost.to_string()))?;

            for child in crate::handles::children(node) {
                let mut deeper = path.to_vec();
                deeper.push(child.step);
                let reference = if expandable(child.value) {
                    self.handles.add(Handle::Nested {
                        frame,
                        scope,
                        path: deeper,
                    })
                } else {
                    0
                };
                listed.push(variable(&child.name, child.value, reference));
            }
            return Ok(listed);
        }

        for entry in &read.entries {
            let reference = if expandable(&entry.value) {
                self.handles.add(Handle::Nested {
                    frame,
                    scope,
                    path: vec![Step::Attribute(entry.name.clone())],
                })
            } else {
                0
            };
            listed.push(variable(&entry.name, &entry.value, reference));
        }

        // a name the scope has and the frame does not hold yet, and a name whose
        // value the frame does not expose. neither is a value and neither is
        // absent, so neither is left out
        for name in &read.unbound {
            listed.push(serde_json::json!({
                "name": name,
                "value": "unbound — the scope has this name and the frame holds nothing under it at this line",
                "variablesReference": 0,
            }));
        }
        for name in &read.unreadable {
            listed.push(serde_json::json!({
                "name": name,
                "value": "unreadable — the value lives in a cell only the function object holds, which is how a class body sees a variable of the function around it",
                "variablesReference": 0,
            }));
        }
        for omitted in &read.omitted {
            listed.push(serde_json::json!({
                "name": "(left out)",
                "value": omitted.to_string(),
                "variablesReference": 0,
            }));
        }

        Ok(listed)
    }

    fn set_variable(&mut self, message: &Incoming) -> Answered {
        let reference = message.arguments["variablesReference"]
            .as_i64()
            .ok_or("a `setVariable` request arrived with no `variablesReference`")?;
        let name = message.arguments["name"]
            .as_str()
            .ok_or("a `setVariable` request arrived with no `name`")?
            .to_string();
        let value = message.arguments["value"]
            .as_str()
            .ok_or("a `setVariable` request arrived with no `value`")?
            .to_string();

        let (frame, scope) = match self.handles.get(reference) {
            Some(Handle::Scope { frame, scope }) => (*frame, *scope),
            Some(_) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names something inside a value, and bpd writes a \
                     **name of a frame's scope**. writing an attribute or an item \
                     means running the program's own `__setattr__` or \
                     `__setitem__`, which is the program rather than the debugger"
                )));
            }
            None => return Err(stale(reference)),
        };

        let detail = self.configuration().variables;
        let written = match self.ask(Request::SetVariable {
            frame,
            scope,
            name,
            value,
            detail,
        })? {
            Response::Evaluated(written) => written,
            other => unreachable!("a variable write was answered with {other:?}"),
        };

        let value = match written {
            Evaluated::Value { value } => value,
            Evaluated::Raised { error } => {
                return Err(Aborted::Refuse(format!(
                    "the value to write did not evaluate: {error}"
                )));
            }
        };

        let reference = self.stored(frame.stop, &value);
        self.respond(
            message,
            Some(serde_json::json!({
                "value": summary(&value),
                "type": value.kind,
                "variablesReference": reference,
            })),
        )?;
        Ok(())
    }

    fn evaluate(&mut self, message: &Incoming) -> Answered {
        let expression = message.arguments["expression"]
            .as_str()
            .ok_or("an `evaluate` request arrived with no expression")?
            .to_string();
        let reference = message.arguments["frameId"].as_i64().ok_or(
            "an `evaluate` request arrived with no `frameId`. bpd evaluates an \
             expression **in a frame**, because that is what decides what a name \
             means — there is no frameless context to evaluate one in",
        )?;
        // either kind of frame: a template frame is where template syntax is
        // evaluated, and refusing one here would leave the capability with no
        // DAP route at all
        let frame = match self.handles.get(reference) {
            Some(Handle::Frame(frame) | Handle::TemplateFrame(frame)) => *frame,
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names {other:?}, not a frame"
                )));
            }
            None => return Err(stale(reference)),
        };

        let detail = self.configuration().variables;
        let evaluated = match self.ask(Request::Evaluate {
            frame,
            expression,
            detail,
        })? {
            Response::Evaluated(evaluated) => evaluated,
            other => unreachable!("an evaluation was answered with {other:?}"),
        };

        let value = match evaluated {
            Evaluated::Value { value } => value,
            // an expression that raised has an answer and the answer is the
            // exception. reporting `None` for it would be inventing a value the
            // program never produced
            Evaluated::Raised { error } => return Err(Aborted::Refuse(error.to_string())),
        };

        let reference = self.stored(frame.stop, &value);
        self.respond(
            message,
            Some(serde_json::json!({
                "result": summary(&value),
                "type": value.kind,
                "variablesReference": reference,
            })),
        )?;
        Ok(())
    }

    fn continue_(&mut self, message: &Incoming) -> Answered {
        let single = message.arguments["singleThread"] == serde_json::Value::Bool(true);
        let which = if single {
            let stop = self.stop_of(message)?;
            let thread = self
                .announced
                .iter()
                .find(|held| held.stop == stop)
                .map(|held| held.thread)
                .ok_or_else(|| Aborted::Refuse(format!("stop {stop} is not held")))?;
            Which::Named {
                threads: vec![thread],
            }
        } else {
            Which::All
        };

        self.resume(which)?;
        self.respond(
            message,
            Some(serde_json::json!({ "allThreadsContinued": !single })),
        )?;
        Ok(())
    }

    fn step(&mut self, message: &Incoming, kind: StepKind) -> Answered {
        if let Some(granularity) = message.arguments["granularity"].as_str()
            && granularity == "instruction"
        {
            return Err(Aborted::Refuse(
                "bpd steps by line. an instruction step would mean reporting a \
                 location between two lines of source, and there is no line \
                 number that describes one"
                    .to_string(),
            ));
        }

        let stop = self.stop_of(message)?;
        match self.ask(Request::Step { stop, kind })? {
            Response::Resumed { .. } => {}
            other => unreachable!("a step was answered with {other:?}"),
        }
        self.respond(message, None)?;
        Ok(())
    }

    fn exception_info(&mut self, message: &Incoming) -> Answered {
        let stop = self.stop_of(message)?;
        let reason = self
            .reasons
            .get(&stop)
            .ok_or_else(|| Aborted::Refuse(format!("nothing is recorded about stop {stop}")))?
            .clone();

        let (error, mode, description) = match &reason {
            StopReason::Raised { error, .. } => (error, "always", "raised here".to_string()),
            StopReason::Uncaught { error, .. } => (
                error,
                "unhandled",
                "leaving the program's outermost frame".to_string(),
            ),
            StopReason::EvaluationFailed {
                breakpoint,
                part,
                expression,
                error,
                ..
            } => (
                error,
                "always",
                format!("the {part} `{expression}` of breakpoint {breakpoint} raised"),
            ),
            other => {
                return Err(Aborted::Refuse(format!(
                    "that thread did not stop for an exception — it stopped for \
                     {other:?}"
                )));
            }
        };

        let mut traceback = String::new();
        for frame in &error.traceback {
            use std::fmt::Write as _;
            writeln!(
                traceback,
                "  File \"{}\", line {}, in {}",
                frame.file, frame.line, frame.function
            )
            .expect("writing to a string cannot fail");
        }

        self.respond(
            message,
            Some(serde_json::json!({
                "exceptionId": error.kind,
                "description": format!("{error} — {description}"),
                "breakMode": mode,
                "details": {
                    "message": error.message,
                    "typeName": error.kind,
                    "stackTrace": traceback,
                },
            })),
        )?;
        Ok(())
    }

    // ---- what the program did ------------------------------------------

    fn report(&mut self, running: Running) -> Answered {
        let rebound = match &running {
            Running::Stopped { rebound, .. }
            | Running::Exited { rebound, .. }
            | Running::Finishing { rebound, .. }
            | Running::StillRunning { rebound, .. } => rebound.clone(),
        };
        self.rebound(&rebound)?;

        match running {
            // only a wait that carried a deadline can be answered with this,
            // and this adapter never sends one — see `coverage::reach_of`
            Running::StillRunning { waited, .. } => unreachable!(
                "a DAP wait carries no deadline and was answered after {waited:?} \
                 with the program still running"
            ),
            Running::Stopped { .. } => self.announce(),
            Running::Exited { status, .. } => {
                self.exited = true;
                self.event(
                    "exited",
                    &serde_json::json!({ "exitCode": exit_code(status) }),
                )?;
                self.event("terminated", &serde_json::json!({}))
            }
            Running::Finishing { threads, .. } => {
                self.say(&format!(
                    "the program has run to its end with {} thread(s) still held: {threads:?}. \
                     the interpreter finalizes by joining its non-daemon threads and a held \
                     one cannot be joined, so the process is sitting there until they are \
                     resumed\n",
                    threads.len()
                ))?;
                self.announce()
            }
        }
    }

    /// tell the client about every stop it has not been told about
    ///
    /// a stop holds one thread and the others keep running, so a second thread
    /// can stop while a first is held — and it arrives on the connection rather
    /// than as the answer to anything
    fn announce(&mut self) -> Answered {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let held = session.held();

        // a stop that is no longer held is one whose thread has run on. every
        // reference minted at it names a frame that has moved, so the numbers
        // stop resolving here rather than being answered about a different
        // program
        let ended: Vec<u64> = self
            .announced
            .iter()
            .filter(|known| !held.iter().any(|stop| stop.stop == known.stop))
            .map(|known| known.stop)
            .collect();
        self.handles.forget(&ended);
        for stop in &ended {
            self.reasons.remove(stop);
        }

        let fresh: Vec<Stop> = held
            .iter()
            .filter(|stop| !self.announced.iter().any(|known| known.stop == stop.stop))
            .cloned()
            .collect();
        // set before the loop below, which asks the session for more: the ask
        // announces in its turn, and would otherwise announce these again
        self.announced = held;

        for stop in fresh {
            self.reasons.insert(stop.stop, stop.reason.clone());

            let whole_program = if self.configuration().stop_the_world {
                let settle = self.configuration().settle();
                let stopped = match self.ask(Request::StopTheWorld {
                    stop: stop.stop,
                    settle,
                })? {
                    Response::WorldStopped(stopped) => stopped,
                    other => unreachable!("stopping the world was answered with {other:?}"),
                };
                if !stopped.native.is_empty() {
                    self.say(&format!(
                        "stop {}: {} thread(s) could not be held and are still running — \
                         they are parked in a C call, where there is no monitoring event \
                         to hold one at: {:?}\n",
                        stop.stop,
                        stopped.native.len(),
                        stopped.native
                    ))?;
                }
                stopped.native.is_empty()
            } else {
                false
            };

            for holding in &stop.holding {
                self.say(&format!(
                    "stop {}: this thread holds {holding}\n",
                    stop.stop
                ))?;
            }

            let (reason, description, breakpoints) = stopped_for(&stop.reason);
            let thread = self.threads.of(stop.thread);
            self.event(
                "stopped",
                &serde_json::json!({
                    "reason": reason,
                    "description": description,
                    "text": description,
                    "threadId": thread,
                    "allThreadsStopped": whole_program,
                    "hitBreakpointIds": breakpoints,
                    "preserveFocusHint": false,
                }),
            )?;
        }
        Ok(())
    }

    /// say what loading a file changed about the breakpoint set
    fn rebound(&mut self, rebound: &[Resolved]) -> Answered {
        for resolved in rebound {
            let requested = self.breakpoints.requested(resolved.id);
            let Some(requested) = requested else {
                continue;
            };
            let body = rendered_breakpoint(resolved, &requested);
            self.event(
                "breakpoint",
                &serde_json::json!({ "reason": "changed", "breakpoint": body }),
            )?;
        }
        Ok(())
    }

    // ---- the plumbing ---------------------------------------------------

    /// ask the session for something, rendering a failure as a refusal
    fn ask(&mut self, request: Request) -> Result<Response, Aborted> {
        let mut events = Events::new(&self.output);
        let answered = self
            .session
            .as_mut()
            .ok_or("nothing has been launched yet")?
            .dispatch(request, &mut events)
            .map_err(|error| failed(&error));
        events.finish().map_err(Aborted::Wire)?;
        let answered = answered?;
        // a request answered on one thread can arrive with another thread's stop
        // behind it. saying nothing about that one would leave a client
        // believing a thread is running that is not
        if self.configured {
            self.announce()?;
        }
        Ok(answered)
    }

    fn stored(&mut self, stop: u64, value: &Value) -> i64 {
        if expandable(value) {
            self.handles.add(Handle::Stored {
                stop,
                value: value.clone(),
            })
        } else {
            0
        }
    }

    /// the stop of the thread a request names
    fn stop_of(&self, message: &Incoming) -> Result<u64, Aborted> {
        let thread = message.arguments["threadId"]
            .as_i64()
            .ok_or("that request names no thread, and every one of these is about one")?;
        let python = self.threads.python(thread).ok_or_else(|| {
            Aborted::Refuse(format!("thread {thread} is not one bpd has reported"))
        })?;
        self.announced
            .iter()
            .find(|stop| stop.thread == python)
            .map(|stop| stop.stop)
            .ok_or_else(|| {
                Aborted::Refuse(format!(
                    "thread {thread} is not held. a stop holds one thread and \
                     leaves the rest running, so a thread bpd never stopped is \
                     one it cannot be asked about"
                ))
            })
    }

    fn configuration(&self) -> &Configuration {
        self.configuration
            .as_ref()
            .expect("nothing reads the configuration before a launch has stored one")
    }

    fn respond(&self, message: &Incoming, body: Option<serde_json::Value>) -> Answered {
        self.output
            .lock()
            .expect(WRITING)
            .respond(message, body)
            .map_err(Aborted::Wire)
    }

    fn refuse(&self, message: &Incoming, reason: &str) -> Result<(), crate::wire::Error> {
        self.output.lock().expect(WRITING).refuse(message, reason)
    }

    fn event(&self, event: &str, body: &serde_json::Value) -> Answered {
        self.output
            .lock()
            .expect(WRITING)
            .event(event, body)
            .map_err(Aborted::Wire)
    }

    fn say(&self, text: &str) -> Answered {
        self.event(
            "output",
            &serde_json::json!({ "category": "console", "output": text }),
        )
    }

    /// let one held thread go, or all of them
    ///
    /// what belonged to the stops that ended is forgotten by [`Self::announce`],
    /// which every ask goes through — a stop can also end without anything here
    /// resuming it, and one rule covers both
    fn resume(&mut self, which: Which) -> Answered {
        match self.ask(Request::Resume { which })? {
            Response::Resumed { .. } => {}
            other => unreachable!("a resume was answered with {other:?}"),
        }
        Ok(())
    }

    /// deal with something that went wrong while nothing was waiting for an
    /// answer
    ///
    /// there is no request to refuse, so a refusal becomes an `output` event —
    /// and a connection that failed ends the session, because there is nothing
    /// left to say it on
    fn finish(&self, outcome: Answered) -> Result<(), crate::wire::Error> {
        match outcome {
            Ok(()) => Ok(()),
            Err(Aborted::Wire(error)) => Err(error),
            Err(Aborted::Refuse(reason)) => match self.say(&format!("bpd: {reason}\n")) {
                Ok(()) => Ok(()),
                Err(Aborted::Wire(error)) => Err(error),
                Err(Aborted::Refuse(reason)) => {
                    unreachable!("saying something cannot be refused, and was: {reason}")
                }
            },
        }
    }
}

/// what a handler returns: nothing, or the reason it got no further
type Answered = Result<(), Aborted>;

/// why a handler stopped
///
/// the two are not the same thing and are not treated as one. a refusal is an
/// answer the client receives; a connection that failed is the end of the
/// session, and pretending it was a refusal would mean trying to write the
/// refusal down the connection that has just gone
#[derive(Debug)]
enum Aborted {
    /// the request is refused, and this is what the client is told
    Refuse(String),
    /// the client's connection failed
    Wire(crate::wire::Error),
}

impl From<crate::wire::Error> for Aborted {
    fn from(error: crate::wire::Error) -> Self {
        Self::Wire(error)
    }
}

impl From<String> for Aborted {
    fn from(reason: String) -> Self {
        Self::Refuse(reason)
    }
}

impl From<&str> for Aborted {
    fn from(reason: &str) -> Self {
        Self::Refuse(reason.to_string())
    }
}

/// a session failure, as the reason a request is refused
fn failed(error: &Failed) -> Aborted {
    Aborted::Refuse(describe(error.as_ref()))
}

fn stale(reference: i64) -> Aborted {
    Aborted::Refuse(format!(
        "{reference} is not a reference bpd is holding. a reference belongs \
             to one stop, and the thread it named has run on since — ask for \
             the stack again"
    ))
}

/// what a stop turns into on the wire: a reason, a description, and the
/// breakpoints that decided it
fn stopped_for(reason: &StopReason) -> (&'static str, String, Vec<u32>) {
    match reason {
        StopReason::Entry => (
            "entry",
            "held before the program's first statement".to_string(),
            Vec::new(),
        ),
        StopReason::Breakpoint {
            breakpoints,
            file,
            line,
        } => (
            "breakpoint",
            format!("breakpoint {breakpoints:?} at {file}:{line}"),
            breakpoints.clone(),
        ),
        StopReason::Stepped { kind, file, line } => (
            "step",
            format!("{kind} landed at {file}:{line}"),
            Vec::new(),
        ),
        StopReason::Paused { file, line } => (
            "pause",
            format!("held at {file}:{line} — the first thread to reach a line"),
            Vec::new(),
        ),
        StopReason::Raised { error, file, line } => (
            "exception",
            format!("{error} raised at {file}:{line}"),
            Vec::new(),
        ),
        StopReason::Uncaught { error, file, line } => (
            "exception",
            format!("{error} is leaving the program at {file}:{line}"),
            Vec::new(),
        ),
        StopReason::EvaluationFailed {
            breakpoint,
            part,
            expression,
            file,
            line,
            error,
        } => (
            "exception",
            format!(
                "the {part} `{expression}` of breakpoint {breakpoint} raised {error} \
                 at {file}:{line}. the program is held rather than resumed: an \
                 expression that raised has not said `false`"
            ),
            Vec::new(),
        ),
        // `StopReason` is `#[non_exhaustive]`: a reason this adapter has not
        // been taught is reported as itself rather than as one it knows
        other => ("pause", format!("{other:?}"), Vec::new()),
    }
}

/// how a resolved breakpoint is reported back
fn rendered_breakpoint(resolved: &Resolved, requested: &SourceBreakpoint) -> serde_json::Value {
    match &resolved.binding {
        Binding::Bound { line, sites, .. } => {
            let mut body = serde_json::json!({
                "id": resolved.id,
                "verified": true,
                "line": line,
                "source": source_of(&requested.file.display().to_string()),
            });
            if *line != requested.line {
                body["message"] = format!(
                    "line {} is not executable, so this moved to line {line}, which is",
                    requested.line
                )
                .into();
            } else if sites.len() > 1 {
                // one source line can belong to several code objects — a `def`
                // line is in the class body and is the method's first line — and
                // every one of them is armed
                body["message"] = format!("armed in {} code objects", sites.len()).into();
            }
            body
        }
        Binding::BoundInTemplate { line, nodes, .. } => {
            let mut body = serde_json::json!({
                "id": resolved.id,
                "verified": true,
                "line": line,
                "source": source_of(&requested.file.display().to_string()),
            });
            if *line != requested.line {
                body["message"] = format!(
                    "line {} renders no django node, so this moved to line \
                     {line}, which does",
                    requested.line
                )
                .into();
            } else if nodes.len() > 1 {
                body["message"] = format!("armed on {} django nodes", nodes.len()).into();
            }
            body
        }
        Binding::Unbound { reason } => serde_json::json!({
            "id": resolved.id,
            "verified": false,
            "line": requested.line,
            "source": source_of(&requested.file.display().to_string()),
            "message": reason.to_string(),
            // a file that is not imported yet binds when it is, and everything
            // else will not bind at all. the distinction is the core's
            "reason": if matches!(reason, Unbound::NotLoaded { .. }) { "pending" } else { "failed" },
        }),
    }
}

/// one DAP variable
fn variable(name: &str, value: &Value, reference: i64) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "value": summary(value),
        "type": value.kind,
        "variablesReference": reference,
    })
}

/// a DAP source for a `co_filename`
///
/// a path is only claimed when the name is one. `<string>`, `<stdin>` and
/// `<frozen importlib._bootstrap>` are real names for code that did not come
/// from the filesystem, and handing one to a client as a path makes it look for
/// a file that is not there
fn source_of(file: &str) -> serde_json::Value {
    let path = Path::new(file);
    if path.is_absolute() {
        let name = path.file_name().map_or_else(
            || file.to_string(),
            |name| name.to_string_lossy().to_string(),
        );
        serde_json::json!({ "name": name, "path": file })
    } else {
        serde_json::json!({ "name": file })
    }
}

/// the program's own stdout and stderr, as the client sees them
///
/// a `stderr` line is categorised as one so a client shows it as the program
/// failing rather than as something it printed. neither is bpd's own voice, and
/// nothing is added to either
struct Console {
    output: Output,
}

impl ProgramOutput for Console {
    fn wrote(&self, stream: Stream, text: &str) {
        let category = match stream {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        };
        // there is nothing to do about a client that has gone: this is a
        // forwarding thread, and the session it would report to is the one that
        // is not there
        let written = self.output.lock().expect(WRITING).event(
            "output",
            &serde_json::json!({ "category": category, "output": text }),
        );
        drop(written);
    }
}

/// what the program said while it ran
///
/// a logpoint's record and a pause's acknowledgement are not answers to
/// anything, so they become `output` events. a write that fails is kept and
/// raised when the dispatch that produced it returns, because a `Reporting`
/// method has nowhere to return one
struct Events<'a> {
    output: &'a Output,
    failed: Option<crate::wire::Error>,
}

impl<'a> Events<'a> {
    const fn new(output: &'a Output) -> Self {
        Self {
            output,
            failed: None,
        }
    }

    fn finish(self) -> Result<(), crate::wire::Error> {
        match self.failed {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn emit(&mut self, body: &serde_json::Value) {
        if self.failed.is_some() {
            return;
        }
        let written = self.output.lock().expect(WRITING).event("output", body);
        if let Err(error) = written {
            self.failed = Some(error);
        }
    }
}

impl Reporting for Events<'_> {
    fn logged(&mut self, record: LogRecord) {
        self.emit(&serde_json::json!({
            "category": "stdout",
            "output": format!("{}\n", record.message),
            "source": source_of(&record.file),
            "line": record.line,
        }));
    }

    fn pausing(&mut self, running: Vec<u64>) {
        let text = if running.is_empty() {
            "a pause is armed and nothing is going to arrive: every thread is parked in a C \
             call, where there is no monitoring event to hold one at\n"
                .to_string()
        } else {
            format!(
                "a pause is armed. {} thread(s) were running python when it went on: \
                 {running:?}\n",
                running.len()
            )
        };
        self.emit(&serde_json::json!({ "category": "console", "output": text }));
    }

    /// the program started a child, on the `console` category
    ///
    /// `console` rather than `stdout`, because the program did not write this —
    /// bpd did. a client showing it among the program's own output would be
    /// putting words in the debuggee's mouth
    ///
    /// there is no `source` and no `line` on it, and that is deliberate: the
    /// audit hook runs on whatever thread made the child and reports what the
    /// program asked the operating system for, not where in the program it was
    /// asked. a location invented from the frame that happened to be running
    /// would be a location nobody can act on
    fn spawned(&mut self, child: bpd_core::Spawn) {
        self.emit(&serde_json::json!({
            "category": "console",
            "output": format!("{child}\n"),
        }));
    }
}

/// what one DAP `setBreakpoints` asked for, for one line
struct Wanted {
    line: u32,
    condition: Option<String>,
    log: Option<String>,
}

/// the breakpoints each file last asked for
///
/// DAP replaces one file's breakpoints at a time and
/// [`Request::SetBreakpoints`] replaces the whole set, so this is where the
/// union lives. it holds what the *client* said and nothing about the program:
/// where a breakpoint bound, and whether it bound at all, is the core's answer
#[derive(Debug, Default)]
struct FileBreakpoints {
    by_file: BTreeMap<PathBuf, Vec<SourceBreakpoint>>,
    next: u32,
}

impl FileBreakpoints {
    /// replace one file's breakpoints, and return the file's new set
    fn replace(&mut self, file: PathBuf, wanted: &[Wanted]) -> Vec<SourceBreakpoint> {
        let mine: Vec<SourceBreakpoint> = wanted
            .iter()
            .map(|wanted| {
                self.next += 1;
                let mut breakpoint = SourceBreakpoint::at(self.next, file.clone(), wanted.line);
                breakpoint.condition.clone_from(&wanted.condition);
                breakpoint.log.clone_from(&wanted.log);
                breakpoint
            })
            .collect();
        self.by_file.insert(file, mine.clone());
        mine
    }

    /// every breakpoint that should be armed now
    fn all(&self) -> Vec<SourceBreakpoint> {
        self.by_file.values().flatten().cloned().collect()
    }

    /// what was asked for under an id, for a report that names it later
    fn requested(&self, id: u32) -> Option<SourceBreakpoint> {
        self.by_file
            .values()
            .flatten()
            .find(|breakpoint| breakpoint.id == id)
            .cloned()
    }
}

/// the client's thread numbers, and the interpreter's
///
/// DAP carries a thread as a signed integer and cpython's `threading.get_ident`
/// is an address-shaped `u64`, so the two are mapped rather than cast. the
/// interpreter's own number is in every thread's *name*, because that is the
/// one a `faulthandler` traceback or a log line will show
#[derive(Debug, Default)]
struct ThreadIds {
    to_dap: BTreeMap<u64, i64>,
    to_python: BTreeMap<i64, u64>,
    next: i64,
}

impl ThreadIds {
    fn of(&mut self, python: u64) -> i64 {
        if let Some(known) = self.to_dap.get(&python) {
            return *known;
        }
        self.next += 1;
        self.to_dap.insert(python, self.next);
        self.to_python.insert(self.next, python);
        self.next
    }

    fn python(&self, dap: i64) -> Option<u64> {
        self.to_python.get(&dap).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_is_only_given_a_path_when_the_name_is_one() {
        let real = source_of("/tmp/app/main.py");
        assert_eq!(real["path"], "/tmp/app/main.py");
        assert_eq!(real["name"], "main.py");

        // code the interpreter has under a name that is not a file: a string
        // passed to `exec`, a frozen module, the `-c` a session was entered
        // through. a client given one as a path looks for a file that is not
        // there and shows the user an editor error instead of a frame
        let pseudo = source_of("<frozen importlib._bootstrap>");
        assert!(pseudo["path"].is_null(), "gave {pseudo}");
        assert_eq!(pseudo["name"], "<frozen importlib._bootstrap>");
    }

    #[test]
    fn one_file_at_a_time_adds_up_to_the_whole_set() {
        // DAP replaces one file's breakpoints; the core replaces the whole set,
        // because a debugger that accumulates edits has two ideas of what is set
        let mut breakpoints = FileBreakpoints::default();
        breakpoints.replace(
            PathBuf::from("/a.py"),
            &[Wanted {
                line: 1,
                condition: None,
                log: None,
            }],
        );
        breakpoints.replace(
            PathBuf::from("/b.py"),
            &[Wanted {
                line: 2,
                condition: Some("x > 1".to_string()),
                log: None,
            }],
        );

        assert_eq!(breakpoints.all().len(), 2);
        assert_eq!(breakpoints.all()[1].condition.as_deref(), Some("x > 1"));

        // replacing one file leaves the other alone, and every id is still
        // distinct — an id names one breakpoint in every report about it
        breakpoints.replace(PathBuf::from("/a.py"), &[]);
        let whole = breakpoints.all();
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].file, PathBuf::from("/b.py"));
    }

    #[test]
    fn a_thread_number_is_mapped_rather_than_cast() {
        // cpython's identity is an address, which does not fit DAP's signed
        // number in general, and casting one that does not fit would name a
        // different thread
        let mut threads = ThreadIds::default();
        let huge = u64::MAX - 3;
        let first = threads.of(huge);

        assert!(first > 0);
        assert_eq!(threads.of(huge), first, "the same thread keeps its number");
        assert_eq!(threads.python(first), Some(huge));
        assert_eq!(threads.python(first + 99), None);
    }

    #[test]
    fn a_breakpoint_that_moved_says_so_and_one_that_did_not_bind_says_why() {
        use bpd_core::{Evaluation, Site};

        let requested = SourceBreakpoint::at(4, "/tmp/app.py", 7);
        let moved = rendered_breakpoint(
            &Resolved {
                id: 4,
                binding: Binding::Bound {
                    line: 9,
                    sites: vec![Site {
                        qualname: "main".to_string(),
                        first_line: 1,
                        offset: 12,
                    }],
                    evaluation: Evaluation::Always,
                },
            },
            &requested,
        );
        assert_eq!(moved["verified"], true);
        assert_eq!(moved["line"], 9);
        assert!(
            moved["message"]
                .as_str()
                .expect("it moved")
                .contains("line 9"),
            "said {moved}"
        );

        let pending = rendered_breakpoint(
            &Resolved {
                id: 4,
                binding: Binding::Unbound {
                    reason: Unbound::NotLoaded {
                        file: PathBuf::from("/tmp/app.py"),
                        templates_available: false,
                    },
                },
            },
            &requested,
        );
        assert_eq!(pending["verified"], false);
        assert_eq!(
            pending["reason"], "pending",
            "a module that is not imported yet binds when it is"
        );
        assert!(
            pending["message"]
                .as_str()
                .expect("an unbound breakpoint says why")
                .contains("imported later"),
            "said {pending}"
        );
    }
}
