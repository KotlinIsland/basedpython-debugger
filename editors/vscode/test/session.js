// a debug session, driven from inside a real vs code
//
// this is the half `crates/bpd_dap/tests/vscode.rs` says it cannot reach: that
// the extension loads, that vs code accepts the manifest, and that the
// javascript runs. it runs *in* the editor's extension host, so
// `require("vscode")` is the editor's own api
//
// **the evidence is taken from vs code, not from bpd.** whether the adapter
// answers a `stackTrace` correctly is already settled by the rust suite, and
// re-reading bpd's output here would prove that again instead of the thing in
// question. so what is asserted is the editor's state: that *vs code* activated
// the extension, that *vs code* holds a live session of type `bpd`, that *vs
// code* focused a stack frame because the program stopped. the two reads that
// go over the wire — the stack and a variable — go through
// `DebugSession.customRequest`, which is the session vs code is holding, so the
// thing they prove is that the editor's session routes

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const vscode = require("vscode");

/** the extension under test, as vs code identifies it */
const EXTENSION = "kotlinisland.bpd";

/** the comment in `program.py` that says which line to stop on */
const MARKER = "# bpd: the session stops here";

/**
 * how long any one step of the session is given
 *
 * generous, because it covers downloading nothing but does cover an interpreter
 * starting and an agent being staged on a cold cache. it is a limit, not a
 * measurement: nothing here reports how long anything took
 */
const LIMIT = 60_000;

/** what the runner put in the environment, or a sentence about the one missing */
function required(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(
      `${name} is not set. this file is meant to be run by \`test/run.js\`, `
        + `which sets it — running it any other way skips the setup a session `
        + `needs`,
    );
  }
  return value;
}

/**
 * the first value an event reports that `wanted` accepts
 *
 * the timeout's message names the thing that never happened, because "timed
 * out" on its own says nothing about which step of a session stalled
 */
function next(what, event, wanted = () => true) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      subscription.dispose();
      reject(new Error(`${what} did not happen within ${LIMIT}ms`));
    }, LIMIT);
    const subscription = event((value) => {
      if (!wanted(value)) {
        return;
      }
      clearTimeout(timer);
      subscription.dispose();
      resolve(value);
    });
  });
}

/** whether a stack item is a frame, which is what a stop leaves focused */
function frame(item) {
  return item !== undefined && typeof item.frameId === "number";
}

/** the line holding the marker, as vs code numbers lines — from zero */
function marked(document) {
  for (let line = 0; line < document.lineCount; line += 1) {
    if (document.lineAt(line).text.includes(MARKER)) {
      return line;
    }
  }
  throw new Error(
    `no line of \`${document.uri.fsPath}\` holds \`${MARKER}\`, so there is `
      + `nowhere to put the breakpoint this session is about`,
  );
}

/** one scope's variables, by the name the adapter gave the scope */
async function variables(session, frameId, scope) {
  const scopes = await session.customRequest("scopes", { frameId });
  const wanted = scopes.scopes.find((one) => one.name === scope);
  assert.ok(
    wanted,
    `the session reports no \`${scope}\` scope, only `
      + `${scopes.scopes.map((one) => one.name).join(", ")}`,
  );
  const read = await session.customRequest("variables", {
    variablesReference: wanted.variablesReference,
  });
  return read.variables;
}

