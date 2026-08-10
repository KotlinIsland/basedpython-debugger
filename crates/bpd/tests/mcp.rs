//! `bpd mcp` driven as a real process, against a real interpreter
//!
//! the acceptance for M5. a breakpoint is set, hit, a local read, written and
//! seen by the program, five steps are taken in five tool calls, and a program
//! that never stops is answered with a timeout rather than a hang
//!
//! nothing here takes the server's word for anything it can check another way —
//! the write to a local is proved by the **program's own output**, which is what
//! a `f_locals` write the compiled code never reads would not change
//!
//! the transport is the real one: a child process, newline delimited JSON-RPC on
//! stdin and stdout. that matters more than it looks, because the debuggee's own
//! stdout is on the file descriptor the protocol would be on if it were
//! inherited — a single `print` would make a line of the protocol unreadable,
//! and a test that spoke to the server in-process would never notice

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bpd_test::debuggee::{Fixture, line_of};

/// the binary this test run built, not whatever `bpd` is on PATH
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// how long a test waits for the server to say something
const PATIENCE: Duration = Duration::from_mins(1);

/// how long a control tool is given when the program really is going to stop
const GENEROUS: u64 = 30_000;

/// a program with a local worth writing to, a marker after the breakpoint, and
/// enough plain statements after it to step over
const PROGRAM: &str = r#"import sys


def work(seed):
    total = seed + 1
    doubled = total * 2
    return total, doubled


def main():
    total, doubled = work(1)
    first = 1
    second = first + 1
    third = second + 1
    print("total", total, flush=True)
    print("doubled", doubled, flush=True)
    sys.exit(3)


main()
"#;

