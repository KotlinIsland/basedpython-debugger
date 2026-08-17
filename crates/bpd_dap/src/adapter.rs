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
use std::time::{Duration, Instant};

use bpd_core::{
    Addressed, Binding, Detail, Evaluated, Forwarded, FrameId, LogRecord, Reporting, Request,
    Resolved, Response, Running, Scope, SessionId, SourceBreakpoint, StepKind, Stop, StopReason,
    Value, Which, exit_code,
};

use crate::capabilities::capabilities;
use crate::configuration::Configuration;
use crate::handles::{Handle, Handles, Step};
use crate::render::{expandable, summary};
use crate::session::{
    Failed, Interrupt, Launcher, ProgramOutput, Reachable, Session, Started, Stream, describe,
};
use crate::wire::{Incoming, Reader, Writer};

/// the shared writing end of the client connection
type Output = Arc<Mutex<Writer<Box<dyn Write + Send>>>>;

/// the handle the reader thread uses to reach a running program
type Held = Arc<Mutex<Option<Box<dyn Interrupt>>>>;

/// how long a refused launch waits for a client to say goodbye
///
/// long enough for a client that answers a failed launch by disconnecting —
/// which is one round trip on a connection that is already open — and short
/// enough that a client which never answers is not what the run is waiting on
const FAREWELL: Duration = Duration::from_secs(2);

/// how often that wait looks
const FAREWELL_POLL: Duration = Duration::from_millis(10);

/// how the adapter's loop ended, which decides whether to wait for the client
///
/// the difference is whether there is anything left to hear. a session that ran
/// ends when the client hangs up, and joining the reader is what puts the answer
/// to its `disconnect` on the wire before this process goes away — but a session
/// that never began ends with a client which may say nothing ever again, and
/// waiting on one is the hang this distinction exists to remove
enum Ended {
    /// the client went away, asked to disconnect, or the program finished
    WithTheClient,
    /// `launch` or `attach` was refused, and there was never a session
    NothingToDebug,
}

/// whether this request is one a session's existence depends on
///
/// the two that bring a session into being. every other request is asked *of* a
/// session and refusing one leaves it standing
fn begins_a_session(message: &Incoming) -> bool {
    matches!(message.command.as_deref(), Some("launch" | "attach"))
}

/// serve one DAP client over `input` and `output`
///
/// returns when the client hangs up or disconnects, or when a `launch` this
/// adapter refused leaves no session to serve. the debuggee never outlives it:
/// a client that vanishes leaves a program running with nothing watching it,
/// which is the state the agent itself refuses to be in
///
/// # errors
///
/// when the connection to the client fails
pub fn serve(
    launcher: &dyn Launcher,
    input: Box<dyn Read + Send>,
    output: Box<dyn Write + Send>,
    reachable: &Reachable,
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

    let mut adapter = Adapter::new(output, interrupt, stopping, reachable.clone());
    let served = adapter.run(launcher, &commands);

    // the reader owns the client's input and ends when the client hangs up.
    // joining it means the answer to a `disconnect` is written before this
    // process goes away
    //
    // **except when the session never began.** a refused `launch` has already
    // had its error response and its `terminated` written — every send flushes,
    // so nothing of ours is outstanding — and what is left is a client that has
    // been told everything and may never speak again. waiting for one to hang
    // up is precisely the hang this avoids: under `by run` bpd *is* `$PYTHON`,
    // so bpd lingering is the whole run lingering, and the temporary build tree
    // lingering with it
    if matches!(served, Ok(Ended::NothingToDebug)) {
        // a client that tears a failed launch down itself is answered: its
        // `disconnect` is handled by the reader thread, which ends once it has.
        // so the wait is for that and nothing else, and it is **bounded** —
        // a client that says nothing costs this and not the rest of the run,
        // which is the whole failure being fixed
        let deadline = Instant::now() + FAREWELL;
        while !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(FAREWELL_POLL);
        }
        return Ok(());
    }

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
                // left running with nothing watching it — and a failure here is
                // not reportable, because the only thing left that could be
                // told is the process that has just been asked to stop existing
                stopping.store(true, Ordering::Relaxed);
                drop(end_debuggee(interrupt));
                return Ok(());
            }
            Err(error) => {
                stopping.store(true, Ordering::Relaxed);
                drop(end_debuggee(interrupt));
                return Err(error);
            }
        };

        // a response is the client answering **this adapter**, and it is queued
        // whatever it is called: a response carries the command it answers, so
        // dispatching on the name alone would read the answer to a question of
        // the adapter's as a request of the client's
        if message.kind == "response" {
            if queue.send(message).is_err() {
                return Ok(());
            }
            continue;
        }

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
                let ended = end_debuggee(interrupt);
                let mut writer = output.lock().expect(WRITING);
                writer.respond(&message, None)?;
                // a program bpd did not start cannot be ended by bpd, and the
                // client is told which of the two happened rather than reading
                // the `terminated` below as a program that has been stopped.
                // what really ends one is the agent inside it: the control
                // connection goes when this process does, and the agent will
                // not carry the debuggee on without a debugger
                if let Err(reason) = ended {
                    writer.event(
                        "output",
                        &serde_json::json!({
                            "category": "console",
                            "output": format!("bpd did not end the program: {reason}\n"),
                        }),
                    )?;
                }
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

/// end the debuggee, if one was ever started, and say why not if it was not
///
/// the one that cannot be ended is the one bpd did not **start**: a session
/// that arrived on bpd's listener, and a launch the client ran in a terminal it
/// owns. there is nothing to signal and nothing to reap, and it is refused by
/// name rather than quietly doing nothing — so the reason comes back here for
/// a caller that still has a client to tell
fn end_debuggee(interrupt: &Held) -> Result<(), String> {
    match interrupt.lock().expect(REACHING).as_mut() {
        Some(reaching) => reaching
            .terminate()
            .map_err(|error| describe(error.as_ref())),
        None => Ok(()),
    }
}

/// how long one turn of the adapter's wait sits on the program
///
/// the wait is sliced rather than open ended, and it is the price of a second
/// connection. two DAP connections serve two sessions of **one** debuggee, and
/// the engine is one object: a wait that blocked in it until the program stopped
/// would hold it for as long as the program ran, and the other connection could
/// not ask anything — including the resume a held child is waiting for
///
/// nothing is reported at the end of a slice. the client's `continue` was
/// answered before the wait began, so there is nothing outstanding for a timeout
/// to be the answer to, and the loop simply waits again. the engine already
/// polls its listener inside a wait, so this changes what is held rather than
/// what is done
const WAIT_SLICE: Duration = Duration::from_millis(25);

/// how long the adapter waits for a client to answer a `runInTerminal`
///
/// generous, because answering it means a client opening a terminal and a
/// person may be asked to allow it. bounded all the same: the whole point of
/// waiting for the answer is that a client which cannot start the program says
/// so, and a wait with no end would turn one that says nothing into a session
/// that hangs with no cause
const TERMINAL_PATIENCE: Duration = Duration::from_secs(30);

/// the field a `startDebugging` configuration names the session in
///
/// bpd's own, because DAP has no field for one: a session **is** a connection
/// there, so what the spec provides is this reverse request and a configuration
/// the adapter fills in
const SESSION_FIELD: &str = "bpdSession";

const WRITING: &str =
    "nothing panics holding the client's output: every path through it is a write";
const REACHING: &str =
    "nothing panics holding the interrupt: every path through it is a send or a kill";

/// how the DAP adapter answers one client
struct Adapter {
    output: Output,
    interrupt: Held,
    stopping: Arc<AtomicBool>,
    /// where a second session of this debuggee could reach this adapter
    ///
    /// read once, when a launch asks for the program's forked children to be
    /// debugged: a `startDebugging` reverse request is only honest if the
    /// session it asks for can arrive
    reachable: Reachable,
    /// what the client said it could do, in `initialize`
    client: ClientCan,
    /// what arrived while the adapter was waiting for a client's answer
    ///
    /// the answer to a `runInTerminal` is the one thing the adapter waits for
    /// on the client's own channel, and anything else that arrives in that
    /// window is set aside rather than dropped or answered out of turn. a
    /// client is entitled to send whatever it likes; what it is not entitled to
    /// is having it silently disappear
    deferred: Vec<Incoming>,
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
    /// whether each held stop was announced as holding the whole program
    ///
    /// remembered rather than recomputed, because a `stopped` event sent for a
    /// stop that is already held — which is what a `goto` and a `restartFrame`
    /// produce — has to say the same thing about the other threads as the event
    /// that announced it. asking again would be asking the world to stop a
    /// second time, and answering `false` because nothing was asked would tell
    /// the client threads are running that this stop is holding
    worlds: BTreeMap<u64, bool>,
    breakpoints: FileBreakpoints,
}

