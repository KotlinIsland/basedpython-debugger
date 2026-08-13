//! what the debugger costs the program it is measuring
//!
//! `README.md` used to say that a line with no breakpoint on it costs nothing,
//! with no number behind it. this is the measurement that was allowed to
//! disagree, and on two counts it did — the readme, `docs/index.md` and
//! `docs/development/architecture.md` all say something different now
//!
//! the workloads were chosen to be the **worst** cases for the design rather
//! than the best — a loop whose body is six lines run three million times, ten
//! million python calls, an import-heavy startup where every code object is
//! seen exactly once — because a benchmark picked to flatter a design launders
//! an unsupported claim into an apparently supported one
//!
//! each workload is run bare, under `bpd`, and under debugpy; the `lines`
//! workload is run twice more under each debugger, once with a breakpoint that
//! is hit fifty times and once with a breakpoint whose line the program never
//! reaches. both debuggers are driven through **DAP**, by the same client, so
//! the comparison includes each one's own front end rather than only its event
//! path. that is what a user pays either way
//!
//! there are three groups, and they answer three different questions:
//!
//! - **`session`** — wall clock for the whole thing. what somebody waits for
//! - **`run`** — the program's own clock over its own work, so the fixed cost of
//!   starting a session lands outside it. this is the number the claim about a
//!   *line* is a claim about, and the one that can contradict it
//! - **`attach`** — where `bpd`'s fixed cost goes, which turns out to be mostly
//!   one thing
//!
//! nothing here takes a debugger's word for the run having happened. every
//! workload's exit code is the answer it computed, and it is asserted to be
//! zero; the breakpoint rows assert the breakpoint bound, bound to the line
//! asked for, and was hit exactly the number of times the program reaches it. a
//! debugger whose breakpoint silently did not bind would otherwise post the
//! best number in the table
//!
//! **CI does not gate on these numbers** and must not start to. wall clock from
//! a shared runner varies by more than the effects worth catching. what CI runs
//! is criterion's `--test` mode, which executes each row once so a benchmark
//! that stops compiling, hangs or fails an assertion fails the build. the
//! deterministic performance gate is the allocation count in
//! `crates/bpd_protocol/tests/allocation.rs`
//!
//! the figures this produced, and the machine they came from, are in
//! `docs/development/overhead.md`

// `criterion_group!` generates an undocumented public function, and a bench
// target has no public api for `missing_docs` to be protecting
#![allow(missing_docs)]
// a benchmark that cannot say which interpreters and which debugpy produced its
// numbers is a benchmark nobody else can reproduce
#![allow(clippy::print_stderr)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bpd_core::python::Capabilities;
use bpd_test::debuggee::{Fixture, line_of};
use criterion::{BenchmarkId, Criterion, SamplingMode, criterion_group, criterion_main};

/// the `bpd` this benchmark run built, not whatever is on PATH
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// how many whole processes each figure is made of
///
/// one criterion sample is one run of one program here, deliberately: what is
/// being measured takes hundreds of milliseconds and the interesting variation
/// is *between* processes, not within one. so every figure is a distribution
/// over this many runs rather than a mean over one
const RUNS: usize = 10;

/// how long the client waits for an adapter to say anything
///
/// a debugger that stops answering otherwise leaves this reading a pipe that
/// will never say anything, and a benchmark that hangs cannot be told apart
/// from a slow one. generous, because the slowest row here takes eleven seconds
/// on the machine it was written on and a loaded CI runner is not a hang
const PATIENCE: Duration = Duration::from_mins(5);

/// a program to run under each of the things being measured
struct Workload {
    /// what it is called in the report, and the module it is written as
    name: &'static str,
    /// its source, on disk rather than in a string literal so it stays real,
    /// lintable python
    source: &'static str,
}