#[test]
fn a_breakpoint_is_hit_a_local_is_written_and_five_steps_cost_five_calls() {
    let fixture = Fixture::new("mcpped", PROGRAM);
    let mut client = Client::start();

    let ready = client.ask(
        "initialize",
        &serde_json::json!({ "protocolVersion": bpd_mcp::PROTOCOL_VERSION }),
    );
    assert_eq!(ready["result"]["serverInfo"]["name"], "bpd");
    assert!(
        ready["result"]["instructions"]
            .as_str()
            .expect("a server says what it is")
            .contains("deadline_ms"),
        "the instructions are one of the two surfaces an agent always sees: {ready}"
    );

    let launched = client.call(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    // every program is held before its first statement, which is how a
    // breakpoint binds against a real interpreter rather than against a guess
    assert_eq!(launched["outcome"], "stopped", "launch gave {launched}");
    assert_eq!(launched["reason"], serde_json::json!("entry"));

    let line = line_of(PROGRAM, "doubled = total * 2");
    let set = client.call(
        "set_breakpoints",
        &serde_json::json!({
            "breakpoints": [ { "file": fixture.path(), "line": line } ],
        }),
    );
    let bound = &set["breakpoints"][0];
    assert_eq!(bound["bound"], true, "the breakpoint did not bind: {set}");
    assert_eq!(bound["line"], line);
    assert_eq!(bound["id"], 1, "ids are the position in the set: {set}");

    let hit = client.call("continue_", &serde_json::json!({ "deadline_ms": GENEROUS }));
    // the whole claim of this front end: the stop **is** the return value, and
    // it arrives with where the program is
    assert_eq!(hit["outcome"], "stopped", "continue_ gave {hit}");
    assert_eq!(
        hit["reason"]["breakpoint"]["breakpoints"],
        serde_json::json!([1])
    );
    assert_eq!(hit["frames"][0]["function"], "work");
    assert_eq!(hit["frames"][0]["line"], line);
    assert_eq!(
        hit["frames"]
            .as_array()
            .expect("frames are an array")
            .iter()
            .map(|frame| frame["function"].clone())
            .collect::<Vec<_>>(),
        vec!["work", "main", "<module>"],
        "no frame of bpd's own is in the stack: {hit}"
    );

    let read = client.call("variables", &serde_json::json!({ "scope": "local" }));
    let total = entry(&read, "total");
    assert_eq!(total["value"]["kind"], "int");
    // an int is carried as **text**, because a python int has no width and a
    // json number that silently became a float would be a different value
    assert_eq!(
        total["value"]["content"]["text"], "2",
        "the locals were {read}"
    );

    let evaluated = client.call(
        "evaluate",
        &serde_json::json!({ "expression": "total * 10" }),
    );
    assert_eq!(evaluated["result"]["value"]["content"]["text"], "20");

    // the write, and the whole point of the test: the program's own output is
    // what proves the compiled code received it
    let written = client.call(
        "set_variable",
        &serde_json::json!({ "scope": "local", "name": "total", "value": "41" }),
    );
    assert_eq!(written["result"]["value"]["content"]["text"], "41");

    // five steps, five tool calls, and nothing else on the wire. an event driven
    // protocol would have cost a request and an event for each of them, and an
    // agent would have had to correlate the two
    let before = client.said();
    for step in 0..5 {
        let stepped = client.call("step_over", &serde_json::json!({ "deadline_ms": GENEROUS }));
        assert_eq!(stepped["outcome"], "stopped", "step {step} gave {stepped}");
    }
    assert_eq!(
        client.said() - before,
        5,
        "five steps cost five answers and nothing else"
    );

    let finished = client.call("continue_", &serde_json::json!({ "deadline_ms": GENEROUS }));
    assert_eq!(finished["outcome"], "exited", "continue_ gave {finished}");
    assert_eq!(finished["exit_code"], 3, "the program calls sys.exit(3)");

    // the program's own stdout came back on the answer rather than into the
    // protocol stream, and it says the write landed
    let said = finished["output"]["text"]
        .as_str()
        .expect("the program printed something");
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
fn a_program_that_never_stops_answers_the_deadline_rather_than_hanging() {
    let fixture = Fixture::new("spinner", SPINNING);
    let mut client = Client::start();

    client.ask("initialize", &serde_json::json!({}));
    client.call(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );

    // nothing can stop this program, so the deadline is the only thing that ends
    // the call. a front end whose answer is the stop and which had no deadline
    // would be a debugger that hung on the first `continue`
    let started = Instant::now();
    let timed_out = client.call("continue_", &serde_json::json!({ "deadline_ms": 400 }));
    let took = started.elapsed();

    assert_eq!(
        timed_out["outcome"], "timed_out",
        "the program never stops and this said {timed_out}"
    );
    assert!(
        took < Duration::from_secs(20),
        "the deadline was 400ms and the call took {took:?}"
    );
    assert!(
        timed_out["waited_ms"].as_u64().expect("it says how long") >= 400,
        "it answered before its own deadline: {timed_out}"
    );
    // it is not a stop, and nothing about a location is claimed. a sampled stack
    // presented as a stopped one is the debugger reporting a state the program
    // was not in
    assert!(
        timed_out.get("frames").is_none()
            && timed_out.get("thread").is_none()
            && timed_out.get("reason").is_none(),
        "a timeout carries no location at all, and carried {timed_out}"
    );
    assert_eq!(
        timed_out["held"],
        serde_json::json!([]),
        "the run resumed everything, so nothing is held: {timed_out}"
    );

    // and asking about the running program is refused with the reason, rather
    // than answered from a sample
    let refused = client.failure("stack", &serde_json::json!({}));
    assert!(
        refused.contains("no thread of the debuggee is held"),
        "a running program cannot be asked for a stack, and said {refused}"
    );

    // a timeout is recoverable: `pause` holds the next thread that reaches a
    // line, and that really is a stop
    let paused = client.call("pause", &serde_json::json!({ "deadline_ms": GENEROUS }));
    assert_eq!(paused["outcome"], "stopped", "the pause gave {paused}");
    // which thread a pause gets, and where it is, belongs to the operating
    // system: this loop calls into `pathlib`, so the line it is held at is as
    // likely to be inside the standard library as in the program
    assert!(
        paused["reason"]["paused"]["line"].is_number(),
        "a pause is held at a line and reported {paused}"
    );
    assert!(
        paused["running"].is_array(),
        "the acknowledgement says which threads were running python: {paused}"
    );

    std::fs::write(fixture.directory().join("stop"), "x").expect("the fixture directory is there");
    let ended = client.call("continue_", &serde_json::json!({ "deadline_ms": GENEROUS }));
    assert_eq!(ended["outcome"], "exited", "continue_ gave {ended}");
    assert_eq!(ended["exit_code"], 0);

    client.finish();
}

/// a program that reaches one line several times
const COUNTING: &str = r#"import sys

total = 0
for step in range(1, 7):
    total = total + step
print("total", total, flush=True)
sys.exit(0)
"#;

#[test]
fn a_hit_condition_the_dap_adapter_cannot_carry_reaches_the_agent_typed() {
    // DAP carries a hit condition as free text whose meaning is a per-client
    // convention, so bpd refuses one there rather than guessing. an MCP tool
    // takes JSON Schema input, so the typed form goes across as itself — this is
    // the one capability the parity test records as unreachable from DAP, and it
    // is only an honest exception if it really is reachable from here
    let fixture = Fixture::new("counter", COUNTING);
    let mut client = Client::start();

    client.ask("initialize", &serde_json::json!({}));
    client.call(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );

    let line = line_of(COUNTING, "total = total + step");
    let set = client.call(
        "set_breakpoints",
        &serde_json::json!({
            "breakpoints": [ {
                "file": fixture.path(),
                "line": line,
                "hits": { "hits": "every", "count": 3 },
            } ],
        }),
    );
    assert_eq!(
        set["breakpoints"][0]["bound"], true,
        "it did not bind: {set}"
    );

    // every third qualifying hit: the third pass, where `total` is 1 + 2 = 3
    // before the line runs, and the sixth, where it is 1..5 = 15
    let mut totals = Vec::new();
    for _ in 0..2 {
        let hit = client.call("continue_", &serde_json::json!({ "deadline_ms": GENEROUS }));
        assert_eq!(hit["outcome"], "stopped", "continue_ gave {hit}");
        // evaluated rather than read out of the module namespace: a module
        // namespace begins with `__builtins__`, and reading one at the default
        // depth spends the whole byte budget on it
        let read = client.call("evaluate", &serde_json::json!({ "expression": "total" }));
        totals.push(
            read["result"]["value"]["content"]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("total is an int and read as {read}"))
                .to_string(),
        );
    }
    assert_eq!(
        totals,
        vec!["3".to_string(), "15".to_string()],
        "the breakpoint stopped on the wrong passes"
    );

    let ended = client.call("continue_", &serde_json::json!({ "deadline_ms": GENEROUS }));
    assert_eq!(ended["outcome"], "exited", "continue_ gave {ended}");
    client.finish();
}

/// the interpreter the built agent matches
fn interpreter() -> String {
    bpd_test::agent::matching_interpreter()
        .executable
        .display()
        .to_string()
}

/// one entry of a `variables` answer, by name
fn entry<'a>(answer: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    answer["entries"]
        .as_array()
        .expect("entries are an array")
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no variable named `{name}` in {answer}"))
}