async function run() {
  const program = required("BPD_TEST_PROGRAM");
  const python = required("BPD_TEST_PYTHON");
  const finished = required("BPD_TEST_FINISHED");

  // vs code accepted the manifest. if it had not, there is no `bpd` debug type
  // and nothing below could start
  const extension = vscode.extensions.getExtension(EXTENSION);
  assert.ok(
    extension,
    `vs code did not load \`${EXTENSION}\`, so the manifest is not one it `
      + `accepts and no launch configuration can name \`bpd\``,
  );
  assert.equal(
    extension.isActive,
    false,
    "the extension is active before a session was asked for, so what activated "
      + "it was not the `onDebugResolve:bpd` event this is about",
  );

  const folder = vscode.workspace.workspaceFolders?.[0];
  assert.ok(
    folder,
    "vs code opened no folder, so there is no workspace to debug in",
  );

  const document = await vscode.workspace.openTextDocument(
    vscode.Uri.file(program),
  );
  const line = marked(document);

  // set through the editor's api, which is the same thing clicking the gutter
  // does. vs code owns the breakpoint from here and sends it to the adapter
  // itself
  const breakpoint = new vscode.SourceBreakpoint(
    new vscode.Location(document.uri, new vscode.Position(line, 0)),
  );
  vscode.debug.addBreakpoints([breakpoint]);
  assert.ok(
    vscode.debug.breakpoints.some((one) => one.id === breakpoint.id),
    "vs code did not take the breakpoint",
  );

  const started = next(
    "vs code reporting a session started",
    vscode.debug.onDidStartDebugSession,
  );
  const stopped = next(
    "the program stopping at the breakpoint",
    vscode.debug.onDidChangeActiveStackItem,
    frame,
  );

  const accepted = await vscode.debug.startDebugging(folder, {
    type: "bpd",
    request: "launch",
    name: "bpd drives this",
    program,
    python,
    args: [finished],
    stopOnEntry: false,
  });
  assert.equal(
    accepted,
    true,
    "vs code refused to start the configuration. a `bpd` type it cannot resolve "
      + "is the failure this whole file exists to catch",
  );

  // the descriptor factory is registered in `activate`, so a session that
  // started at all is one whose adapter this extension named
  assert.equal(
    extension.isActive,
    true,
    "a `bpd` session started without activating the extension, so the adapter "
      + "vs code is talking to is not the one `extension.js` returned",
  );

  const session = await started;
  assert.equal(session.type, "bpd");
  assert.equal(session.name, "bpd drives this");

  const item = await stopped;
  assert.equal(
    item.session.id,
    session.id,
    "vs code focused a frame belonging to some other session",
  );
  assert.equal(
    vscode.debug.activeDebugSession?.id,
    session.id,
    "vs code holds no active session while stopped at a breakpoint",
  );

  const stack = await session.customRequest("stackTrace", {
    threadId: item.threadId,
  });
  const top = stack.stackFrames[0];
  assert.equal(
    path.resolve(top.source.path),
    path.resolve(program),
    "the session stopped in a file other than the one the breakpoint is in",
  );
  // DAP counts lines from one and vs code counts them from zero, and the
  // breakpoint above was placed by vs code's numbering
  assert.equal(
    top.line,
    line + 1,
    `the session stopped on line ${top.line}, and the breakpoint is on the line `
      + `holding \`${MARKER}\``,
  );
  assert.equal(top.name, "accumulate");

  const locals = await variables(session, item.frameId, "local");
  const total = locals.find((one) => one.name === "total");
  assert.ok(
    total,
    `\`total\` is a local of \`accumulate\` at this line, and the frame reports `
      + `${locals.map((one) => one.name).join(", ")}`,
  );
  assert.equal(total.value, "3", "`total` is 0 + 1 + 2 at the breakpoint");

  // the program has not run past the breakpoint, so it has not written its file
  assert.equal(
    fs.existsSync(finished),
    false,
    "the program reached its last statement before the breakpoint stopped it",
  );

  const ended = next(
    "the session ending",
    vscode.debug.onDidTerminateDebugSession,
    (one) => one.id === session.id,
  );
  // vs code clears the active session *after* it announces the terminate, so
  // this is awaited rather than asserted the moment `ended` resolves — reading
  // it there is reading a state the editor has not finished leaving
  const released = next(
    "vs code releasing the active session",
    vscode.debug.onDidChangeActiveDebugSession,
    (one) => one === undefined,
  );
  // the editor's own continue command rather than a request written here: what
  // a person presses is what is being tested
  await vscode.commands.executeCommand("workbench.action.debug.continue");
  await ended;
  await released;

  assert.equal(
    vscode.debug.activeDebugSession,
    undefined,
    "vs code still holds an active session after the program exited",
  );
  // the program's own word that it ran to the end. a session that was killed
  // rather than resumed leaves no file here
  assert.equal(
    fs.existsSync(finished),
    true,
    "the program never reached its last statement, so the session ended by "
      + "something other than the program exiting",
  );
  assert.equal(fs.readFileSync(finished, "utf8"), "42");

  await refused(folder, program, python);
}

/**
 * a session whose `bpd.executable` names nothing, which must not start
 *
 * this runs after the session above because it is the same workspace with the
 * setting spoiled, and there is no way back to a good one worth writing
 *
 * what it is here for is the sentence. a `bpd` that cannot be found is the
 * likeliest thing to go wrong for a new user, and the whole reason the
 * extension walks `PATH` itself is so that what they read names the command and
 * says what to do — rather than the "debug adapter exited unexpectedly" a
 * failed spawn produces
 */
async function refused(folder, program, python) {
  const missing = path.join(folder.uri.fsPath, "there-is-no-bpd-here");
  await vscode.workspace
    .getConfiguration("bpd", folder)
    .update("executable", missing, vscode.ConfigurationTarget.Workspace);

  const started = [];
  const watching = vscode.debug.onDidStartDebugSession((one) =>
    started.push(one)
  );

  let reported;
  try {
    await vscode.debug.startDebugging(folder, {
      type: "bpd",
      request: "launch",
      name: "bpd cannot be found",
      program,
      python,
    });
  } catch (reason) {
    reported = reason.message;
  } finally {
    watching.dispose();
  }

  assert.ok(
    reported,
    "vs code started a session with no adapter to start, so nothing told the "
      + "user their `bpd.executable` names nothing",
  );
  assert.ok(
    reported.includes(missing),
    `what vs code reported does not name the path that was wrong: ${reported}`,
  );

  // this is the assertion the whole phase is for. what vs code carried to the
  // user is the extension's own sentence, remedy and all — so an error thrown
  // out of `createDebugAdapterDescriptor` **is** rendered, and the extension
  // does not have to show it as well
  //
  // that last part is why `extension.js` no longer calls `showErrorMessage`,
  // and it is worth saying plainly that **the doubling it used to cause cannot
  // be observed from here**. in an extension host the dialog service throws
  // instead of showing anything, so the extension's own call throws too and
  // replaces the error it was about to rethrow — the message that comes back is
  // byte for byte the same either way. it was measured both ways rather than
  // reasoned about, and a check that cannot fail is not written down as if it
  // could
  assert.ok(
    reported.includes(
      "put `bpd` on PATH, or set `bpd.executable` in your settings",
    ),
    `vs code did not put the extension's own sentence in front of the user, so `
      + `what they read is not the one that says what to do: ${reported}`,
  );

  assert.deepEqual(
    started.map((one) => one.name),
    [],
    "a session was started even though its adapter could not be named",
  );
  assert.equal(
    vscode.debug.activeDebugSession,
    undefined,
    "vs code holds an active session that has no adapter behind it",
  );
}

module.exports = { run };