/// the programs, worst cases first
const WORKLOADS: &[Workload] = &[
    Workload {
        name: "startup",
        source: include_str!("workloads/startup.py"),
    },
    Workload {
        name: "lines",
        source: include_str!("workloads/lines.py"),
    },
    Workload {
        name: "calls",
        source: include_str!("workloads/calls.py"),
    },
    Workload {
        name: "imports",
        source: include_str!("workloads/imports.py"),
    },
    Workload {
        name: "mixed",
        source: include_str!("workloads/mixed.py"),
    },
];

/// the workload the breakpoint rows are measured on
///
/// the tight loop rather than one of the others, because a breakpoint's cost is
/// a claim about the code object it lives in and this is the one whose code
/// object runs eighteen million lines
const HELD: &str = "lines";

/// the lines the breakpoints go on, and how many times each is reached
///
/// looked up by text rather than written down as a line number, so adding a
/// line to the workload above one cannot silently move a breakpoint somewhere
/// cheaper. the hit counts are **asserted**: a breakpoint that bound and was
/// then never offered its line would post the fastest row in the table
///
/// the second one is the measurement that separates the two halves of what a
/// breakpoint costs. its line is inside a branch that cannot be taken, so the
/// code object is instrumented exactly as it is for the first one and the
/// program never stops — whatever is left is what *holding* a breakpoint costs
const HELD_LINES: &[(&str, &str, u32)] = &[
    ("one breakpoint", "# breakpoint reached fifty times", 50),
    ("one breakpoint, never hit", "# breakpoint reached never", 0),
];

/// names an interpreter that has debugpy installed
///
/// the default is the interpreter under test itself. this exists so a machine's
/// own python does not have to be written to for a benchmark to run — point it
/// at a virtualenv instead
const DEBUGPY_ENV: &str = "BPD_BENCH_DEBUGPY";

/// what is being measured, and against what
struct Session {
    /// the interpreter every row debugs, which is the one the built agent
    /// matches
    interpreter: PathBuf,
    /// how that interpreter describes itself
    describes_itself: String,
    /// the interpreter that provides `debugpy.adapter`
    debugpy: PathBuf,
    /// the debugpy release behind the debugpy rows
    debugpy_version: String,
}

impl Session {
    /// find everything the rows need, or fail naming what is missing
    fn discover() -> Self {
        // `cargo bench` builds in the release profile and the agent is a cdylib
        // nothing links, so this invocation did not build it. an artifact left
        // by an earlier `cargo build` (no `--release`) is in a different
        // directory and is not found — which is the honest outcome, since a
        // debug agent's numbers would not be the shipped agent's numbers
        //
        // the interpreter comes first because an agent is resolved **for** one:
        // a `bpd` carries one per interpreter tag. `matching_interpreter` fails
        // with what every candidate said, which is the same missing release
        // build seen from the other end
        let interpreter = bpd_test::agent::matching_interpreter();
        if let Err(error) = bpd_engine::agent::stage_for(interpreter) {
            panic!(
                "{error}\n\nthis is a benchmark, so it runs against the release \
                 agent. build one:\n    cargo build --release -p bpd_agent"
            );
        }

        let (debugpy, debugpy_version) = debugpy_interpreter(interpreter);

        Self {
            interpreter: interpreter.executable.clone(),
            describes_itself: format!(
                "{} {} ({}{})",
                interpreter.executable.display(),
                interpreter.version,
                interpreter.implementation,
                if interpreter.free_threaded {
                    ", free-threaded"
                } else {
                    ""
                }
            ),
            debugpy,
            debugpy_version,
        }
    }

    /// debugpy's own DAP adapter, ready to be spawned
    fn debugpy_adapter(&self) -> Command {
        let mut command = Command::new(&self.debugpy);
        command.args(["-m", "debugpy.adapter"]);
        command
    }
}

/// `bpd dap`, ready to be spawned
fn bpd_adapter() -> Command {
    let mut command = Command::new(BPD);
    command.arg("dap");
    command
}