/// an MCP client, talking to a real `bpd mcp`
///
/// there is a **watchdog**, and it is not decoration. a server that stops
/// answering leaves this reading a pipe that will never say anything, and a
/// suite that hangs is a suite nobody can tell apart from a slow one. the
/// watchdog ends the server, the read returns nothing, and the failure prints
/// the whole conversation up to the point it stopped
struct Client {
    server: Arc<Mutex<Child>>,
    finished: Arc<AtomicBool>,
    /// `None` once the client has hung up, which is how an MCP session ends
    writes: Option<ChildStdin>,
    reads: BufReader<ChildStdout>,
    seen: Vec<serde_json::Value>,
    seq: i64,
}

impl Client {
    fn start() -> Self {
        let mut server = Command::new(BPD)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary was built by the same cargo invocation as this test");

        let writes = server.stdin.take().expect("stdin was asked for");
        let reads = BufReader::new(server.stdout.take().expect("stdout was asked for"));

        let server = Arc::new(Mutex::new(server));
        let finished = Arc::new(AtomicBool::new(false));
        watch(&server, &finished);

        Self {
            server,
            finished,
            writes: Some(writes),
            reads,
            seen: Vec::new(),
            seq: 0,
        }
    }

    /// how many messages the server has written
    fn said(&self) -> usize {
        self.seen.len()
    }