impl Adapter {
    fn new(
        output: Output,
        interrupt: Held,
        stopping: Arc<AtomicBool>,
        reachable: Reachable,
    ) -> Self {
        Self {
            output,
            interrupt,
            stopping,
            reachable,
            client: ClientCan::default(),
            deferred: Vec::new(),
            session: None,
            configuration: None,
            configured: false,
            exited: false,
            handles: Handles::default(),
            threads: ThreadIds::default(),
            announced: Vec::new(),
            reasons: BTreeMap::new(),
            worlds: BTreeMap::new(),
            breakpoints: FileBreakpoints::default(),
        }
    }

    /// answer requests, and wait for the program whenever nothing is held
    fn run(
        &mut self,
        launcher: &dyn Launcher,
        commands: &Receiver<Incoming>,
    ) -> Result<Ended, crate::wire::Error> {
        loop {
            if self.stopping.load(Ordering::Relaxed) {
                return Ok(Ended::WithTheClient);
            }

            if self.waiting() {
                let mut events = Events::new(&self.output);
                let waited = self
                    .session
                    .as_mut()
                    .expect("the adapter only waits once a program has been launched")
                    .dispatch(
                        // a wait is about the program rather than about a held
                        // thread, so it names no session and is answered by the
                        // only one this connection serves
                        Addressed::unnamed(Request::Wait {
                            deadline: Some(WAIT_SLICE),
                        }),
                        &mut events,
                    );
                let written = events.finish();
                let outcome = match (waited, written) {
                    (_, Err(error)) => Err(Aborted::Wire(error)),
                    (Ok(Response::Ran(running)), Ok(joined)) => self
                        .tell_the_client_to_start(&joined)
                        .and_then(|()| self.report(running)),
                    (Ok(other), Ok(_)) => unreachable!("a wait was answered with {other:?}"),
                    (Err(error), Ok(_)) => {
                        if self.stopping.load(Ordering::Relaxed) {
                            return Ok(Ended::WithTheClient);
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

            // what was set aside while the adapter waited for a client's answer
            // to a reverse request, in the order it arrived. before the channel,
            // because a message that arrived first is answered first
            let message = if self.deferred.is_empty() {
                let Ok(message) = commands.recv() else {
                    return Ok(Ended::WithTheClient);
                };
                message
            } else {
                self.deferred.remove(0)
            };
            let handled = self.handle(launcher, &message, commands);
            match handled {
                Ok(()) => {}
                Err(Aborted::Refuse(reason)) => {
                    // the refusal goes out **first**, always. it carries the
                    // only account of why, and it is the string the client puts
                    // in front of a person
                    self.refuse(&message, &reason)?;

                    // and then, for the two requests a session's existence
                    // depends on, the session is over. refusing `variables`
                    // leaves a session that still exists; refusing `launch`
                    // leaves one that never began, and every other exit from
                    // this adapter sends `terminated` while that one did not —
                    // so a client with nothing to react to sat on a live
                    // connection to a program that was never started
                    //
                    // scoped to these two rather than to `Aborted::Refuse`,
                    // which every refused request in a live session goes
                    // through: terminating on those would end a session because
                    // one `evaluate` was malformed
                    if begins_a_session(&message) {
                        // `terminated`, and not `exited`. no process was ever
                        // started, so there is no exit code and `exited` would
                        // be inventing one
                        self.event("terminated", &serde_json::json!({}))
                            .map_err(|aborted| match aborted {
                                Aborted::Wire(error) => error,
                                Aborted::Refuse(reason) => {
                                    unreachable!("an event cannot be refused, and was: {reason}")
                                }
                            })?;
                        return Ok(Ended::NothingToDebug);
                    }
                }
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

    fn handle(
        &mut self,
        launcher: &dyn Launcher,
        message: &Incoming,
        commands: &Receiver<Incoming>,
    ) -> Answered {
        // the client answering a reverse request. the one that is waited for —
        // `runInTerminal` — was taken off this channel by the launch that sent
        // it, so anything reaching here is a `startDebugging`, and there is
        // nothing to do about that answer: the session it asked for arrives on
        // a connection of its own, or it does not and the child stays held,
        // which is a fact the client already has. what must not happen is this
        // being refused as an unknown request, which is what every other `type`
        // gets
        if message.kind == "response" {
            return Ok(());
        }
        if message.kind != "request" {
            return Err(Aborted::Refuse(format!(
                "a debug adapter is sent requests, and this was a `{}`",
                message.kind
            )));
        }

        match message.command.as_deref() {
            Some("initialize") => self.initialize(message),
            Some("launch") => self.launch(launcher, message, commands),
            Some("attach") => self.attach(launcher, message),
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
            Some("gotoTargets") => self.goto_targets(message),
            Some("goto") => self.goto(message),
            Some("bpd/understands") => self.understands(message),
            Some("restartFrame") => self.restart_frame(message),
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
            // `variables` says what a scope holds and has nowhere to put how
            // long that stays true. an editor drawing what a branch will do
            // needs the second half, and there is no field on a DAP `Variable`
            // that could carry it
            Some("bpd/facts") => self.facts(message),
            // DAP's own `restart` throws the process away, which is the opposite
            // of this. an editor is where the file gets edited, so an editor is
            // where a replacement is worth offering — and the parity rule does
            // not let it be an agent's alone
            Some("bpd/record") => self.record(message),
            Some("bpd/trail") => self.trail(message),
            Some("bpd/retainers") => self.retainers(message),
            Some("bpd/replaceCode") => self.replace_code(message),
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

        // the two **client** capabilities, and the only ones this adapter
        // reads. each decides whether something can be offered at all, and each
        // is checked at `launch` rather than discovered later: a client that
        // cannot start a session this adapter asks for would leave a debugged
        // child held for ever, and one that cannot run a command line in a
        // terminal would leave a launch waiting for an agent nothing started
        self.client = ClientCan {
            start_debugging: message.arguments["supportsStartDebuggingRequest"]
                == serde_json::Value::Bool(true),
            run_in_terminal: message.arguments["supportsRunInTerminalRequest"]
                == serde_json::Value::Bool(true),
            // carried across rather than reset. `bpd/understands` is its own
            // request precisely so it does not have to be part of this one, and
            // nothing says a client must send them in one order
            understands: std::mem::take(&mut self.client.understands),
        };

        self.respond(message, Some(capabilities()))?;
        Ok(())
    }

    /// take up a session of a debuggee this adapter's process already holds
    ///
    /// what a connection that arrived because of a `startDebugging` reverse
    /// request sends. it is not PEP 768 attaching — nothing is injected into
    /// anything — and the two are told apart by the `bpdSession` the reverse
    /// request put in the configuration it handed the client
    fn attach(&mut self, launcher: &dyn Launcher, message: &Incoming) -> Answered {
        let Some(named) = message.arguments[SESSION_FIELD].as_u64() else {
            return Err(Aborted::Refuse(
                "bpd cannot attach to a running process. attaching is PEP 768, which \
                 needs cpython 3.14, and bpd refuses rather than injecting by another \
                 route — use a `launch` configuration.\n\nan `attach` that names \
                 `bpdSession` is a different thing: it takes up a session this adapter \
                 already holds, which is what the `startDebugging` reverse request asks \
                 a client to start"
                    .to_string(),
            ));
        };
        if self.session.is_some() {
            return Err("this connection already has a program".into());
        }

        let Started::Stopped(session) = launcher.attach(named).map_err(|error| failed(&error))?
        else {
            unreachable!("a session that is already held cannot have exited before stopping")
        };
        let reaching = session.interrupt().map_err(|error| failed(&error))?;
        *self.interrupt.lock().expect(REACHING) = Some(reaching);
        self.session = Some(session);
        // the configuration is the one the reverse request handed the client,
        // which carries no program: this connection did not start anything
        self.configuration = Some(serde_json::from_value(message.arguments.clone()).map_err(
            |error| Aborted::Refuse(format!("the attach configuration is not usable: {error}")),
        )?);
        self.respond(message, None)?;
        self.event("initialized", &serde_json::json!({}))?;
        Ok(())
    }

    fn launch(
        &mut self,
        launcher: &dyn Launcher,
        message: &Incoming,
        commands: &Receiver<Incoming>,
    ) -> Answered {
        if self.session.is_some() {
            return Err("this session already has a program running".into());
        }

        let configuration: Configuration = serde_json::from_value(message.arguments.clone())
            .map_err(|error| {
                Aborted::Refuse(format!("the launch configuration is not usable: {error}"))
            })?;

        if configuration.debug_children {
            self.can_debug_children()?;
        }

        if configuration.no_debug {
            return Err(Aborted::Refuse(
                "`noDebug` asks for the program to be run without a debugger. bpd has \
                 no such path — its agent is how a program is launched at all — so \
                 running it from here would debug it anyway. run it without bpd"
                    .to_string(),
            ));
        }

        let started = match configuration.console.kind() {
            Some(kind) => self.start_in_a_terminal(launcher, &configuration, kind, commands)?,
            None => {
                let program = Arc::new(Console {
                    output: Arc::clone(&self.output),
                });
                launcher
                    .launch(&configuration, program)
                    .map_err(|error| failed(&error))?
            }
        };

        match started {
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

    /// have the client start the program in a terminal it owns
    ///
    /// the `runInTerminal` reverse request, and the only honest way for a
    /// debuggee under this adapter to have a terminal: an adapter cannot make
    /// one, and a pseudo-terminal in front of a debug console would be
    /// `isatty()` answering `True` about a thing that is not a terminal — see
    /// [launching a debuggee](../../../docs/development/launching.md#runinterminal-the-client-owns-the-terminal-and-starts-the-program)
    ///
    /// what is handed over is exactly what bpd would have spawned, and the
    /// agent connects **back** as it always does. so the difference is the last
    /// step of a launch rather than a second kind of launch — and everything
    /// that follows from bpd not being the parent is already the rule for a
    /// session that arrived on its listener
    ///
    /// the answer is **waited for**. a client that could not run the command
    /// line says so, and a launch that went on to wait for the agent anyway
    /// would report a timeout thirty seconds later with the cause already in
    /// hand
    fn start_in_a_terminal(
        &mut self,
        launcher: &dyn Launcher,
        configuration: &Configuration,
        kind: &'static str,
        commands: &Receiver<Incoming>,
    ) -> Result<Started, Aborted> {
        if !self.client.run_in_terminal {
            return Err(Aborted::Refuse(
                "`console` asks for the program to run in a terminal, and that is \
                 the `runInTerminal` reverse request — which this client did not \
                 say it supports in `initialize`. an adapter cannot make a \
                 terminal: the client owns the one the program would run on, so \
                 a client that cannot be asked would leave this launch waiting \
                 for an agent nothing was going to start. take `console` out and \
                 the program runs on the debug console, which is what bpd does \
                 without it — its output arrives as `output` events, its stdin \
                 is `/dev/null`, and `isatty()` is `False`"
                    .to_string(),
            ));
        }

        let output = Arc::clone(&self.output);
        let title = format!("bpd: {}", configuration.program.display());
        // set aside rather than dropped or answered out of turn: a client is
        // entitled to send whatever it likes while it is being asked something
        let mut deferred = Vec::new();
        // a connection that failed is not a refusal, and the closure below can
        // only report one. so it is kept and raised out here, where the
        // difference between "the client said no" and "there is no client" is
        // still expressible
        let mut wire = None;

        let started = launcher.launch_in_terminal(configuration, &mut |invocation| {
            let environment: serde_json::Map<String, serde_json::Value> = invocation
                .env
                .iter()
                .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
                .collect();
            let asked = output
                .lock()
                .expect(WRITING)
                .request(
                    "runInTerminal",
                    &serde_json::json!({
                        "kind": kind,
                        "title": title,
                        "cwd": invocation.directory,
                        "args": invocation.arguments,
                        "env": environment,
                    }),
                )
                .map_err(|error| {
                    let said = describe(&error);
                    wire = Some(error);
                    Failed::from(said)
                })?;

            let deadline = Instant::now() + TERMINAL_PATIENCE;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                // the two ways there is no answer are not the same thing, and a
                // client reading the wrong one goes looking for the wrong
                // problem: one is a client that is still there
                let arrived = commands.recv_timeout(left).map_err(|why| {
                    Failed::from(match why {
                        std::sync::mpsc::RecvTimeoutError::Timeout => format!(
                            "the client was asked to run the program in a terminal \
                             and did not answer within {}s, so bpd cannot tell \
                             whether it was ever started",
                            TERMINAL_PATIENCE.as_secs()
                        ),
                        std::sync::mpsc::RecvTimeoutError::Disconnected => {
                            "the client hung up while it was being asked to run the \
                             program in a terminal, so bpd cannot tell whether it \
                             was ever started"
                                .to_string()
                        }
                    })
                })?;
                if arrived.request_seq != Some(asked) {
                    deferred.push(arrived);
                    continue;
                }
                if arrived.success == Some(true) {
                    return Ok(());
                }
                // the client's own words for why, because they are the only
                // account of what happened in a terminal bpd cannot see
                return Err(Failed::from(format!(
                    "the client did not run the program in a terminal: {}",
                    arrived
                        .message
                        .as_deref()
                        .unwrap_or("it answered the `runInTerminal` request without saying why")
                )));
            }
        });

        self.deferred.append(&mut deferred);
        if let Some(error) = wire {
            return Err(Aborted::Wire(error));
        }
        started.map_err(|error| failed(&error))
    }

    fn configuration_done(&mut self, message: &Incoming) -> Answered {
        self.configured = true;
        self.respond(message, None)?;

        // before the program runs a line, because the handler that acts on it
        // runs inside `os.fork()` and a program can fork in its first statement.
        // only when it was asked for: the agent's default is off, and a request
        // saying so on every session would be a round trip for nothing
        if self.configuration().debug_children {
            match self.ask(Request::DebugChildren { on: true })? {
                Response::DebuggingChildren { on: true } => {}
                // read back off the agent rather than assumed. a client told
                // this took would wait for child sessions that never arrive
                Response::DebuggingChildren { on: false } => {
                    return Err(Aborted::Refuse(
                        "the debuggee did not take `debugChildren`, and a forked \
                         child of it will not be debugged"
                            .to_string(),
                    ));
                }
                other => unreachable!("debugging children was answered with {other:?}"),
            }
        }

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
            // `after` names a file and a line rather than an id, because the
            // adapter's ids are re-minted on every `setBreakpoints` and one a
            // client read off an earlier response is already stale
            let after = match &requested["after"] {
                serde_json::Value::Null => None,
                after => {
                    let path = after["path"].as_str().ok_or(
                        "a breakpoint's `after` needs `path`, the file of the \
                         breakpoint it waits for. it is a file and a line rather \
                         than an id because this adapter re-mints breakpoint ids \
                         on every `setBreakpoints`",
                    )?;
                    let line = after["line"]
                        .as_u64()
                        .and_then(|line| u32::try_from(line).ok())
                        .ok_or("a breakpoint's `after` needs `line`, a number")?;
                    Some((PathBuf::from(path), line))
                }
            };
            wanted.push(Wanted {
                line,
                condition: requested["condition"].as_str().map(ToString::to_string),
                log: requested["logMessage"].as_str().map(ToString::to_string),
                after,
            });
        }

        let mine = self.breakpoints.replace(&file, &wanted);
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
                    "source": mapped_source_of(&frame.file, frame.mapping.as_ref()),
                })
            })
            .collect();

        // said on the console rather than among the frames. DAP's `stackFrames`
        // is a call chain and nothing else — a client draws it as one — so a
        // scheduling frame put in there would be an editor showing a call that
        // never happened. the console is where bpd's own words go
        if stack.in_a_task && stack.scheduled_by.is_empty() {
            self.event(
                "output",
                &serde_json::json!({
                    "category": "console",
                    "output": format!("{}\n", bpd_core::TASK_NOT_SEEN),
                }),
            )?;
        }
        for (at, scheduled) in stack.scheduled_by.iter().enumerate() {
            let lead = if at == 0 {
                "this stack is inside an asyncio task, scheduled at"
            } else {
                "                                     called from"
            };
            self.event(
                "output",
                &serde_json::json!({
                    "category": "console",
                    "output": format!(
                        "{lead} {}:{} in {}\n",
                        scheduled.file, scheduled.line, scheduled.function
                    ),
                }),
            )?;
        }
        // the frames a bounded record drops are the outermost, so a client
        // shown a cut one reads the last line above as where the task was
        // scheduled from. it is not, and only bpd knows that
        if stack.scheduling_cut {
            self.event(
                "output",
                &serde_json::json!({
                    "category": "console",
                    "output": "and from above that, which this record does not reach — it keeps \
                               the innermost frames only, so the outermost line above is not where \
                               the task was scheduled from\n",
                }),
            )?;
        }

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

    /// what is provable about a frame's names, and for how long
    ///
    /// the names come from the client because the client is the one reading
    /// source: it knows which names the region it is analysing mentions, and
    /// every other local in the frame is a read nobody asked for
    fn facts(&mut self, message: &Incoming) -> Answered {
        let reference = message.arguments["frameId"]
            .as_i64()
            .ok_or("a `bpd/facts` request arrived with no `frameId`")?;
        let frame = match self.handles.get(reference) {
            Some(Handle::Frame(frame) | Handle::TemplateFrame(frame)) => *frame,
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names {other:?}, not a frame"
                )));
            }
            None => return Err(stale(reference)),
        };

        let names: Vec<String> = message.arguments["names"]
            .as_array()
            .ok_or(
                "a `bpd/facts` request needs `names`, the names to prove things about. it is a \
                 list rather than a whole scope because the client is the one reading the source \
                 that mentions them",
            )?
            .iter()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect();

        // absent is the default rather than an error: `limit` bounds an answer
        // and a client that does not care what it costs is not making a mistake
        let limit: bpd_core::Limit = match message.arguments.get("limit") {
            Some(limit) => serde_json::from_value(limit.clone()).map_err(|error| {
                Aborted::Refuse(format!(
                    "this is not a fact limit: {error}. it takes `text`, how much of a value one \
                     fact may carry, and `depth`, how many segments of a dotted path to follow"
                ))
            })?,
            None => bpd_core::Limit::default(),
        };

        let facts = match self.ask(Request::Facts {
            frame,
            names,
            limit,
        })? {
            Response::Facts(facts) => facts,
            other => unreachable!("a fact request was answered with {other:?}"),
        };
        let body = serde_json::to_value(&facts)
            .expect("facts are built from types whose serde is derived");
        self.respond(message, Some(body))
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

    /// start or stop recording where the program goes
    ///
    /// deliberately **not** DAP's `stepBack`. that request means "put the
    /// program back where it was", and a trail says only where it went — a
    /// client that drew this as `stepBack` would be offering to undo something
    /// bpd cannot undo. `supportsStepBack` stays unadvertised for that reason
    fn record(&mut self, message: &Incoming) -> Answered {
        let on = match &message.arguments["on"] {
            serde_json::Value::Bool(on) => *on,
            other => {
                return Err(Aborted::Refuse(format!(
                    "a `bpd/record` needs `on`, a boolean, and this was {other}. \
                     turning recording on costs about four times a bare run, so \
                     it is not something to start on a guess"
                )));
            }
        };

        // how much of each step to keep. absent is the cheap one, because the
        // depths differ by hundreds of times a bare run and a client that said
        // nothing has not asked to pay for the expensive one
        let depth = match message.arguments.get("depth") {
            None => bpd_core::Depth::default(),
            Some(named) => serde_json::from_value(named.clone()).map_err(|error| {
                Aborted::Refuse(format!(
                    "this is not a recording depth: {error}. it takes `where`, \
                     which keeps the location, `values`, which also keeps what \
                     the frame held, and `frame` and `locals`, which keep \
                     neither and exist so the cost of the other two can be \
                     told apart"
                ))
            })?,
        };

        let answer = match self.ask(Request::Record { on, depth })? {
            Response::Recording { on, held, dropped } => {
                serde_json::json!({ "recording": on, "held": held, "dropped": dropped })
            }
            other => unreachable!("a recording was answered with {other:?}"),
        };
        // on the console because it is a mode with a price, and a person who
        // left it on would otherwise have nothing to remind them
        if on {
            self.event(
                "output",
                &serde_json::json!({
                    "category": "important",
                    "output": format!(
                        "bpd is recording at depth `{depth}`. every line is watched \
                         while this is on, and what that costs depends on the depth \
                         — see the trail documentation, which carries the measured \
                         figures\n"
                    ),
                }),
            )?;
        }
        self.respond(message, Some(answer))?;
        Ok(())
    }

    /// the window of where the program has been
    fn trail(&mut self, message: &Incoming) -> Answered {
        let went = match self.ask(Request::Trail)? {
            Response::Trail(went) => went,
            other => unreachable!("a trail was answered with {other:?}"),
        };
        // the window's edge, said where a person is looking. a trail whose start
        // is not where the recording began reads as the whole run otherwise
        if went.dropped > 0 {
            self.event(
                "output",
                &serde_json::json!({
                    "category": "important",
                    "output": format!(
                        "the trail holds the last {} steps and {} fell out of the \
                         window before them, so its oldest entry is not where the \
                         recording began\n",
                        went.went.len(),
                        went.dropped
                    ),
                }),
            )?;
        }
        let body = serde_json::to_value(&went)
            .expect("a trail is built from types whose serde is derived");
        self.respond(message, Some(body))?;
        Ok(())
    }

    /// what is holding an object, and how
    ///
    /// a custom request because DAP has none and will not grow one: its model of
    /// state is a tree walked **downwards** from a frame, and this is the
    /// question asked upwards from an object. `variablesReference` cannot
    /// express it — there is no handle for "the things that point at this"
    fn retainers(&mut self, message: &Incoming) -> Answered {
        let reference = message.arguments["frameId"]
            .as_i64()
            .ok_or("a `bpd/retainers` needs `frameId`, the frame to evaluate in")?;
        let expression = message.arguments["expression"].as_str().ok_or(
            "a `bpd/retainers` needs `expression`, naming the object to ask \
             about. an object has no id of its own that outlives being asked \
             about, so an expression is the only way to point at one",
        )?;

        // a python frame only. a template frame resolves its syntax to a **new**
        // value — `{{ user.name }}` builds a string — so a walk would find what
        // the resolution just made, which is the agent's refusal to give
        let frame = match self.handles.get(reference) {
            Some(Handle::Frame(frame)) => *frame,
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names {other:?}, not a python frame"
                )));
            }
            None => return Err(stale(reference)),
        };

        let found = match self.ask(Request::Retainers {
            frame,
            expression: expression.to_string(),
        })? {
            Response::Retainers(found) => found,
            other => unreachable!("a retainer walk was answered with {other:?}"),
        };

        // on the console as well as in the body, because the coverage is the
        // half a person acts on and a debug console is where they are looking.
        // a list of holders read without it answers a narrower question than the
        // one that was asked
        self.event(
            "output",
            &serde_json::json!({
                "category": "console",
                "output": format!(
                    "what holds {}: {} found. {}\n",
                    found.of,
                    found.found.len(),
                    found.coverage.not_python
                ),
            }),
        )?;

        let body = serde_json::to_value(&found)
            .expect("a retainer answer is built from types whose serde is derived");
        self.respond(message, Some(body))?;
        Ok(())
    }

    /// replace the code the process is running for one file with what is on disk
    ///
    /// a custom request, because DAP has none of its own: its `restart` throws
    /// the process away and starts another, and the whole point of this is that
    /// the process stays. the answer is `bpd_core::Replaced` whole — an editor
    /// given only "yes" cannot show what is now different about the process, and
    /// one given only "no" cannot show which of the user's edits to undo
    fn replace_code(&mut self, message: &Incoming) -> Answered {
        let file = message.arguments["file"].as_str().ok_or_else(|| {
            Aborted::Refuse(
                "a `bpd/replaceCode` needs `file`, the path of the file whose \
                 code to replace, on the debuggee's own filesystem"
                    .to_string(),
            )
        })?;

        // an optional argument, defaulting off, because the default is the
        // guarantee: a replacement under a live frame leaves the process
        // running two versions of one function, and a client that had not asked
        // for that must not be given it by an adapter's convenience
        let even_under_a_live_frame = match message.arguments.get("evenUnderALiveFrame") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(asked)) => *asked,
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "`evenUnderALiveFrame` is a boolean and this was {other}. it \
                     trades the guarantee that the process never runs two \
                     versions of one function for a report of every frame that \
                     will, and guessing which was meant is not a trade bpd can \
                     make for a client"
                )));
            }
        };

        let replaced = match self.ask(Request::ReplaceCode {
            file: file.into(),
            even_under_a_live_frame,
        })? {
            Response::Replaced(replaced) => replaced,
            other => unreachable!("a code replacement was answered with {other:?}"),
        };

        // every refusal, as its own sentence, on the `output` stream the client
        // already shows a user — the body carries the same facts as data, and a
        // user watching a debug console is the one who has to decide what to
        // change about their edit
        if let bpd_core::Replacement::Refused { because } = &replaced.outcome {
            for reason in because {
                self.event(
                    "output",
                    &serde_json::json!({
                        "category": "important",
                        "output": format!("bpd did not replace the code: {reason}\n"),
                    }),
                )?;
            }
        }

        // and the other way round: a replacement that **was** applied under a
        // live frame is the one case where succeeding costs something, and the
        // cost goes where a refusal's reason goes. a user who asked for this and
        // then read an unqualified success would have been told the process is
        // on one version of the code when it is on two
        if let bpd_core::Replacement::Applied { still_running, .. } = &replaced.outcome {
            for running in still_running {
                self.event(
                    "output",
                    &serde_json::json!({
                        "category": "important",
                        "output": format!("bpd replaced the code under a live frame: {running}\n"),
                    }),
                )?;
            }
        }

        let body = serde_json::to_value(&replaced)
            .expect("a replacement is built from types whose serde is derived");
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
            Handle::Goto { frame, line } => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names line {line} of {frame}, which is a place \
                     to move to rather than a value. it is what a `goto` \
                     carries"
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

    /// the places a `goto` could move a held thread to
    ///
    /// a target is minted for the location the client asked about, and only when
    /// that location is in the file a held thread is **executing**. the file
    /// check is the whole value of this request: `goto` carries a target rather
    /// than a line precisely because a line number means nothing on its own, and
    /// cpython would take the same number against whatever file the frame
    /// happens to be running
    ///
    /// offering a target is not a claim that the move will happen. whether a
    /// line can be reached from where the frame is is cpython's answer, given
    /// when the move is made — and the label says so rather than implying the
    /// list has been checked
    fn goto_targets(&mut self, message: &Incoming) -> Answered {
        let file = message.arguments["source"]["path"].as_str().ok_or(
            "a `gotoTargets` request has no `source.path`, and a line means \
             nothing without the file it is in",
        )?;
        let file = PathBuf::from(file);
        let line = message.arguments["line"]
            .as_u64()
            .and_then(|line| u32::try_from(line).ok())
            .ok_or("a `gotoTargets` request arrived with no line")?;

        let mut targets = Vec::new();
        for stop in self.announced.clone() {
            let Some(frame) = self.executing_frame(stop.stop)? else {
                continue;
            };
            if !same_file(&file, &frame.file) {
                continue;
            }
            let id = self.handles.add(Handle::Goto {
                frame: frame.id,
                line,
            });
            targets.push(serde_json::json!({
                "id": id,
                "label": format!(
                    "line {line} of `{}`, which stop {} is executing — whether \
                     the frame can be moved there is cpython's answer, given \
                     when the move is made",
                    frame.name(),
                    stop.stop,
                ),
                "line": line,
            }));
        }

        self.respond(message, Some(serde_json::json!({ "targets": targets })))?;
        Ok(())
    }

    /// move a held thread's executing frame to a target minted for it
    fn goto(&mut self, message: &Incoming) -> Answered {
        let target = message.arguments["targetId"]
            .as_i64()
            .ok_or("a `goto` request arrived with no `targetId`")?;
        let stop = self.stop_of(message)?;

        let (frame, line) = match self.handles.get(target) {
            Some(Handle::Goto { frame, line }) => (*frame, *line),
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "{target} names {other:?}, not a place to move to. a target \
                     comes from `gotoTargets`"
                )));
            }
            None => return Err(stale(target)),
        };
        // a target is minted against one frame of one stop. using it on another
        // thread would move a frame the client did not look at
        if frame.stop != stop {
            return Err(Aborted::Refuse(format!(
                "{target} was minted for stop {}, and this `goto` names the \
                 thread stop {stop} is holding. a target is a place in one \
                 frame — ask `gotoTargets` again for this thread",
                frame.stop
            )));
        }

        let jumped = match self.ask(Request::SetNextStatement { frame, line })? {
            Response::Jumped(jumped) => jumped,
            other => unreachable!("a jump was answered with {other:?}"),
        };
        self.respond(message, None)?;
        self.moved(stop, &jumped, "goto")
    }

    /// which of bpd's own events this client reads
    ///
    /// bpd narrates what it noticed on the console — the locals a jump bound to
    /// `None`, the breakpoints a destination line will not fire for this pass —
    /// because for most clients that is the only channel those facts have. The
    /// same facts also go out as data on `bpd/moved`, and a client that reads
    /// them there and *also* shows the narration shows everything twice.
    ///
    /// so a client says what it reads and bpd stops saying it in prose. it is a
    /// request rather than a client capability in `initialize` because a client
    /// does not always own that message — an editor whose debug support builds
    /// `initialize` itself has nowhere to put a field, and can still send this.
    ///
    /// unknown names are kept rather than refused: they name events a later bpd
    /// may send, and refusing would make a client that is ahead unusable
    /// against a bpd that is behind
    fn understands(&mut self, message: &Incoming) -> Answered {
        let events = message.arguments["events"]
            .as_array()
            .ok_or("a `bpd/understands` request needs an `events` array")?;
        self.client.understands = events
            .iter()
            .filter_map(|event| event.as_str().map(str::to_owned))
            .collect();
        self.respond(message, None)
    }

    /// re-enter a frame from the top
    ///
    /// DAP's own wording for this request has it discard the frames above the
    /// one named. there is no mechanism for that — the refusal for a frame that
    /// is not the executing one says so — and what this does is exactly what it
    /// says: the executing frame runs again from its first line, with what its
    /// parameters hold now
    fn restart_frame(&mut self, message: &Incoming) -> Answered {
        let reference = message.arguments["frameId"]
            .as_i64()
            .ok_or("a `restartFrame` request arrived with no `frameId`")?;
        // either kind: a template frame is refused by the session, which names
        // the python frame underneath and why a synthesised frame has no
        // instruction pointer to move
        let frame = match self.handles.get(reference) {
            Some(Handle::Frame(frame) | Handle::TemplateFrame(frame)) => *frame,
            Some(other) => {
                return Err(Aborted::Refuse(format!(
                    "{reference} names {other:?}, not a frame"
                )));
            }
            None => return Err(stale(reference)),
        };

        let jumped = match self.ask(Request::RestartFrame { frame })? {
            Response::Jumped(jumped) => jumped,
            other => unreachable!("a restart was answered with {other:?}"),
        };
        self.respond(message, None)?;
        self.moved(frame.stop, &jumped, "restart")
    }

    /// the frame a stop's thread is **executing**, which is the only one that
    /// can move
    ///
    /// the topmost python frame rather than the topmost frame: a django template
    /// frame is synthesised above the `Node.render_annotated` frame that renders
    /// it, and the interpreter has no frame for it at all
    fn executing_frame(&mut self, stop: u64) -> Result<Option<bpd_core::Frame>, Aborted> {
        // two, because a template frame sits above the python frame that is
        // really running and a client may be looking at either
        let stack = match self.ask(Request::Stack { stop, top: Some(2) })? {
            Response::Stack(stack) => stack,
            other => unreachable!("a stack walk was answered with {other:?}"),
        };
        Ok(stack
            .frames
            .into_iter()
            .find(|frame| matches!(frame.kind, bpd_core::FrameKind::Python { .. })))
    }

    /// tell the client where a frame is now, and what the move did to it
    ///
    /// DAP answers a `goto` and a `restartFrame` with an empty response and then
    /// a `stopped` event, because the thread was never resumed and the client
    /// has to re-read the stack to see where it is
    ///
    /// the two facts that have nowhere to go in that event go to the console.
    /// neither is decoration: a breakpoint on the destination line does not fire
    /// for this pass, and a local that held nothing holds `None` now — a client
    /// that was not told would watch its own breakpoint be passed over
    fn moved(&mut self, stop: u64, jumped: &bpd_core::Jumped, reason: &str) -> Answered {
        let Some(thread) = self
            .announced
            .iter()
            .find(|held| held.stop == stop)
            .map(|held| self.threads.of(held.thread))
        else {
            // the stop ended while the jump was in flight, which means the
            // thread ran on. there is nothing left to report a position for, and
            // `announce` has already told the client the stop is gone
            return Ok(());
        };

        let description = match &jumped.outcome {
            bpd_core::Jump::Moved {
                from,
                bound_to_none,
                unannounced,
            } => {
                let narrate = !self.client.understands.contains(MOVED_EVENT);
                if narrate && !unannounced.is_empty() {
                    self.say(&format!(
                        "stop {stop}: breakpoint(s) {unannounced:?} are on line \
                         {} and will not fire for this pass — no line event is \
                         delivered for the line a jump moves to. they are still \
                         set, and fire the next time that line runs\n",
                        jumped.at.line
                    ))?;
                }
                if narrate && !bound_to_none.is_empty() {
                    self.say(&format!(
                        "stop {stop}: {bound_to_none:?} held nothing before the \
                         move and hold `None` now — cpython binds every unbound \
                         local of a frame as part of a jump\n"
                    ))?;
                }
                format!(
                    "moved from line {from} to {}. the lines between were not \
                     executed, and neither was the cleanup of any block the move \
                     left",
                    jumped.at
                )
            }
            bpd_core::Jump::Refused { wanted, error } => {
                if !self.client.understands.contains(MOVED_EVENT) {
                    self.say(&format!(
                        "stop {stop}: cpython refused the move to line {wanted} — \
                         {error}\n"
                    ))?;
                }
                format!("still at {}: {error}", jumped.at)
            }
        };

        // the same facts as data, for a client that can act on them
        //
        // the console lines above stay, and that is an addition rather than
        // duplication left in by accident: they are what a client which has
        // never heard of this event still shows a person, and every front end
        // other than one taught this is such a client. both are written from the
        // same values in the same function, so neither can drift from the other
        //
        // a custom **event** rather than a request a client would have to send:
        // these are things bpd noticed while doing what it was asked, and a fact
        // that has to be asked for is a fact a client which does not ask never
        // learns. `jumped` is serialised whole rather than picked apart, so a
        // reader gets `bound_to_none` and `unannounced` under `moved`, or
        // `wanted` and cpython's own `error` under `refused`, without this
        // having to know which. the name is namespaced for the reason
        // `bpd/facts` is
        self.event(
            MOVED_EVENT,
            &serde_json::json!({
                "stop": stop,
                "threadId": thread,
                "jumped": jumped,
            }),
        )?;

        self.event(
            "stopped",
            &serde_json::json!({
                "reason": reason,
                "description": description,
                "text": description,
                "threadId": thread,
                // what this stop was announced with. the thread was never
                // resumed, so nothing about the other threads changed — and a
                // second event saying otherwise would contradict the first
                "allThreadsStopped": self.worlds.get(&stop).copied().unwrap_or(false),
                "preserveFocusHint": false,
            }),
        )
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
            | Running::Ended { rebound }
            | Running::Finishing { rebound, .. }
            | Running::StillRunning { rebound, .. } => rebound.clone(),
        };
        self.rebound(&rebound)?;

        match running {
            // only a wait that carried a deadline can be answered with this,
            // and this adapter never sends one — see `coverage::reach_of`
            // the slice passed and the program is running. it is not a stop and
            // nothing is said about it: the client's `continue` was answered
            // before the wait began, so there is nothing outstanding for a
            // timeout to be the answer to. the loop goes round and waits again
            Running::StillRunning { .. } => Ok(()),
            Running::Stopped { .. } => self.announce(),
            Running::Exited { status, output, .. } => {
                self.exited = true;
                // said **before** the exit rather than after it, because it is
                // about everything that follows: a client reads `exited` as the
                // end of the program's output, and a line arriving after one it
                // was not warned about is a line it will attribute to a run that
                // had already finished
                if output == Forwarded::StillHeldOpen {
                    // what bpd waited for and did not get, rather than the
                    // reason it did not get it. end-of-file needs every write
                    // end of the pipe closed, and a child that outlived the
                    // program is what usually holds one — but bpd did not watch
                    // the descriptor and naming a cause it did not see would be
                    // the invention this whole line exists to avoid
                    self.say(
                        "the program has exited and its output is still being written: \
                         bpd waited for the stream it wrote to and it did not end, \
                         which is what a child outliving the program looks like. \
                         anything after this line was written by whatever still holds \
                         that stream, and not by the program that just ended\n",
                    )?;
                }
                self.event(
                    "exited",
                    &serde_json::json!({ "exitCode": exit_code(status) }),
                )?;
                self.event("terminated", &serde_json::json!({}))
            }
            // `terminated` and **no `exited`**, which is the protocol saying
            // exactly what is true. DAP's `exited` event carries an `exitCode`
            // and there is none: bpd did not start this process, is not its
            // parent, and never learns what it exited with. sending the event
            // with a zero in it would be the adapter inventing the one field it
            // is for
            Running::Ended { .. } => {
                self.exited = true;
                self.say(
                    "the program is over. bpd did not start that process and is not its \
                     parent, so what it exited with is not bpd's to read — there is no \
                     exit code to report\n",
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
            self.worlds.remove(stop);
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

            self.worlds.insert(stop.stop, whole_program);

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
    ///
    /// the request is addressed before it is sent, and the rule for what to
    /// address it to is [`bpd_core::Addressed::of`] — in the core, because both
    /// front ends have to apply it. a request that is about a stop goes to the
    /// session that stop was reported from, and one that is about the program
    /// names none, which is the only session this connection serves
    fn ask(&mut self, request: Request) -> Result<Response, Aborted> {
        let mut events = Events::new(&self.output);
        let held = self.session.as_ref().map(|session| session.held());
        let answered = self
            .session
            .as_mut()
            .ok_or("nothing has been launched yet")?
            .dispatch(
                Addressed::of(request, &held.unwrap_or_default()),
                &mut events,
            )
            .map_err(|error| failed(&error));
        let joined = events.finish().map_err(Aborted::Wire)?;
        self.tell_the_client_to_start(&joined)?;
        let answered = answered?;
        // a request answered on one thread can arrive with another thread's stop
        // behind it. saying nothing about that one would leave a client
        // believing a thread is running that is not
        if self.configured {
            self.announce()?;
        }
        Ok(answered)
    }

    /// whether a debugged child could be delivered on this connection at all
    ///
    /// asked **before** anything forks, because that is the only moment at which
    /// refusing costs nothing. a child that has already opened a session is a
    /// held process, and a client that cannot be told to take it up would leave
    /// it held for ever — so both halves are checked here and neither is
    /// discovered later
    fn can_debug_children(&self) -> Result<(), Aborted> {
        if !self.client.start_debugging {
            return Err(Aborted::Refuse(
                "`debugChildren` needs the client to support the `startDebugging` \
                 reverse request, and this one did not say it does in \
                 `initialize`. a debugged child **stops** — a fork at the line \
                 that forked, and one that was `exec`'d at its own startup — and \
                 DAP's only way to hand a second program to a client is to ask \
                 it to start a second session, so bpd would have a held process \
                 nothing could reach. take `debugChildren` out and the child is \
                 reported and left running undebugged, which is what bpd does \
                 without it"
                    .to_string(),
            ));
        }
        if matches!(self.reachable, Reachable::Nowhere) {
            return Err(Aborted::Refuse(
                "`debugChildren` needs this adapter to be reachable by a second \
                 connection, and it is speaking on the pipes it was spawned \
                 with. the second session `startDebugging` asks for would be \
                 another `bpd dap` process, with an engine of its own that this \
                 debuggee is not in. run `bpd dap --listen <PORT>` and connect \
                 to it instead — the same listener serves the child's session"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// ask the client to start a session for each debuggee that joined
    ///
    /// the standard `startDebugging` reverse request, and **not** debugpy's
    /// `debugpyAttach` event: that predates the spec having an answer, and this
    /// is the answer the spec has
    ///
    /// the configuration it carries is what the client hands back on its
    /// `attach`. it names the session and how to reach this adapter, because the
    /// second session has to arrive at **this** process — a client that started
    /// a fresh adapter would start a fresh engine, which does not hold the child
    fn tell_the_client_to_start(&mut self, joined: &[SessionId]) -> Answered {
        for session in joined {
            let Reachable::At {
                host,
                port,
                header,
                token,
            } = &self.reachable
            else {
                // `debugChildren` is refused unless a second session can arrive,
                // so a debuggee that joined without one is a session the engine
                // produced that nothing asked for
                unreachable!(
                    "{session} joined this debuggee and nothing can reach this \
                     adapter, so nothing asked for it"
                );
            };
            // written out of the configuration this connection was launched
            // with, so the child's session carries the same settings its
            // parent's does — the same `stopTheWorld`, the same value bounds,
            // and the same `debugChildren`, which is what makes a fork of a
            // fork debugged as well
            let mut configuration = serde_json::to_value(self.configuration())
                .expect("a launch configuration is json and serialises");
            let extra = serde_json::json!({
                "name": format!("bpd: forked child ({session})"),
                "type": "bpd",
                "request": "attach",
                SESSION_FIELD: session.get(),
                // how to reach this adapter. one listener and one token for
                // every session of a debuggee: the connection being asked for
                // is asked for by an adapter this client is already
                // authenticated to, and a token per child would be a second
                // lifetime to get wrong for no boundary it does not already have
                "bpdConnect": {
                    "host": host,
                    "port": port,
                    "header": header,
                    "token": token,
                },
            });
            for (key, value) in extra
                .as_object()
                .expect("the object above is an object")
                .clone()
            {
                configuration[key] = value;
            }
            self.output
                .lock()
                .expect(WRITING)
                .request(
                    "startDebugging",
                    &serde_json::json!({ "request": "attach", "configuration": configuration }),
                )
                .map_err(Aborted::Wire)?;
            self.say(&format!(
                "the program made a child and it is **held** — a fork at the line that \
                 forked, and one that was `exec`'d at its own interpreter startup, \
                 before its program has been compiled. it is {session}, and this \
                 adapter has asked the client to start a debug session for it\n"
            ))?;
        }
        Ok(())
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

/// what the client said it could do, in `initialize`
///
/// **client** capabilities rather than the adapter's, which is why they are
/// recorded rather than advertised, and both are the same shape of thing: each
/// is a thing bpd can only offer if the client can be asked for something. so
/// each is checked at `launch`, before anything has started — the alternative
/// to refusing there is discovering it when a child is already held, or when a
/// session is already waiting for an agent nobody was asked to start
#[derive(Debug, Default, Clone)]
struct ClientCan {
    /// `supportsStartDebuggingRequest` — a session for a debugged child
    start_debugging: bool,
    /// `supportsRunInTerminalRequest` — a command line, in a terminal it owns
    run_in_terminal: bool,
    /// the custom events this client says it reads, from `bpd/understands`
    ///
    /// empty until a client says otherwise, which is every client that has
    /// never heard of the request — so the default is the narration, and being
    /// quiet is the thing that has to be asked for
    understands: std::collections::BTreeSet<String>,
}

/// the event carrying what a jump or a frame restart really did
///
/// named once, because the narration it replaces is switched off by a client
/// naming it back in `bpd/understands` — two spellings of it would mean a client
/// that asked for quiet and did not get it
const MOVED_EVENT: &str = "bpd/moved";

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

/// whether a path the client named and a frame's `co_filename` are one file
///
/// resolved before they are compared, because a client sends the path its editor
/// opened and the interpreter reports the path it imported — the same file
/// reached two ways. when a path cannot be resolved the spellings are compared
/// instead, which is a narrower claim and the only one left: a `co_filename`
/// like `<string>` names no file on disk, and a file that has been deleted since
/// it was imported names nothing either
///
/// a comparison that says no costs a target that would have worked. a
/// comparison that says yes wrongly costs a frame moved against another file's
/// line numbers, which cpython would accept, so this errs the first way
fn same_file(client: &Path, frame: &str) -> bool {
    let frame = Path::new(frame);
    match (client.canonicalize(), frame.canonicalize()) {
        (Ok(client), Ok(frame)) => client == frame,
        _ => client == frame,
    }
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
        // DAP has no reason of its own for it, and `entry` is what it is:
        // nothing of this process has run. that it is a **different process**
        // from the one the client launched is what the description is for — a
        // client showing only the reason would show a second entry stop with
        // nothing saying where it came from
        StopReason::Forked { parent, file, line } => (
            "entry",
            format!(
                "this process was forked from {parent} and is held at \
                 {file}:{line}, the line that forked. it has run nothing of its \
                 own"
            ),
            Vec::new(),
        ),
        // `entry` for the reason a fork is: nothing of this process has run.
        // there is no location on it at all, and inventing one out of the
        // startup machinery it is held in would be a line nobody can act on
        StopReason::Started { parent } => (
            "entry",
            format!(
                "this process was started by {parent} and is held at \
                 interpreter startup, before its program has been compiled. it \
                 has run nothing of its own"
            ),
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
    let mut body = rendered_binding(resolved, requested);
    // a waiting breakpoint is `verified` — it really did bind — so without this
    // an editor shows a solid red dot on a line the interpreter is not watching,
    // and the user waits at it. DAP's `message` is the field for exactly this
    if let Some(after) = resolved.waiting_for {
        let saying = format!(
            "this is bound and not armed yet: it is watched only once breakpoint \
             {after} has been hit"
        );
        body["message"] = match body["message"].as_str() {
            Some(already) => format!("{already}. {saying}").into(),
            None => saying.into(),
        };
    }
    body
}

/// the same, before the arming is added to it
fn rendered_binding(resolved: &Resolved, requested: &SourceBreakpoint) -> serde_json::Value {
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
        // DAP's `Breakpoint` carries one `source` and one `line`, so the `.by`
        // location is what goes in them — it is the file the client asked
        // about, and the one it will show a marker in. the generated location
        // has nowhere of its own to go and rides in `message`, which is the
        // only field of a `Breakpoint` that is free text. it is not dropped:
        // a person who does not believe the debugger needs to be able to see
        // what it saw
        Binding::BoundInSource {
            line,
            generated,
            sites,
            ..
        } => {
            let moved = if *line == requested.line {
                String::new()
            } else {
                format!(
                    "line {} of that file generated nothing bpd can stop on, so this moved to line \
                     {line}, which did. ",
                    requested.line
                )
            };
            serde_json::json!({
                "id": resolved.id,
                "verified": true,
                "line": line,
                "source": source_of(&requested.file.display().to_string()),
                "message": format!(
                    "{moved}`by` transpiled that to line {} of `{}`, and it is armed in {} code \
                     object(s) there",
                    generated.line,
                    generated.file.display(),
                    sites.len()
                ),
            })
        }
        Binding::Unbound { reason } => serde_json::json!({
            "id": resolved.id,
            "verified": false,
            "line": requested.line,
            "source": source_of(&requested.file.display().to_string()),
            "message": reason.to_string(),
            // a file that is not imported yet binds when it is, and everything
            // else will not bind at all. the distinction is the core's, and it
            // is **asked** rather than reproduced here: a reason can arrive
            // wrapped — `InGeneratedPython` is an ordinary one a level down —
            // and matching the variant this adapter happened to know about made
            // every unbound `.by` breakpoint `failed`, beside a message of its
            // own saying it would bind on import
            "reason": if reason.will_bind_later() { "pending" } else { "failed" },
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

/// a DAP source for a frame's file, saying so when a source map placed it
///
/// `origin` is DAP's own field for exactly this — its example is "inlined
/// content from source map" — and it is where a frame reported as `.by` says
/// where the interpreter really is. that location is not shown by default in
/// any client and it is one line of the same object away, which is what a user
/// who does not believe the debugger needs
///
/// there is no match on the mapping here. one sentence says it, it is written
/// in the core, and both front ends read the same one — two adapters wording it
/// themselves is two descriptions of one fact
fn mapped_source_of(file: &str, mapping: Option<&bpd_core::Mapping>) -> serde_json::Value {
    let mut source = source_of(file);
    if let Some(mapping) = mapping {
        source["origin"] = serde_json::Value::String(mapping.to_string());
    }
    source
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
    /// the sessions that joined the debuggee while this was the sink
    ///
    /// not written from here. a `startDebugging` is a request rather than an
    /// event, and one written in the middle of answering something else would
    /// interleave with the answer the adapter is part way through — so it is
    /// collected and sent by the caller, which is the one place that knows the
    /// answer is finished
    joined: Vec<SessionId>,
}

impl<'a> Events<'a> {
    const fn new(output: &'a Output) -> Self {
        Self {
            output,
            failed: None,
            joined: Vec::new(),
        }
    }

    fn finish(self) -> Result<Vec<SessionId>, crate::wire::Error> {
        match self.failed {
            Some(error) => Err(error),
            None => Ok(self.joined),
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

    /// this interpreter hides a whole way of starting a child
    ///
    /// on `important` rather than `console`. DAP has a category for exactly
    /// this — something the user should see even with the console collapsed —
    /// and a client that filed it beside ordinary notices would let the one
    /// message that stops silence being evidence scroll past
    fn blind_to(&mut self, blindspot: bpd_core::Blindspot) {
        self.emit(&serde_json::json!({
            "category": "important",
            "output": format!("{blindspot}\n"),
        }));
    }

    /// a debugged fork opened a session of its own
    ///
    /// kept rather than written, and the caller turns each into a
    /// `startDebugging` reverse request — see the field
    fn attached(&mut self, session: SessionId) {
        self.joined.push(session);
    }
}

/// what one DAP `setBreakpoints` asked for, for one line
struct Wanted {
    line: u32,
    condition: Option<String>,
    log: Option<String>,
    /// the file and line of the breakpoint this one waits for
    ///
    /// **not an id.** the adapter mints breakpoint ids and re-mints them on
    /// every `setBreakpoints` for that file, so an id a client read off an
    /// earlier response has already gone stale. a file and a line are what the
    /// client actually knows, and they are resolved to whatever id the
    /// predecessor holds *now*, when the request is built
    after: Option<(PathBuf, u32)>,
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
    fn replace(&mut self, file: &Path, wanted: &[Wanted]) -> Vec<SourceBreakpoint> {
        let mine: Vec<SourceBreakpoint> = wanted
            .iter()
            .map(|wanted| {
                self.next += 1;
                let mut breakpoint =
                    SourceBreakpoint::at(self.next, file.to_path_buf(), wanted.line);
                breakpoint.condition.clone_from(&wanted.condition);
                breakpoint.log.clone_from(&wanted.log);
                breakpoint.after.clone_from(&None);
                breakpoint
            })
            .collect();
        self.by_file.insert(file.to_path_buf(), mine);

        // the `after` links are resolved **after** the insert, over the whole
        // union, because a breakpoint may wait for one in a file this call did
        // not touch — and because the ids of this file's own breakpoints only
        // exist once they have been minted above
        self.link(file, wanted);
        self.by_file
            .get(file)
            .cloned()
            .unwrap_or_else(|| unreachable!("the file was just inserted"))
    }

    /// turn each `after` file and line into the id that breakpoint holds now
    ///
    /// a predecessor nothing matches is left as `None` rather than invented,
    /// and the core then reports the successor as armed immediately. that is
    /// the one place this could lie, so it does not guess: a client naming a
    /// line with no breakpoint on it gets a breakpoint that is armed, which is
    /// what it would have got had it not asked at all
    fn link(&mut self, file: &Path, wanted: &[Wanted]) {
        let ids: Vec<Option<u32>> = wanted
            .iter()
            .map(|wanted| {
                let (after_file, after_line) = wanted.after.as_ref()?;
                self.by_file
                    .get(after_file)?
                    .iter()
                    .find(|breakpoint| breakpoint.line == *after_line)
                    .map(|breakpoint| breakpoint.id)
            })
            .collect();
        if let Some(mine) = self.by_file.get_mut(file) {
            for (breakpoint, after) in mine.iter_mut().zip(ids) {
                breakpoint.after = after;
            }
        }
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
    use bpd_core::Unbound;

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
            Path::new("/a.py"),
            &[Wanted {
                after: None,
                line: 1,
                condition: None,
                log: None,
            }],
        );
        breakpoints.replace(
            Path::new("/b.py"),
            &[Wanted {
                after: None,
                line: 2,
                condition: Some("x > 1".to_string()),
                log: None,
            }],
        );

        assert_eq!(breakpoints.all().len(), 2);
        assert_eq!(breakpoints.all()[1].condition.as_deref(), Some("x > 1"));

        // replacing one file leaves the other alone, and every id is still
        // distinct — an id names one breakpoint in every report about it
        breakpoints.replace(Path::new("/a.py"), &[]);
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
                waiting_for: None,
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
                waiting_for: None,
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

    #[test]
    fn a_wrapped_reason_decides_the_code_by_what_is_inside_the_wrapper() {
        use bpd_core::source_map::{Located, Unmapped};

        // a `.by` breakpoint whose module is not imported yet fails for exactly
        // the reason a `.py` one does — `InGeneratedPython` says where bpd
        // looked, not what stopped it. deciding the code on the wrapper made
        // every unbound `.by` breakpoint `failed`, including the most ordinary
        // one there is, while the message beside it said "it will bind if that
        // file is imported later"
        let requested = SourceBreakpoint::at(4, "/src/main.by", 5);
        let generated = || Located {
            file: PathBuf::from("/tmp/build/main.py"),
            line: 86,
        };
        let wrapped = |reason: Unbound| {
            rendered_breakpoint(
                &Resolved {
                    waiting_for: None,
                    id: 4,
                    binding: Binding::Unbound {
                        reason: Unbound::InGeneratedPython {
                            file: PathBuf::from("/src/main.by"),
                            requested: 5,
                            generated: generated(),
                            reason: Box::new(reason),
                        },
                    },
                },
                &requested,
            )
        };

        let pending = wrapped(Unbound::NotLoaded {
            file: PathBuf::from("/tmp/build/main.py"),
            templates_available: false,
        });
        assert_eq!(
            pending["reason"], "pending",
            "a `.by` breakpoint waiting for its module binds when the module is \
             imported, exactly as the `.py` one does: {pending}"
        );

        // and the other side, which is what stops an unwrap that answers
        // `pending` for everything from passing. a line the map cannot place is
        // not waiting for anything
        let unmappable = wrapped(Unbound::Unmappable {
            reason: Unmapped::NotInTheMap {
                file: PathBuf::from("/src/main.by"),
            },
        });
        assert_eq!(
            unmappable["reason"], "failed",
            "the map could not place the line, and nothing arriving later \
             changes that: {unmappable}"
        );

        let no_line = wrapped(Unbound::NoExecutableLine {
            file: PathBuf::from("/tmp/build/main.py"),
            requested: 86,
            last_executable: Some(40),
        });
        assert_eq!(
            no_line["reason"], "failed",
            "the file is loaded and has no line there: {no_line}"
        );
    }

    #[test]
    fn a_by_breakpoint_keeps_the_by_location_and_still_says_where_it_really_is() {
        use bpd_core::source_map::Located;
        use bpd_core::{Evaluation, Site};

        // DAP's `Breakpoint` has one source and one line. the `.by` is what goes
        // in them, because that is the file the client asked about and the one
        // it will put a marker in — and the generated location is not dropped
        // for want of a field, because a person who does not believe the
        // debugger has to be able to see what it saw
        let requested = SourceBreakpoint::at(4, "/src/app.by", 7);
        let rendered = rendered_breakpoint(
            &Resolved {
                waiting_for: None,
                id: 4,
                binding: Binding::BoundInSource {
                    line: 7,
                    generated: Located {
                        file: PathBuf::from("/tmp/build/app.py"),
                        line: 19,
                    },
                    sites: vec![Site {
                        qualname: "main".to_string(),
                        first_line: 12,
                        offset: 4,
                    }],
                    evaluation: Evaluation::Always,
                },
            },
            &requested,
        );

        assert_eq!(rendered["verified"], true);
        assert_eq!(
            rendered["line"], 7,
            "the `.by` line is what the client sees"
        );
        assert_eq!(rendered["source"]["path"], "/src/app.by");
        let said = rendered["message"]
            .as_str()
            .expect("a mapped breakpoint says where it really is");
        assert!(said.contains("/tmp/build/app.py"), "said {said}");
        assert!(said.contains("line 19"), "said {said}");
    }
}