/// the interpreter that can import debugpy, and which debugpy that is
///
/// it has to be the same `major.minor` as the interpreter under test. debugpy
/// vendors pydevd with a compiled tracing extension built for one version at a
/// time, and a mismatch would silently fall back to the pure python
/// implementation — which is a different debugger from the one being compared
fn debugpy_interpreter(under_test: &Capabilities) -> (PathBuf, String) {
    let python =
        std::env::var_os(DEBUGPY_ENV).map_or_else(|| under_test.executable.clone(), PathBuf::from);

    let probe = "import debugpy, sys; print(debugpy.__version__); \
                 print(f'{sys.version_info.major}.{sys.version_info.minor}')";
    let output = Command::new(&python)
        .args(["-c", probe])
        .output()
        .unwrap_or_else(|error| panic!("could not run `{}`: {error}", python.display()));

    assert!(
        output.status.success(),
        "`{}` cannot import debugpy, so the comparison this benchmark exists to \
         make would not be made:\n{}\ninstall it there:\n    \
         uv pip install --python {} debugpy\nor name an interpreter that has \
         it:\n    {DEBUGPY_ENV}=/path/to/venv/bin/python cargo bench",
        python.display(),
        String::from_utf8_lossy(&output.stderr).trim(),
        python.display(),
    );

    let said = String::from_utf8(output.stdout).expect("cpython writes utf8 to stdout");
    let mut answers = said.lines();
    let version = answers
        .next()
        .expect("the probe prints debugpy's version first")
        .to_string();
    let series = answers
        .next()
        .expect("the probe prints the interpreter's series second");

    let wanted = format!("{}.{}", under_test.version.major, under_test.version.minor);
    assert_eq!(
        series,
        wanted,
        "`{}` is python {series} and the programs under test run on python \
         {wanted}. debugpy ships compiled tracing built for one series at a \
         time, so a mismatch would be measured against its pure python fallback \
         rather than against debugpy",
        python.display(),
    );

    (python, version)
}

/// what one run of one program came to
struct Ran {
    /// what the program itself said it spent on its own work
    ///
    /// the `run` rows. a debugger's fixed cost — staging an agent, spawning an
    /// interpreter, a handshake — lands outside this, so it is the number the
    /// claim about a *line* is a claim about
    reported: Duration,
    /// how many times the program stopped
    hits: u32,
}

/// the line every workload prints, and what it means
///
/// the program's own clock over its own work, in microseconds. it comes back
/// through whatever channel the debugger gives a program's stdout, which is a
/// pipe for the bare row and an `output` event for the other two
const MARKER: &str = "bpd-bench ";

/// the program's own timing, out of everything it wrote
fn reported(script: &Path, said: &str) -> Duration {
    let microseconds: u64 = said
        .lines()
        .find_map(|line| line.trim().strip_prefix(MARKER))
        .unwrap_or_else(|| {
            panic!(
                "`{}` never reported its own timing. what it said:\n{said}",
                script.display()
            )
        })
        .parse()
        .unwrap_or_else(|error| panic!("`{}` reported {error}", script.display()));
    Duration::from_micros(microseconds)
}

/// run a program with no debugger of any kind
///
/// the baseline. its output goes into a pipe rather than to the terminal for
/// the same reason the debugged rows' does — the program's own timing comes
/// back on it, and a row that read its output differently would be a row with a
/// different cost
fn bare(interpreter: &Path, script: &Path) -> Ran {
    let output = Command::new(interpreter)
        .arg(script)
        .output()
        .unwrap_or_else(|error| panic!("could not run `{}`: {error}", interpreter.display()));
    assert!(
        output.status.success(),
        "`{}` exited with {}, so its own answer was wrong and this run measured \
         something other than the workload",
        script.display(),
        output.status
    );

    Ran {
        reported: reported(
            script,
            &String::from_utf8(output.stdout).expect("the workloads write utf8"),
        ),
        hits: 0,
    }
}

