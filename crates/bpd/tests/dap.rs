//! `bpd dap` driven as a real process, against a real interpreter
//!
//! this is the acceptance for M4: set a breakpoint, hit it, inspect a variable,
//! change it, step, and let the program finish. nothing here takes the
//! adapter's word for anything it can check another way — the write to a local
//! is proved by the **program's own output**, which is what a `f_locals` write
//! the compiled code never reads would not change
//!
//! the transport is the real one too: a child process, `Content-Length`
//! framing, stdin and stdout. that matters more than it looks, because the
//! debuggee's own stdout is on the same file descriptor the protocol would be
//! on if it were inherited — a single `print` would make every message after it
//! unreadable, and a test that spoke to the adapter in-process would never
//! notice

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bpd_test::debuggee::{Fixture, line_of};

/// the binary this test run built, not whatever `bpd` is on PATH
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// a program with a local worth writing to, and a marker after the breakpoint
///
/// `total` is printed *after* the line the breakpoint is on, so a write the
/// compiled code never reads shows up as the old number
const PROGRAM: &str = r#"import sys


def work(seed):
    total = seed + 1
    doubled = total * 2
    return total, doubled


def main():
    total, doubled = work(1)
    print("total", total, flush=True)
    print("doubled", doubled, flush=True)
    sys.exit(3)


main()
"#;

/// how long a test waits for the adapter to say something
const PATIENCE: Duration = Duration::from_mins(1);

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "it is one session end to end, and that is the acceptance. \
              splitting it into helpers hides the order the messages go in"
)]
fn a_breakpoint_is_hit_a_local_is_written_and_the_program_sees_the_write() {
    let fixture = Fixture::new("dapped", PROGRAM);
    let mut client = Client::start();

    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({
            "program": fixture.path(),
            "python": interpreter(),
        }),
    );
    client.event("initialized");

    // the breakpoint goes on the line *after* `total` is bound, so the frame
    // really holds it when the stop happens
    let line = line_of(PROGRAM, "doubled = total * 2");
    let set = client.request(
        "setBreakpoints",
        &serde_json::json!({
            "source": { "path": fixture.path() },
            "breakpoints": [ { "line": line } ],
        }),
    );
    let bound = &set["body"]["breakpoints"][0];
    assert_eq!(
        bound["verified"], true,
        "the breakpoint did not bind: {set}"
    );
    assert_eq!(bound["line"], line);

    client.request("configurationDone", &serde_json::json!({}));

    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint");
    // a stop holds one thread and the rest of the program keeps running. a
    // client told otherwise would render a whole-program stop that never
    // happened
    assert_eq!(stopped["body"]["allThreadsStopped"], false);
    let thread = stopped["body"]["threadId"].clone();

    let threads = client.request("threads", &serde_json::json!({}));
    assert!(
        !threads["body"]["threads"]
            .as_array()
            .expect("a thread list")
            .is_empty(),
        "the census reported no threads at all: {threads}"
    );

    let stack = client.request("stackTrace", &serde_json::json!({ "threadId": thread }));
    let frames = stack["body"]["stackFrames"]
        .as_array()
        .expect("a stack is an array");
    assert_eq!(frames[0]["name"], "work", "the stack was {stack}");
    assert_eq!(frames[0]["line"], line);
    assert_eq!(
        frames[0]["source"]["path"].as_str(),
        fixture.path().to_str()
    );
    // `main`, `<module>`, and no frame of bpd's own: the `-c` the interpreter
    // was entered through is the parent of the program's module frame
    assert_eq!(
        frames
            .iter()
            .map(|frame| &frame["name"])
            .collect::<Vec<_>>(),
        vec!["work", "main", "<module>"],
        "the stack was {stack}"
    );
    let frame = frames[0]["id"].clone();

    let scopes = client.request("scopes", &serde_json::json!({ "frameId": frame }));
    let named: Vec<&str> = scopes["body"]["scopes"]
        .as_array()
        .expect("scopes are an array")
        .iter()
        .map(|scope| scope["name"].as_str().expect("a scope has a name"))
        .collect();
    assert_eq!(
        named,
        vec!["local", "cell", "free", "global"],
        "python resolves a name by which of these it is, so all four are offered"
    );
    let local = scopes["body"]["scopes"][0]["variablesReference"].clone();

    let variables = client.request(
        "variables",
        &serde_json::json!({ "variablesReference": local }),
    );
    let total = variable(&variables, "total");
    assert_eq!(total["value"], "2", "the locals were {variables}");
    assert_eq!(total["type"], "int");

    let evaluated = client.request(
        "evaluate",
        &serde_json::json!({ "expression": "total * 10", "frameId": frame }),
    );
    assert_eq!(evaluated["body"]["result"], "20");

    // the write, and the whole point of the test: the program's own output is
    // what proves the compiled code received it
    let written = client.request(
        "setVariable",
        &serde_json::json!({ "variablesReference": local, "name": "total", "value": "41" }),
    );
    assert_eq!(written["body"]["value"], "41");

    client.request("next", &serde_json::json!({ "threadId": thread }));
    let stepped = client.event("stopped");
    assert_eq!(stepped["body"]["reason"], "step");

    client.request("continue", &serde_json::json!({ "threadId": thread }));

    let exited = client.event("exited");
    assert_eq!(
        exited["body"]["exitCode"], 3,
        "the program calls sys.exit(3)"
    );
    client.event("terminated");

    // the program's own stdout came through as `output` events rather than into
    // the protocol stream, and it says the write landed
    let said = client.output();
    assert!(
        said.contains("total 41"),
        "the program printed {said:?}, so the write never reached the frame the \
         compiled code reads"
    );
    // and the line the stop was on had not run yet, so the program computed
    // `doubled` from the written value rather than from the one it had
    assert!(
        said.contains("doubled 82"),
        "the program printed {said:?}, so the line it was held on ran against \
         the old value"
    );

    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

