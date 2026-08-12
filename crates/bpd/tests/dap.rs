//! `bpd dap` driven as a real process, against a real interpreter
//!
//! this is the acceptance for M4: set a breakpoint, hit it, inspect a variable,
//! change it, step, and let the program finish. nothing here takes the
//! adapter's word for anything it can check another way — the write to a local
//! is proved by the **program's own output**, which is what a `f_locals` write
//! the compiled code never reads would not change
//!
//! the transport is the real one too: a child process and `Content-Length`
//! framing. that matters more than it looks, because the debuggee's own stdout
//! is on the same file descriptor the protocol would be on if it were inherited
//! — a single `print` would make every message after it unreadable, and a test
//! that spoke to the adapter in-process would never notice
//!
//! ## both transports, one set of assertions
//!
//! DAP defines two, and `bpd dap` speaks both: stdin and stdout, and a loopback
//! socket a client connects to. every scenario below is a function that takes
//! the [`Transport`], and [`over_each_transport`] gives each one two `#[test]`s
//! — so the assertions that prove a session works exist **once** and are run
//! over both. a transport that quietly did something different from the other
//! would be a second debugger nobody is testing, and there is no way to add one
//! here without both entries appearing
//!
//! what is loopback's alone is at the bottom: what it prints when it binds, and
//! the connections it refuses

use std::io::{BufRead, BufReader, Read as _, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bpd_test::debuggee::{Fixture, line_of};

/// the binary this test run built, not whatever `bpd` is on PATH
const BPD: &str = env!("CARGO_BIN_EXE_bpd");

/// how a client reaches the adapter
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// the adapter is spawned and speaks on the pipes it was given
    Stdio,
    /// the adapter listens on a loopback port and the client connects to it
    Loopback,
}

/// give a scenario a `#[test]` on each transport
///
/// the scenario takes a [`Transport`] rather than a started client, so that one
/// which starts more than one client starts all of them the same way
macro_rules! over_each_transport {
    ($($scenario:ident),* $(,)?) => {
        $(
            mod $scenario {
                #[test]
                fn over_stdio() {
                    super::$scenario(super::Transport::Stdio);
                }

                #[test]
                fn over_loopback_tcp() {
                    super::$scenario(super::Transport::Loopback);
                }
            }
        )*
    };
}

over_each_transport!(
    a_breakpoint_is_hit_a_local_is_written_and_the_program_sees_the_write,
    a_running_program_can_be_paused_while_the_adapter_is_waiting_for_it,
    a_breakpoint_in_a_module_that_is_not_imported_yet_is_pending_and_says_so,
    an_editor_can_run_a_whole_investigation_the_way_an_agent_can,
    an_editor_can_ask_what_changed_between_two_stops,
    an_editor_can_move_where_the_program_carries_on_from,
    a_capability_that_is_not_advertised_is_refused_rather_than_guessed_at,
    a_client_is_refused_the_same_interpreter_the_command_line_is,
);

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