/// run a program under a DAP adapter, to the program's own exit
///
/// `breakpoint` is a line in the program, and the hit count that comes back is
/// the only thing that proves the breakpoint was really there
fn under(adapter: Command, script: &Path, interpreter: &Path, breakpoint: Option<u32>) -> Ran {
    let mut client = Client::start(adapter);

    client.request(
        "initialize",
        &serde_json::json!({
            "adapterID": "bpd-bench",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
        }),
    );

    // sent rather than asked. bpd answers `launch` straight away and debugpy
    // does not answer it until `configurationDone`, so a client that waited for
    // the answer here would work against one of them and deadlock against the
    // other
    client.send(
        "launch",
        &serde_json::json!({ "program": script, "python": interpreter }),
    );
    client.event("initialized");

    if let Some(line) = breakpoint {
        let set = client.request(
            "setBreakpoints",
            &serde_json::json!({
                "source": { "path": script },
                "breakpoints": [ { "line": line } ],
            }),
        );
        let bound = &set["body"]["breakpoints"][0];
        assert_eq!(
            bound["verified"], true,
            "the breakpoint did not bind, so this row is a debugger with \
             nothing set in it: {set}"
        );
        assert_eq!(
            bound["line"], line,
            "the breakpoint moved off the hot line, so this row is not the \
             measurement it claims: {set}"
        );
    }

    client.request("configurationDone", &serde_json::json!({}));

    let mut hits = 0;
    let mut exit = None;
    let mut said = String::new();
    loop {
        let event = client.next_event();
        match event["event"].as_str() {
            Some("stopped") => {
                hits += 1;
                client.send(
                    "continue",
                    &serde_json::json!({ "threadId": event["body"]["threadId"] }),
                );
            }
            Some("output") => {
                if let Some(text) = event["body"]["output"].as_str() {
                    said.push_str(text);
                }
            }
            Some("exited") => exit = event["body"]["exitCode"].as_i64(),
            Some("terminated") => break,
            _ => {}
        }
    }

    assert_eq!(
        exit,
        Some(0),
        "`{}` exited with {exit:?}, so its own answer was wrong and this run \
         measured something other than the workload",
        script.display()
    );

    client.finish();
    Ran {
        reported: reported(script, &said),
        hits,
    }
}

/// a DAP client that drives a session and measures nothing itself
///
/// there is a **watchdog**, and it is not decoration: an adapter that stops
/// answering would otherwise leave this reading a pipe that says nothing for
/// ever, and a benchmark that hangs looks exactly like a slow one
struct Client {
    adapter: Arc<Mutex<Child>>,
    finished: Arc<AtomicBool>,
    /// an option only so [`Client::finish`] can close it: an adapter ends when
    /// its client's end of the pipe goes away
    writes: Option<ChildStdin>,
    reads: BufReader<ChildStdout>,
    /// every message read, so an event that arrived while a request was in
    /// flight is not lost
    seen: Vec<serde_json::Value>,
    /// how far through `seen` the session has already looked for an event
    taken: usize,
    seq: i64,
}

impl Client {
    fn start(mut adapter: Command) -> Self {
        let mut adapter = adapter
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("could not start the adapter: {error}"));

        let writes = adapter.stdin.take().expect("stdin was asked for");
        let reads = BufReader::new(adapter.stdout.take().expect("stdout was asked for"));

        let adapter = Arc::new(Mutex::new(adapter));
        let finished = Arc::new(AtomicBool::new(false));
        watch(&adapter, &finished);