    /// send a request and read its answer
    fn ask(&mut self, method: &str, params: &serde_json::Value) -> serde_json::Value {
        self.seq += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": self.seq, "method": method, "params": params,
        })
        .to_string();
        let writes = self
            .writes
            .as_mut()
            .expect("nothing is sent after the client has hung up");
        writeln!(writes, "{body}").expect("the server is reading its stdin");
        writes.flush().expect("the server is reading its stdin");

        let answered = self.next_message();
        assert_eq!(
            answered["id"], self.seq,
            "this server answers one request at a time, and answered {answered}"
        );
        answered
    }

    /// call one tool and require that it worked, returning the parsed content
    fn call(&mut self, tool: &str, arguments: &serde_json::Value) -> serde_json::Value {
        let answered = self.ask(
            "tools/call",
            &serde_json::json!({ "name": tool, "arguments": arguments }),
        );
        let text = Self::text_of(tool, &answered);
        assert_eq!(
            answered["result"]["isError"], false,
            "`{tool}` failed: {text}"
        );
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("`{tool}` answered with {text}: {error}"))
    }

    /// call one tool and require that it reported a failure, returning the reason
    fn failure(&mut self, tool: &str, arguments: &serde_json::Value) -> String {
        let answered = self.ask(
            "tools/call",
            &serde_json::json!({ "name": tool, "arguments": arguments }),
        );
        let text = Self::text_of(tool, &answered);
        assert_eq!(
            answered["result"]["isError"], true,
            "`{tool}` was expected to fail and answered {text}"
        );
        text
    }

    fn text_of(tool: &str, answered: &serde_json::Value) -> String {
        answered["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("`{tool}` answered with no text: {answered}"))
            .to_string()
    }

    fn next_message(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let read = self
            .reads
            .read_line(&mut line)
            .expect("the server is writing its stdout");
        assert_ne!(
            read, 0,
            "the server said nothing more. either it hung up, or the watchdog \
             ended it after {PATIENCE:?}. what it had said:\n{:#?}",
            self.seen
        );

        let message: serde_json::Value =
            serde_json::from_str(line.trim()).unwrap_or_else(|error| {
                panic!("the server wrote something that is not JSON-RPC: {error}\n{line}")
            });
        self.seen.push(message.clone());
        message
    }

    /// close the client's end and require that the server ended cleanly
    fn finish(mut self) {
        // hanging up is how an MCP session ends. the debuggee does not outlive
        // it: a program left running with nothing watching it is the state the
        // agent inside it refuses to be in
        drop(self.writes.take());

        let deadline = Instant::now() + PATIENCE;
        loop {
            let ended = self
                .server
                .lock()
                .expect("nothing panics holding the server")
                .try_wait()
                .expect("the server is a real child");
            match ended {
                Some(status) => {
                    self.finished.store(true, Ordering::Relaxed);
                    assert!(status.success(), "`bpd mcp` exited with {status}");
                    return;
                }
                None => assert!(
                    Instant::now() < deadline,
                    "`bpd mcp` did not end after its client hung up"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// end the server if the test is still waiting on it after [`PATIENCE`]
fn watch(server: &Arc<Mutex<Child>>, finished: &Arc<AtomicBool>) {
    let server = Arc::clone(server);
    let finished = Arc::clone(finished);
    std::thread::Builder::new()
        .name("mcp-watchdog".to_string())
        .spawn(move || {
            let deadline = Instant::now() + PATIENCE;
            while Instant::now() < deadline {
                if finished.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            end(&server);
        })
        .expect("a watchdog thread can be started");
}

/// end the server, and the debuggee it is holding with it
fn end(server: &Arc<Mutex<Child>>) {
    let mut server = server
        .lock()
        .expect("nothing panics holding the server: every path is a kill or a wait");
    if server
        .try_wait()
        .expect("the server is a real child")
        .is_none()
    {
        let killed = server.kill();
        drop(killed);
        let reaped = server.wait();
        drop(reaped);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // a test that failed part way through leaves a server holding a
        // debuggee, and a debuggee with a held thread never exits. left alone it
        // keeps the harness's own stdio open, and the suite looks hung rather
        // than failed
        self.finished.store(true, Ordering::Relaxed);
        end(&self.server);
    }
}