#[expect(
    clippy::too_many_lines,
    reason = "it is one session end to end, and that is the acceptance. \
              splitting it into helpers hides the order the messages go in"
)]
fn a_breakpoint_is_hit_a_local_is_written_and_the_program_sees_the_write(transport: Transport) {
    let fixture = Fixture::new("dapped", PROGRAM);
    let mut client = Client::start(transport);

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

fn a_running_program_can_be_paused_while_the_adapter_is_waiting_for_it(transport: Transport) {
    // the request that cannot go the way every other one goes: the agent
    // answers on a thread it is holding, and a running program has none. so it
    // is delivered on an interrupt while the session is blocked reading its
    // connection, and this is what proves that path is real
    let fixture = Fixture::new("spinner", SPINNING);
    let mut client = Client::start(transport);

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

fn a_breakpoint_in_a_module_that_is_not_imported_yet_is_pending_and_says_so(transport: Transport) {
    // "the breakpoint is set" is the easiest thing in a debugger to claim
    // wrongly. a module the program has not imported has no code object behind
    // it, and DAP has a word for exactly that state
    let fixture = Fixture::new(
        "importer",
        "import later\n\nprint(later.value(), flush=True)\n",
    );
    let sibling = fixture.sibling("later", "def value():\n    return 7\n");

    let mut client = Client::start(transport);
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

fn an_editor_can_run_a_whole_investigation_the_way_an_agent_can(transport: Transport) {
    // the parity rule, at the far end: a debug script is a capability of the
    // core, so it is not an agent's alone. DAP has no request of its own for one
    // and never will, so it is an extension — and a client sends it with the
    // `customRequest` every DAP client has
    let fixture = Fixture::new("scripted", PROGRAM);
    let mut client = Client::start(transport);

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

fn an_editor_can_ask_what_changed_between_two_stops(transport: Transport) {
    // the parity rule again: the declarative query and the difference between
    // two of its answers are capabilities of the core, so they are not an
    // agent's alone. "what changed between here and there" is a thing a person
    // wants, and no editor offers it
    let fixture = Fixture::new("compared", PROGRAM);
    let mut client = Client::start(transport);

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
            "breakpoints": [ { "line": line_of(PROGRAM, "doubled = total * 2") } ],
        }),
    );
    client.request("configurationDone", &serde_json::json!({}));

    let stopped = client.event("stopped");
    let thread = stopped["body"]["threadId"].clone();

    let before = client.request(
        "bpd/state",
        &serde_json::json!({
            "threadId": thread,
            "query": { "frames": 1, "scopes": ["local"], "source": 1 },
        }),
    );
    assert_eq!(before["success"], true, "the query was refused: {before}");
    assert_eq!(
        before["body"]["state"]["frames"][0]["frame"]["function"],
        "work"
    );
    assert_eq!(
        before["body"]["state"]["frames"][0]["frame"]["kind"], "python",
        "a frame says what it is, so nothing reads a synthesised one as real"
    );

    // one step, and the line the program is on has run: `doubled` was unbound
    // and now holds a number
    client.request("next", &serde_json::json!({ "threadId": thread }));
    client.event("stopped");
    let after = client.request(
        "bpd/state",
        &serde_json::json!({
            "threadId": thread,
            "query": { "frames": 1, "scopes": ["local"] },
        }),
    );

    let difference = client.request(
        "bpd/diff",
        &serde_json::json!({
            "before": before["body"]["id"],
            "after": after["body"]["id"],
        }),
    );
    assert_eq!(
        difference["success"], true,
        "the difference was refused: {difference}"
    );

    let changed = difference["body"]["changed"]
        .as_array()
        .expect("changed is an array");
    assert!(
        changed.iter().any(|change| {
            change["subject"]["name"] == "doubled"
                && change["before"]["seen"] == "unbound"
                && change["after"]["value"]["content"]["text"] == "4"
        }),
        "the line assigned `doubled`, and it was unbound before it ran: \
         {difference}"
    );

    client.request("continue", &serde_json::json!({ "threadId": thread }));
    client.event("exited");
    // the program's own words, which is what says the numbers above were real
    assert!(
        client.output().contains("doubled 4"),
        "the program printed what the diff said it computed"
    );
    client.event("terminated");
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

/// a program where running a line twice is visible in what it prints
const REPEATING: &str = r#"import sys


def work(seed):
    total = seed + 1
    total = total + 10
    doubled = total * 2
    print("total", total, flush=True)
    print("doubled", doubled, flush=True)
    return total, doubled


work(1)
sys.exit(0)
"#;

fn an_editor_can_move_where_the_program_carries_on_from(transport: Transport) {
    // set next statement, as an editor really reaches it: `gotoTargets` for the
    // line the user picked, then `goto` with the target it minted. what proves
    // the move happened is the **program's own output** — a line that ran twice
    // prints a different number
    let fixture = Fixture::new("moving", REPEATING);
    let elsewhere = fixture.sibling("elsewhere", "value = 1\n");
    let mut client = Client::start(transport);

    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");

    let again = line_of(REPEATING, "total = total + 10");
    let held = line_of(REPEATING, "doubled = total * 2");
    client.request(
        "setBreakpoints",
        &serde_json::json!({
            "source": { "path": fixture.path() },
            "breakpoints": [ { "line": held } ],
        }),
    );
    client.request("configurationDone", &serde_json::json!({}));

    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint");
    let thread = stopped["body"]["threadId"].clone();

    // a line in a file no held thread is executing has no target. a line number
    // means nothing without its file, and cpython would take the same number
    // against whatever file the frame happens to be running
    let none = client.request(
        "gotoTargets",
        &serde_json::json!({ "source": { "path": elsewhere }, "line": again }),
    );
    assert_eq!(
        none["body"]["targets"],
        serde_json::json!([]),
        "a file nothing is executing was offered a place to move to: {none}"
    );

    let targets = client.request(
        "gotoTargets",
        &serde_json::json!({ "source": { "path": fixture.path() }, "line": again }),
    );
    let target = targets["body"]["targets"][0].clone();
    assert_eq!(
        target["line"], again,
        "the target has to be the line the client asked about: {targets}"
    );
    assert!(
        target["label"]
            .as_str()
            .expect("a target has a label")
            .contains("work"),
        "the label has to say which frame it is about: {targets}"
    );

    client.request(
        "goto",
        &serde_json::json!({ "threadId": thread, "targetId": target["id"] }),
    );

    // the thread was never resumed, so DAP's answer to a move is a `stopped`
    // event of its own — and this is where a client learns where the frame is
    let moved = client.event("stopped");
    assert_eq!(moved["body"]["reason"], "goto", "the event was {moved}");
    assert_eq!(moved["body"]["threadId"], thread);

    // and the stack agrees. no line event is delivered for the line a jump
    // moves to, so this is derived from the move rather than waited for
    let stack = client.request("stackTrace", &serde_json::json!({ "threadId": thread }));
    assert_eq!(
        stack["body"]["stackFrames"][0]["line"], again,
        "the stack was {stack}"
    );

    // the breakpoint is on a line the move went back **over**, so it fires
    // again when that line runs again. only the destination's own line is
    // passed over, and only for the pass the jump landed in
    client.request("continue", &serde_json::json!({ "threadId": thread }));
    let again_at = client.event("stopped");
    assert_eq!(
        again_at["body"]["reason"], "breakpoint",
        "a breakpoint on a line the jump went back over has to fire when the \
         line runs again: {again_at}"
    );

    client.request(
        "setBreakpoints",
        &serde_json::json!({
            "source": { "path": fixture.path() },
            "breakpoints": [],
        }),
    );
    client.request("continue", &serde_json::json!({ "threadId": thread }));
    let exited = client.event("exited");
    assert_eq!(exited["body"]["exitCode"], 0);
    client.event("terminated");

    let said = client.output();
    assert!(
        said.contains("total 22"),
        "the line the frame moved to did not run again: {said:?}"
    );
    assert!(
        said.contains("doubled 44"),
        "the lines after the destination ran against the old value: {said:?}"
    );

    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

fn a_capability_that_is_not_advertised_is_refused_rather_than_guessed_at(transport: Transport) {
    let fixture = Fixture::new("hit", "x = 1\ny = 2\n");
    let mut client = Client::start(transport);

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
    writes: Box<dyn Write + Send>,
    reads: Box<dyn BufRead + Send>,
    /// the port this adapter bound, when it is listening on one
    listening: Option<Listener>,
    /// the token to put on the next message, until it has been presented
    ///
    /// only the loopback transport has one. a pipe has exactly one writer and
    /// whoever spawned the adapter chose it, so there is nothing for a token to
    /// separate
    token: Option<String>,
    seen: Vec<serde_json::Value>,
    /// how much of `seen` the test has already looked at for an event
    taken: usize,
    seq: i64,
}

/// a `bpd dap --listen 0` that has bound, before anything has connected to it
///
/// separate from [`Client`] because the interesting refusals happen to a
/// connection that never becomes a client, and one of them has to be the
/// **first** connection to prove the listener carries on waiting afterwards
struct Listener {
    process: Arc<Mutex<Child>>,
    finished: Arc<AtomicBool>,
    endpoint: SocketAddr,
    token: String,
    /// the adapter's own stdout, held open
    ///
    /// the announcement is the only thing it ever writes there, and a closed
    /// read end would turn a stray write into a signal instead of a failure
    _stdout: BufReader<ChildStdout>,
}

impl Listener {
    /// spawn a listening adapter and read the line it prints when it binds
    ///
    /// this is the whole reason the port is reported: the adapter binds `0`, the
    /// operating system picks, and the number comes back here. a test that had
    /// to choose a port would be racing every other test and every other
    /// program on the machine for it
    fn start() -> Self {
        let mut adapter = Command::new(BPD)
            .args(["dap", "--listen", "0"])
            // a listening adapter never reads its stdin, and a pipe left open
            // for it would only be something to close later
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary was built by the same cargo invocation as this test");
        let mut stdout = BufReader::new(adapter.stdout.take().expect("stdout was asked for"));

        let process = Arc::new(Mutex::new(adapter));
        let finished = Arc::new(AtomicBool::new(false));
        watch(&process, &finished);

        let mut line = String::new();
        let read = stdout
            .read_line(&mut line)
            .expect("a listening adapter says where it bound before it accepts anything");
        assert_ne!(
            read, 0,
            "`bpd dap --listen 0` ended without saying where it had bound"
        );

        let said: serde_json::Value = serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("the announcement is one line of json: {error}\n{line}")
        });
        assert_eq!(
            said["listening"]["host"], "127.0.0.1",
            "a DAP message runs the debuggee's own code, so this binds loopback \
             and nothing else: {said}"
        );

        let port = said["listening"]["port"]
            .as_u64()
            .unwrap_or_else(|| panic!("the announcement names a port: {said}"));
        let port = u16::try_from(port).expect("a port is sixteen bits");
        assert_ne!(port, 0, "nothing can connect to the port that means any");

        let token = said["listening"]["token"]
            .as_str()
            .unwrap_or_else(|| panic!("the announcement names the token to present: {said}"))
            .to_string();

        Self {
            process,
            finished,
            endpoint: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            token,
            _stdout: stdout,
        }
    }

    /// become this adapter's client, presenting the token it asked for
    fn client(self) -> Client {
        let socket =
            TcpStream::connect(self.endpoint).expect("the adapter said it had bound that endpoint");
        Client {
            adapter: Arc::clone(&self.process),
            finished: Arc::clone(&self.finished),
            writes: Box::new(socket.try_clone().expect("a connected socket clones")),
            reads: Box::new(BufReader::new(socket)),
            token: Some(self.token.clone()),
            listening: Some(self),
            seen: Vec::new(),
            taken: 0,
            seq: 0,
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // the same reason [`Client`] has one: an adapter holding a debuggee
        // with a held thread never exits on its own
        self.finished.store(true, Ordering::Relaxed);
        end(&self.process);
    }
}

impl Client {
    fn start(transport: Transport) -> Self {
        match transport {
            Transport::Stdio => Self::spawned(),
            Transport::Loopback => Listener::start().client(),
        }
    }

    /// the adapter as an editor starts one: spawned, speaking on its pipes
    fn spawned() -> Self {
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
            writes: Box::new(writes),
            reads: Box::new(reads),
            listening: None,
            token: None,
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

        // the token goes on the first message and only the first: it
        // authenticates the connection, and the connection is the session
        let presenting = match self.token.take() {
            Some(token) => format!("X-Bpd-Token: {token}\r\n"),
            None => String::new(),
        };
        write!(
            self.writes,
            "Content-Length: {}\r\n{presenting}\r\n{body}",
            body.len()
        )
        .expect("the adapter is reading its end of the connection");
        self.writes
            .flush()
            .expect("the adapter is reading its end of the connection");

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

    /// where this adapter is listening, and what a connection to it presents
    ///
    /// what a **second** connection needs, which is what a `startDebugging`
    /// reverse request asks a client to open: one listener, one token, and as
    /// many sessions of the debuggee as it has
    fn listening_at(&self) -> (SocketAddr, String) {
        let listening = self
            .listening
            .as_ref()
            .expect("this client was started on the loopback transport");
        (listening.endpoint, listening.token.clone())
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

/// the refusal a `launch` request gets is the one the command line gets
///
/// the check is in `bpd_engine::launch::start`, which every front end goes
/// through, so this is not a second implementation to keep in step — it is the
/// assertion that there is only one. an adapter that swallowed the reason and
/// answered "launch failed" would leave a client with no way to tell an
/// unsupported interpreter from a missing file, and the parity rule is that an
/// agent can do everything a human can, including *know why*
fn a_client_is_refused_the_same_interpreter_the_command_line_is(transport: Transport) {
    for capabilities in bpd_test::discovered().unsupported() {
        // a program that would announce itself, so "it did not run" is
        // something the test observes rather than infers
        let fixture = Fixture::new("never_reached", "print('the-program-ran')\n");
        let mut client = Client::start(transport);

        client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
        let refused = client.request(
            "launch",
            &serde_json::json!({
                "program": fixture.path(),
                "python": capabilities.executable,
            }),
        );

        assert_eq!(
            refused["success"], false,
            "the adapter accepted a launch on python {}, which cannot be \
             debugged: {refused}",
            capabilities.version
        );

        let message = refused["message"]
            .as_str()
            .unwrap_or_else(|| panic!("a failed response carries a message: {refused}"));
        assert!(
            message.contains(&capabilities.version.to_string()),
            "the client must be told which version it got, so it can say so \
             without probing the interpreter itself, got:\n{message}"
        );
        assert!(
            message.contains(&bpd_core::python::MINIMUM_SUPPORTED.to_string()),
            "the client must be told the minimum, got:\n{message}"
        );
        assert!(
            !client.output().contains("the-program-ran"),
            "the program ran before the adapter refused the interpreter"
        );
    }
}

/// a connection that is not the adapter's client
///
/// a raw socket rather than a [`Client`], because what these tests are about is
/// what a connection is told *before* it becomes a session — and one of them
/// never sends anything at all
struct Bystander {
    socket: TcpStream,
}

impl Bystander {
    fn connect(endpoint: SocketAddr) -> Self {
        let socket = TcpStream::connect(endpoint).expect("the adapter is listening there");
        // a refusal that never arrives is the hang this whole shape exists to
        // rule out, so a read that waits is a failure rather than a wait
        socket
            .set_read_timeout(Some(PATIENCE))
            .expect("a connected socket takes a read timeout");
        Self { socket }
    }

    /// send one framed message, with whatever headers were asked for
    fn send(&mut self, headers: &str, body: &str) {
        self.send_raw(&format!(
            "Content-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        ));
    }

    /// send bytes that are not necessarily a DAP message at all
    fn send_raw(&mut self, text: &str) {
        self.socket
            .write_all(text.as_bytes())
            .expect("the adapter is reading the connection");
        self.socket.flush().expect("the adapter is reading it");
    }

    /// the next message of any kind
    fn next_message(&mut self) -> serde_json::Value {
        let mut reader = BufReader::new(&self.socket);
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .unwrap_or_else(|error| panic!("the adapter said nothing back: {error}"));
            assert_ne!(
                read, 0,
                "the connection was closed without being told why it was refused"
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
        reader
            .read_exact(&mut body)
            .expect("the adapter wrote as many bytes as it promised");
        serde_json::from_slice(&body).expect("what the adapter wrote is a DAP message")
    }

    /// the text of the next `output` event, whatever else arrives first
    fn told(&mut self) -> String {
        let message = self.next_message();
        assert_eq!(
            message["event"], "output",
            "an unsolicited message is an event, since a refused connection sent \
             nothing this adapter would answer: {message}"
        );
        message["body"]["output"]
            .as_str()
            .unwrap_or_else(|| panic!("an output event carries text: {message}"))
            .to_string()
    }

    /// require that the adapter hung up rather than holding the connection
    ///
    /// what is left to read does not matter; that the read **ends** does. a
    /// refused connection left open is a client that has been told no and is
    /// still waiting to be told something else
    fn hung_up(&mut self) {
        let mut rest = Vec::new();
        if let Err(error) = self.socket.read_to_end(&mut rest) {
            panic!("a refused connection is closed, not held open: {error}");
        }
    }
}

/// a program that runs long enough to be interrupted by a second connection
const WAITING: &str = "import sys\nx = 1\nsys.exit(0)\n";

/// one raw connection that presents this listener's token, and its `initialize`
///
/// enough to establish that a connection was **served** rather than turned away,
/// which is the whole of what a second one used to be denied
fn admitted(endpoint: SocketAddr, token: &str, seq: i64) -> (Bystander, serde_json::Value) {
    let mut connection = Bystander::connect(endpoint);
    connection.send(
        &format!("X-Bpd-Token: {token}\r\n"),
        &format!(
            r#"{{"seq":{seq},"type":"request","command":"initialize","arguments":{{"adapterID":"bpd"}}}}"#
        ),
    );
    let answered = connection.next_message();
    (connection, answered)
}

#[test]
fn a_second_client_that_presents_the_token_is_served_beside_the_first() {
    // this used to be refused: one listener meant one session, and a second
    // connection was told the adapter was busy. that was right then and is
    // wrong now — a debugged fork is a second session of the **same** debuggee,
    // and the `startDebugging` reverse request asks the client to open exactly
    // this connection. an adapter that refused it would be turning away the
    // thing it had just asked for
    let fixture = Fixture::new("shared", WAITING);
    let mut client = Client::start(Transport::Loopback);
    let (endpoint, token) = client.listening_at();

    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");

    let (mut second, answered) = admitted(endpoint, &token, 1);
    assert_eq!(
        answered["type"], "response",
        "a second connection is served, so what it gets is the answer to what it \
         sent: {answered}"
    );
    assert_eq!(answered["command"], "initialize", "{answered}");
    assert_eq!(answered["success"], true, "{answered}");

    // and it reached the **same** launcher, which is the point of it being a
    // second connection rather than a second adapter: this debuggee is already
    // launched, and a second program on it is refused by name
    second.send(
        "",
        &format!(
            r#"{{"seq":2,"type":"request","command":"launch","arguments":{{"program":"{}"}}}}"#,
            fixture.path().display()
        ),
    );
    let refused = second.next_message();
    assert_eq!(refused["success"], false, "{refused}");
    let said = refused["message"]
        .as_str()
        .unwrap_or_else(|| panic!("a refusal says why: {refused}"));
    assert!(
        said.contains("already has a program"),
        "the refusal has to say what stood in the way, and said {said:?}"
    );
    assert!(
        said.contains("startDebugging"),
        "and what a second connection is for, and said {said:?}"
    );

    // and the session that was already running is untouched by any of it
    client.request("configurationDone", &serde_json::json!({}));
    let exited = client.event("exited");
    assert_eq!(exited["body"]["exitCode"], 0);
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

#[test]
fn a_connection_that_says_nothing_takes_no_slot_and_holds_nothing_up() {
    // the half of the old refusal that still matters. anything on this machine
    // can open a socket to a loopback port, and one that connects and then says
    // nothing must not be able to stop the session a `startDebugging` asked a
    // client to start — which it could if the wait for a token happened on the
    // thread that accepts
    let fixture = Fixture::new("silent", WAITING);
    let mut client = Client::start(Transport::Loopback);
    let (endpoint, token) = client.listening_at();

    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");

    // it says nothing at all, and is deliberately still connected below: this
    // is not about it eventually being dropped on its deadline, it is about it
    // not being in the way while it sits there
    let _silent = Bystander::connect(endpoint);

    let (_admitted, answered) = admitted(endpoint, &token, 1);
    assert_eq!(
        answered["command"], "initialize",
        "a connection that presented the token was answered while a silent one \
         was still open: {answered}"
    );
    assert_eq!(answered["success"], true, "{answered}");

    // and the session that was already running is untouched by both of them
    client.request("configurationDone", &serde_json::json!({}));
    let exited = client.event("exited");
    assert_eq!(exited["body"]["exitCode"], 0);
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

#[test]
fn a_connection_with_no_token_or_the_wrong_one_is_refused_and_the_listener_waits_on() {
    // the token is the whole of what separates this adapter's client from any
    // other process that can reach loopback, and a DAP message runs the
    // debuggee's own code. so: refused, told why — and the listener carries on,
    // because a bad connection that could end the adapter or take its one slot
    // would let anything that reaches the port stop the session being started
    let listener = Listener::start();
    let endpoint = listener.endpoint;
    let initialize = r#"{"seq":1,"type":"request","command":"initialize"}"#;

    let mut nothing = Bystander::connect(endpoint);
    nothing.send("", initialize);
    let told = nothing.told();
    assert!(
        told.contains("x-bpd-token"),
        "the refusal has to name the header, and said {told:?}"
    );
    assert!(
        told.contains("--listen"),
        "and where the token comes from, and said {told:?}"
    );
    nothing.hung_up();

    let mut guessing = Bystander::connect(endpoint);
    guessing.send(
        &format!("X-Bpd-Token: {}\r\n", "f".repeat(listener.token.len())),
        initialize,
    );
    let told = guessing.told();
    assert!(
        told.contains("not this session's token"),
        "a wrong token is named as one, and said {told:?}"
    );
    assert!(
        !told.contains(&listener.token[..8]),
        "a refusal must not quote the token back at whoever is guessing: {told:?}"
    );
    guessing.hung_up();

    // and after both, the client that does present the token gets a session
    let fixture = Fixture::new("admitted", WAITING);
    let mut client = listener.client();
    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");
    client.request("configurationDone", &serde_json::json!({}));
    let exited = client.event("exited");
    assert_eq!(
        exited["body"]["exitCode"], 0,
        "two refused connections left the listener able to serve a real one"
    );
    client.request("disconnect", &serde_json::json!({}));
    client.finish();
}

#[test]
fn a_port_that_cannot_be_bound_names_the_port_and_what_to_do_instead() {
    // the flag's value is really used, and a listener that could not start says
    // so rather than exiting with nothing on either stream
    let taken = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .expect("loopback binds an arbitrary port");
    let port = taken
        .local_addr()
        .expect("a bound listener has an address")
        .port();

    let refused = Command::new(BPD)
        .args(["dap", "--listen", &port.to_string()])
        .stdin(Stdio::null())
        .output()
        .expect("the binary was built by the same cargo invocation as this test");

    assert!(
        !refused.status.success(),
        "the adapter reported success on a port it never bound"
    );
    assert!(
        refused.stdout.is_empty(),
        "nothing announces an endpoint that does not exist, and it said {:?}",
        String::from_utf8_lossy(&refused.stdout)
    );

    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains(&port.to_string()), "said {said}");
    assert!(
        said.contains("--listen 0"),
        "the refusal has to say what to do instead, and said {said}"
    );
}

#[test]
fn there_is_no_way_to_ask_this_adapter_to_listen_anywhere_but_loopback() {
    // the security decision, as the command line: `--listen` takes a **port**,
    // so a wildcard bind is not expressible rather than merely defaulted away
    // from. reaching this port is running code as whoever started bpd, and an
    // address that could be widened is one that eventually is
    for address in ["0.0.0.0:5678", "0.0.0.0", "::", "127.0.0.1:5678"] {
        let refused = Command::new(BPD)
            .args(["dap", "--listen", address])
            .stdin(Stdio::null())
            .output()
            .expect("the binary was built by the same cargo invocation as this test");

        assert!(
            !refused.status.success(),
            "`--listen {address}` was accepted, so there is an address to widen"
        );
        assert!(
            refused.stdout.is_empty(),
            "`--listen {address}` announced an endpoint: {:?}",
            String::from_utf8_lossy(&refused.stdout)
        );
    }

    // and no flag alongside it offers one either
    let help = Command::new(BPD)
        .args(["dap", "--help"])
        .output()
        .expect("the binary was built by the same cargo invocation as this test");
    let said = String::from_utf8_lossy(&help.stdout);
    assert!(
        said.contains("127.0.0.1"),
        "`bpd dap --help` has to say where it listens, and said {said}"
    );
    for widening in ["--host", "--address", "--bind"] {
        assert!(
            !said.contains(widening),
            "`{widening}` would be a way to widen the address, and it is offered: {said}"
        );
    }
}

#[test]
fn a_request_shaped_like_a_browser_fetch_is_refused_before_anything_in_it_is_read() {
    // this is the sharp edge the token is for, and it is why loopback is not a
    // trust boundary. a page can POST to 127.0.0.1 with `text/plain` and no
    // preflight, and this framing is HTTP shaped enough that a request line
    // with a colon in its path parses as an ordinary header — so the body that
    // follows would be a whole DAP message. a DAP message runs the debuggee's
    // own code. what a page cannot do is obtain this session's token, because
    // the same origin policy stops it reading anything back
    let listener = Listener::start();
    let mut page = Bystander::connect(listener.endpoint);

    let body = r#"{"seq":1,"type":"request","command":"evaluate","arguments":{"expression":"1"}}"#;
    page.send_raw(&format!(
        "POST /a:b HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://example.invalid\r\n\
         Content-Type: text/plain;charset=UTF-8\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));

    let told = page.told();
    assert!(
        told.contains("x-bpd-token"),
        "the request was answered rather than refused, and said {told:?}"
    );
    page.hung_up();
}

#[test]
fn a_connection_that_presents_nothing_is_dropped_rather_than_holding_the_listener() {
    // the one thing a token cannot stop on its own: a process that connects and
    // then says nothing. left alone it would hold the slot for as long as it
    // liked, and the client the person is waiting for would never be reached.
    // so there is a deadline on presenting one, and when it passes the listener
    // goes back to waiting rather than staying stuck on it
    let listener = Listener::start();
    let silent = TcpStream::connect(listener.endpoint).expect("the adapter is listening there");

    // this one connects after, so it cannot be admitted until the silent one is
    // let go of
    let fixture = Fixture::new("after_a_silence", WAITING);
    let mut client = listener.client();
    client.request("initialize", &serde_json::json!({ "adapterID": "bpd" }));
    client.request(
        "launch",
        &serde_json::json!({ "program": fixture.path(), "python": interpreter() }),
    );
    client.event("initialized");
    client.request("configurationDone", &serde_json::json!({}));
    let exited = client.event("exited");
    assert_eq!(
        exited["body"]["exitCode"], 0,
        "a connection that never presented a token stopped the session that did"
    );

    client.request("disconnect", &serde_json::json!({}));
    client.finish();
    drop(silent);
}