        Self {
            adapter,
            finished,
            writes: Some(writes),
            reads,
            seen: Vec::new(),
            taken: 0,
            seq: 0,
        }
    }

    /// write a request and do not wait for its answer
    fn send(&mut self, command: &str, arguments: &serde_json::Value) -> i64 {
        self.seq += 1;
        let body = serde_json::json!({
            "seq": self.seq, "type": "request", "command": command, "arguments": arguments,
        })
        .to_string();

        let writes = self
            .writes
            .as_mut()
            .expect("nothing is sent after the client has hung up");
        write!(writes, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("the adapter is reading its stdin");
        writes.flush().expect("the adapter is reading its stdin");
        self.seq
    }

    /// write a request and read until its answer arrives
    fn request(&mut self, command: &str, arguments: &serde_json::Value) -> serde_json::Value {
        let sent = self.send(command, arguments);
        loop {
            let message = self.next_message();
            if message["type"] == "response" && message["request_seq"] == sent {
                assert_eq!(
                    message["success"], true,
                    "`{command}` was refused: {message}"
                );
                return message;
            }
        }
    }

    /// the next event that has not been looked at yet
    ///
    /// a cursor over everything read rather than a fresh read, because a
    /// `stopped` can arrive while a request is in flight and an event dropped
    /// there is a stop the program is still waiting on
    fn next_event(&mut self) -> serde_json::Value {
        loop {
            while self.taken < self.seen.len() {
                let message = self.seen[self.taken].clone();
                self.taken += 1;
                if message["type"] == "event" {
                    return message;
                }
            }
            self.next_message();
        }
    }

    /// read events until one of this name arrives
    fn event(&mut self, event: &str) -> serde_json::Value {
        loop {
            let message = self.next_event();
            if message["event"] == event {
                return message;
            }
        }
    }

    fn next_message(&mut self) -> serde_json::Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self
                .reads
                .read_line(&mut line)
                .expect("the adapter is writing its stdout");
            assert_ne!(
                read, 0,
                "the adapter said nothing more. either it hung up, or the \
                 watchdog ended it after {PATIENCE:?}"
            );
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = Some(value.parse().expect("a length is a number"));
            }
        }

        let mut body = vec![0; length.expect("every DAP message carries its length")];
        self.reads
            .read_exact(&mut body)
            .expect("the adapter wrote as many bytes as it promised");

        let message: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|error| {
            panic!(
                "the adapter wrote something that is not a DAP message: {error}\n{}",
                String::from_utf8_lossy(&body)
            )
        });
        self.seen.push(message.clone());
        message
    }

    /// disconnect, and end the adapter without waiting on how it tidies up
    ///
    /// the session under measurement is over by here: the program has exited
    /// with the code the workload's own answer produced, and that was asserted.
    /// what an adapter does *after* a disconnect is not part of what is being
    /// timed, and it is not something to assert on here either — debugpy's
    /// adapter races itself shutting down and exits non-zero often enough that
    /// a benchmark asserting on it would fail for a reason that has nothing to
    /// do with a number. that `bpd dap` exits cleanly after a disconnect is
    /// asserted where it belongs, in `crates/bpd/tests/dap.rs`
    fn finish(&mut self) {
        self.request("disconnect", &serde_json::json!({}));
        drop(self.writes.take());
        self.finished.store(true, Ordering::Relaxed);
        end(&self.adapter);
    }
}