/// a program whose only thread goes round a python loop until a file appears
const SPINNING: &str = r#"import pathlib
import sys

HERE = pathlib.Path(__file__).parent
STOP = HERE / "stop"

print("running", flush=True)
going = True
while going:
    going = not STOP.exists()
print("finished", flush=True)
sys.exit(0)
"#;

#[test]
fn a_running_program_can_be_paused_while_the_adapter_is_waiting_for_it() {
    // the request that cannot go the way every other one goes: the agent
    // answers on a thread it is holding, and a running program has none. so it
    // is delivered on an interrupt while the session is blocked reading its
    // connection, and this is what proves that path is real
    let fixture = Fixture::new("spinner", SPINNING);
    let mut client = Client::start();

    client.request("initialize", &serde_json::json!({}));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");
    client.request("configurationDone", &serde_json::json!({}));

    // wait until the program is really going round the loop, so the pause is
    // delivered to a program that is running rather than to one still starting
    let deadline = Instant::now() + PATIENCE;
    while !client.output().contains("running") {
        assert!(Instant::now() < deadline, "the program never started");
        client.drain();
    }

    client.request("pause", &serde_json::json!({}));
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "pause");

    // the acknowledgement says which threads were running python when the pause
    // went on, which is what tells a client whether a stop is coming at all
    assert!(
        client.output().contains("a pause is armed"),
        "the pause acknowledgement never reached the client: {:?}",
        client.output()
    );

    let thread = stopped["body"]["threadId"].clone();
    let stack = client.request("stackTrace", &serde_json::json!({ "threadId": thread }));
    assert!(
        !stack["body"]["stackFrames"]
            .as_array()
            .expect("a stack is an array")
            .is_empty(),
        "a paused thread is held and has a stack: {stack}"
    );

    std::fs::write(fixture.directory().join("stop"), "x").expect("the fixture directory is there");
    client.request("continue", &serde_json::json!({ "threadId": thread }));

    let exited = client.event("exited");
    assert_eq!(exited["body"]["exitCode"], 0);
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