/// end the adapter if the client is still waiting on it after [`PATIENCE`]
fn watch(adapter: &Arc<Mutex<Child>>, finished: &Arc<AtomicBool>) {
    let adapter = Arc::clone(adapter);
    let finished = Arc::clone(finished);
    std::thread::Builder::new()
        .name("bench-watchdog".to_string())
        .spawn(move || {
            let deadline = Instant::now() + PATIENCE;
            while Instant::now() < deadline {
                if finished.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            end(&adapter);
        })
        .expect("a watchdog thread can be started");
}

/// end the adapter, and the debuggee it is holding with it
fn end(adapter: &Arc<Mutex<Child>>) {
    let mut adapter = adapter
        .lock()
        .expect("nothing panics holding the adapter: every path is a kill or a wait");
    if adapter
        .try_wait()
        .expect("the adapter is a real child")
        .is_none()
    {
        let killed = adapter.kill();
        drop(killed);
        let reaped = adapter.wait();
        drop(reaped);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // a row that failed part way through leaves an adapter holding a
        // debuggee, and a debuggee with a held thread never exits
        self.finished.store(true, Ordering::Relaxed);
        end(&self.adapter);
    }
}

/// the rows of one workload, as `(what it is called, how to run it)`
///
/// built once and used by both groups, so `session` and `run` can never end up
/// measuring different things under the same name
fn rows<'a>(
    session: &'a Session,
    workload: &'a Workload,
    script: &'a Path,
) -> Vec<(String, Box<dyn Fn() -> Ran + 'a>)> {
    let mut rows: Vec<(String, Box<dyn Fn() -> Ran + 'a>)> = vec![
        (
            "bare".to_string(),
            Box::new(move || bare(&session.interpreter, script)),
        ),
        (
            "bpd".to_string(),
            Box::new(move || {
                let ran = under(bpd_adapter(), script, &session.interpreter, None);
                assert_eq!(
                    ran.hits, 0,
                    "no breakpoints were set and the program stopped"
                );
                ran
            }),
        ),
        (
            "debugpy".to_string(),
            Box::new(move || {
                let ran = under(
                    session.debugpy_adapter(),
                    script,
                    &session.interpreter,
                    None,
                );
                assert_eq!(
                    ran.hits, 0,
                    "no breakpoints were set and the program stopped"
                );
                ran
            }),
        ),
    ];

    if workload.name == HELD {
        for &(what, needle, hits) in HELD_LINES {
            let line = line_of(workload.source, needle);
            rows.push((
                format!("bpd, {what}"),
                Box::new(move || held(bpd_adapter(), script, session, line, hits)),
            ));
            rows.push((
                format!("debugpy, {what}"),
                Box::new(move || held(session.debugpy_adapter(), script, session, line, hits)),
            ));
        }
    }

    rows
}

/// run a program with one breakpoint in it, and check it was really there
fn held(adapter: Command, script: &Path, session: &Session, line: u32, hits: u32) -> Ran {
    let ran = under(adapter, script, &session.interpreter, Some(line));
    assert_eq!(
        ran.hits, hits,
        "the breakpoint on line {line} was hit {} times and the program reaches \
         it {hits}, so this row is not the measurement it claims",
        ran.hits
    );
    ran
}

/// tell criterion that one sample here is one whole process
///
/// what is being measured takes hundreds of milliseconds, so criterion is given
/// a target time under one of them and the iteration count it derives from its
/// warm-up is always one. it says on stderr that it could not fit [`RUNS`]
/// samples in the target time, which is the arrangement rather than a problem —
/// the interesting variation in a process-level measurement is *between*
/// processes, and averaging inside a sample would hide it
fn one_process_per_sample<M: criterion::measurement::Measurement>(
    group: &mut criterion::BenchmarkGroup<'_, M>,
) {
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(RUNS);
    group.warm_up_time(Duration::from_millis(1));
    group.measurement_time(Duration::from_millis(1));
}

/// the whole session, end to end — what somebody waits for
///
/// process spawn, agent staging, the handshake, the program, and the
/// disconnect. this is the number a user experiences, and it is dominated by
/// the fixed cost of starting a session rather than by anything on the event
/// path
fn whole_session(criterion: &mut Criterion) {
    let session = Session::discover();
    announce(&session);

    let programs = laid_out();
    let mut group = criterion.benchmark_group("session");
    one_process_per_sample(&mut group);

    for (workload, fixture) in &programs {
        let script = fixture.path();
        for (name, run) in rows(&session, workload, &script) {
            group.bench_function(BenchmarkId::new(name, workload.name), |bencher| {
                bencher.iter(&run);
            });
        }
    }

    group.finish();
}

/// the program's own clock over its own work
///
/// this is the row the "a line with no breakpoint on it costs nothing" claim is
/// about, and the one that can contradict it. the fixed cost of starting a
/// session lands outside the program's own timing, so what is left is the event
/// path: eighteen million lines, ten million calls, thousands of code objects
///
/// the measured value is the number the *program* reported, handed to criterion
/// through `iter_custom`. the workloads whose whole content is the fixed cost
/// have no row here
fn run_cost(criterion: &mut Criterion) {
    let session = Session::discover();
    let programs = laid_out();
    let mut group = criterion.benchmark_group("run");
    one_process_per_sample(&mut group);

    for (workload, fixture) in &programs {
        if workload.name == FIXED_COST_ONLY {
            continue;
        }
        let script = fixture.path();
        for (name, run) in rows(&session, workload, &script) {
            group.bench_function(BenchmarkId::new(name, workload.name), |bencher| {
                bencher.iter_custom(|iterations| {
                    (0..iterations).map(|_| run().reported).sum::<Duration>()
                });
            });
        }
    }

    group.finish();
}

/// where the fixed cost of attaching goes
///
/// the `session` rows have `bpd` adding a fixed cost to a program that does
/// nothing at all, and the `run` rows have the event path adding nothing — so
/// that fixed cost is the attaching itself, and this is where most of it was
///
/// the agent is a `cdylib`, and the first load of a shared object the system
/// has never seen costs far more than every load of the same file after it.
/// staging is a content-addressed cache because of these two rows: `staged
/// once` is what a launch does now, and `staged fresh` is the control — a copy
/// into a directory of its own, which is what every launch used to get. the
/// third row is the interpreter on its own, so the other two can be read
/// against something
fn attaching(criterion: &mut Criterion) {
    let session = Session::discover();
    let mut group = criterion.benchmark_group("attach");
    one_process_per_sample(&mut group);

    group.bench_function("interpreter alone", |bencher| {
        bencher.iter(|| snippet(&session.interpreter, "pass", None));
    });

    group.bench_function("agent imported, staged once", |bencher| {
        let staged = bpd_test::agent::staged_for(bpd_test::agent::matching_interpreter());
        bencher.iter(|| {
            snippet(
                &session.interpreter,
                "import bpd_agent",
                Some(staged.python_path()),
            );
        });
    });

    group.bench_function("agent imported, staged fresh", |bencher| {
        bencher.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| {
                    // a cache of its own per iteration, so every one of them is
                    // a file the system has never seen. the copy is outside the
                    // timed part: what is being asked is what the *interpreter*
                    // pays for such a file, not what a one megabyte copy costs
                    let cache = tempfile::tempdir().expect("a temporary directory can be made");
                    let staged = bpd_engine::agent::stage_for_into(
                        cache.path(),
                        bpd_test::agent::matching_interpreter(),
                    )
                    .unwrap_or_else(|error| panic!("could not stage the agent: {error}"));
                    let started = Instant::now();
                    snippet(
                        &session.interpreter,
                        "import bpd_agent",
                        Some(staged.python_path()),
                    );
                    started.elapsed()
                })
                .sum()
        });
    });

    group.finish();
}

/// run one snippet in one interpreter, with the agent importable or not
fn snippet(interpreter: &Path, code: &str, agent: Option<&Path>) {
    let mut command = Command::new(interpreter);
    command.args(["-c", code]);
    if let Some(path) = agent {
        command.env("PYTHONPATH", path);
    }
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("could not run `{}`: {error}", interpreter.display()));
    assert!(status.success(), "`{code}` exited with {status}");
}

/// the workload that is nothing but the fixed cost of a session
const FIXED_COST_ONLY: &str = "startup";

/// every workload, written into a directory of its own
fn laid_out() -> Vec<(&'static Workload, Fixture)> {
    WORKLOADS
        .iter()
        .map(|workload| (workload, Fixture::new(workload.name, workload.source)))
        .collect()
}

/// say what produced the numbers, since a number without that is not
/// reproducible
fn announce(session: &Session) {
    eprintln!(
        "bpd bench: {RUNS} runs per figure\n    \
         interpreter: {}\n    \
         debugpy: {} from {}\n",
        session.describes_itself,
        session.debugpy_version,
        session.debugpy.display(),
    );
}

criterion_group!(benches, whole_session, run_cost, attaching);
criterion_main!(benches);