#[test]
fn a_breakpoint_in_a_module_that_is_not_imported_yet_is_pending_and_says_so() {
    // "the breakpoint is set" is the easiest thing in a debugger to claim
    // wrongly. a module the program has not imported has no code object behind
    // it, and DAP has a word for exactly that state
    let fixture = Fixture::new(
        "importer",
        "import later\n\nprint(later.value(), flush=True)\n",
    );
    let sibling = fixture.sibling("later", "def value():\n    return 7\n");

    let mut client = Client::start();
    client.request("initialize", &serde_json::json!({}));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");

    let set = client.request(
        "setBreakpoints",
        &serde_json::json!({
            "source": { "path": sibling },
            "breakpoints": [ { "line": 2 } ],
        }),
    );
    let breakpoint = &set["body"]["breakpoints"][0];
    assert_eq!(
        breakpoint["verified"], false,
        "nothing has been imported yet: {set}"
    );
    assert_eq!(breakpoint["reason"], "pending");
    assert!(
        breakpoint["message"]
            .as_str()
            .expect("an unbound breakpoint says why")
            .contains("imported later"),
        "the reason was {breakpoint}"
    );

    client.request("configurationDone", &serde_json::json!({}));

    // importing it is what binds it, and the client is told rather than left
    // holding a breakpoint it believes is unset
    let changed = client.event("breakpoint");
    assert_eq!(changed["body"]["breakpoint"]["verified"], true);
    assert_eq!(changed["body"]["breakpoint"]["line"], 2);

    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint");

    client.request(
        "continue",
        &serde_json::json!({ "threadId": stopped["body"]["threadId"] }),
    );
    client.event("exited");
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

#[test]
fn an_editor_can_run_a_whole_investigation_the_way_an_agent_can() {
    // the parity rule, at the far end: a debug script is a capability of the
    // core, so it is not an agent's alone. DAP has no request of its own for one
    // and never will, so it is an extension — and a client sends it with the
    // `customRequest` every DAP client has
    let fixture = Fixture::new("scripted", PROGRAM);
    let mut client = Client::start();

    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");
    client.request(
        "setBreakpoints",
        &serde_json::json!({
            "source": { "path": fixture.path() },
            "breakpoints": [ { "line": line_of(PROGRAM, "total = seed + 1") } ],
        }),
    );
    client.request("configurationDone", &serde_json::json!({}));

    let stopped = client.event("stopped");
    let thread = stopped["body"]["threadId"].clone();

    let ran = client.request(
        "bpd/runScript",
        &serde_json::json!({
            "threadId": thread,
            "steps": [
                { "step": "step_over" },
                {
                    "step": "if",
                    "predicate": { "expression": "total == 2" },
                    "then": [ { "step": "eval", "expression": "total * 10" } ],
                    "otherwise": [ { "step": "log", "note": "the seed was not 1" } ],
                },
            ],
            "budget": { "steps": 10, "wall_ms": 30000, "bytes": 65536 },
        }),
    );

    assert_eq!(ran["success"], true, "the script was refused: {ran}");
    let records = ran["body"]["records"]
        .as_array()
        .expect("a transcript has records");
    assert_eq!(records.len(), 3, "{ran}");
    assert_eq!(
        records[0]["did"]["landed"]["to"]["place"]["line"],
        line_of(PROGRAM, "doubled = total * 2"),
        "the step landed on the next line and the transcript says where: {ran}"
    );
    assert_eq!(records[1]["did"]["answered"]["value"], true);
    assert_eq!(
        records[2]["did"]["result"]["value"]["content"]["text"],
        "20"
    );
    assert_eq!(ran["body"]["outcome"]["outcome"], "ran");

    // a script ends leaving a thread held at a stop the client was never told
    // about by an event, so the adapter announces it — the same rule as any
    // other stop that arrives while a request is being answered
    let announced = client.event("stopped");
    assert_eq!(announced["body"]["reason"], "step", "{announced}");

    client.request("continue", &serde_json::json!({ "threadId": thread }));
    client.event("exited");
    client.event("terminated");
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

#[test]
fn a_capability_that_is_not_advertised_is_refused_rather_than_guessed_at() {
    let fixture = Fixture::new("hit", "x = 1\ny = 2\n");
    let mut client = Client::start();

    client.request("initialize", &serde_json::json!({}));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");

    let capabilities = client.seen_response("initialize");
    assert_eq!(
        capabilities["body"]["supportsHitConditionalBreakpoints"],
        serde_json::Value::Null,
        "the capability is not advertised, so a client has no reason to send one"
    );

    // and a client that sends one anyway is told why rather than answered with
    // whichever of the four conventions bpd happened to pick
    let refused = client.request(
        "setBreakpoints",
        &serde_json::json!({
            "source": { "path": fixture.path() },
            "breakpoints": [ { "line": 2, "hitCondition": ">5" } ],
        }),
    );
    assert_eq!(refused["success"], false, "it was answered: {refused}");
    assert!(
        refused["message"]
            .as_str()
            .expect("a refusal says why")
            .contains("per-client convention"),
        "the refusal was {refused}"
    );

    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

/// the interpreter the built agent matches
fn interpreter() -> String {
    bpd_test::agent::matching_interpreter()
        .executable
        .display()
        .to_string()
}

/// one variable of a `variables` response, by name
fn variable<'a>(response: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    response["body"]["variables"]
        .as_array()
        .expect("variables are an array")
        .iter()
        .find(|variable| variable["name"] == name)
        .unwrap_or_else(|| panic!("no variable named `{name}` in {response}"))
}

/// a DAP client, talking to a real `bpd dap`
///
/// there is a **watchdog**, and it is not decoration. an adapter that stops
/// answering leaves this reading a pipe that will never say anything, and a
/// suite that hangs is a suite nobody can tell apart from a slow one. the
/// watchdog ends the adapter, the read returns nothing, and the failure prints
/// the whole conversation up to the point it stopped
struct Client {
    adapter: Arc<Mutex<Child>>,
    finished: Arc<AtomicBool>,
    writes: ChildStdin,
    reads: BufReader<ChildStdout>,
    seen: Vec<serde_json::Value>,
    /// how much of `seen` the test has already looked at for an event
    taken: usize,
    seq: i64,
}

impl Client {
    fn start() -> Self {
        let mut adapter = Command::new(BPD)
            .arg("dap")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary was built by the same cargo invocation as this test");

        let writes = adapter.stdin.take().expect("stdin was asked for");
        let reads = BufReader::new(adapter.stdout.take().expect("stdout was asked for"));

        let adapter = Arc::new(Mutex::new(adapter));
        let finished = Arc::new(AtomicBool::new(false));
        watch(&adapter, &finished);

        Self {
            adapter,
            finished,
            writes,
            reads,
            seen: Vec::new(),
            taken: 0,
            seq: 0,
        }
    }

    /// send a request and read until its answer arrives
    fn request(&mut self, command: &str, arguments: &serde_json::Value) -> serde_json::Value {
        self.seq += 1;
        let sent = self.seq;
        let body = serde_json::json!({
            "seq": sent, "type": "request", "command": command, "arguments": arguments,
        })
        .to_string();

        write!(self.writes, "Content-Length: {}\r\n\r\n{body}", body.len())
            .expect("the adapter is reading its stdin");
        self.writes
            .flush()
            .expect("the adapter is reading its stdin");

        loop {
            let message = self.next_message();
            if message["type"] == "response" && message["request_seq"] == sent {
                return message;
            }
        }
    }

    /// the next event of this name that has not been taken yet
    ///
    /// a cursor rather than a search from the start, because a session has more
    /// than one `stopped` event in it and the second one is a different stop
    fn event(&mut self, event: &str) -> serde_json::Value {
        loop {
            while self.taken < self.seen.len() {
                let message = self.seen[self.taken].clone();
                self.taken += 1;
                if message["type"] == "event" && message["event"] == event {
                    return message;
                }
            }
            self.next_message();
        }
    }

    /// everything the program and bpd have said so far, as one string
    fn output(&self) -> String {
        self.seen
            .iter()
            .filter(|message| message["type"] == "event" && message["event"] == "output")
            .filter_map(|message| message["body"]["output"].as_str())
            .collect()
    }

    /// read one message, so that waiting on the program makes progress
    fn drain(&mut self) {
        self.next_message();
    }

    fn seen_response(&self, command: &str) -> serde_json::Value {
        self.seen
            .iter()
            .find(|message| message["type"] == "response" && message["command"] == command)
            .cloned()
            .unwrap_or_else(|| panic!("nothing answered `{command}`"))
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
                 watchdog ended it after {PATIENCE:?}. what it had said:\n{:#?}",
                self.seen
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

    /// wait for the adapter to end, and require that it ended cleanly
    fn finish(self) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let ended = self
                .adapter
                .lock()
                .expect("nothing panics holding the adapter")
                .try_wait()
                .expect("the adapter is a real child");
            match ended {
                Some(status) => {
                    self.finished.store(true, Ordering::Relaxed);
                    assert!(status.success(), "`bpd dap` exited with {status}");
                    return;
                }
                None => assert!(
                    Instant::now() < deadline,
                    "`bpd dap` did not end after a disconnect"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// end the adapter if the test is still waiting on it after [`PATIENCE`]
///
/// without it an adapter that stops answering leaves the test reading a pipe
/// that will never say anything, and a suite that hangs is one nobody can tell
/// apart from a slow one. this way the read returns nothing and the failure
/// prints the whole conversation up to where it stopped
fn watch(adapter: &Arc<Mutex<Child>>, finished: &Arc<AtomicBool>) {
    let adapter = Arc::clone(adapter);
    let finished = Arc::clone(finished);
    std::thread::Builder::new()
        .name("dap-watchdog".to_string())
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
        // a test that failed part way through leaves an adapter holding a
        // debuggee, and a debuggee with a held thread never exits. left alone it
        // keeps the harness's own stdio open, and the suite looks hung rather
        // than failed
        self.finished.store(true, Ordering::Relaxed);
        end(&self.adapter);
    }
}
