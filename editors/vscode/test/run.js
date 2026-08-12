// downloads a real vs code, builds a workspace for it, and runs `session.js`
// inside it
//
// this half runs in plain node and does everything that has to happen before vs
// code exists: it finds the `bpd` binary and the interpreter, checks both are
// ones a session could actually use, and lays out a throwaway workspace holding
// the program to debug and the `bpd.executable` setting pointing at the binary.
// the other half, `session.js`, runs inside the editor and is where the
// assertions are
//
// everything checked here is checked because the alternative is a failure that
// surfaces as vs code saying the debug adapter exited unexpectedly, which names
// nothing a person can act on

const childProcess = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { runTests } = require("@vscode/test-electron");

/**
 * the vs code this runs in
 *
 * pinned rather than `stable` for the reason every action in the workflow is
 * pinned by sha: a suite whose subject changes underneath it cannot say what it
 * tested. `session.js` reads `vscode.debug.activeStackItem`, which is api from
 * 1.89, so the floor is real as well as tidy
 */
const VSCODE = "1.133.0";

/** this extension's directory, which is what vs code is pointed at */
const extension = path.resolve(__dirname, "..");

/** the checkout, for the default location of the built binary */
const repository = path.resolve(extension, "../..");

/** what cargo calls the binary on this platform */
const BINARY = process.platform === "win32" ? "bpd.exe" : "bpd";

/** where `cargo build` leaves it */
const built = path.join(repository, "target", "debug", BINARY);

/** the interpreter the session runs the program under */
const PYTHON = process.env.BPD_PYTHON || "python3";

/** a failure with a cause, told apart from a bug in this file by its name */
class Unusable extends Error {
  constructor(message) {
    super(message);
    this.name = "Unusable";
  }
}

/** the `bpd` to drive, checked to be there before vs code is downloaded */
function executable() {
  const named = process.env.BPD_EXECUTABLE || built;
  if (!path.isAbsolute(named)) {
    throw new Unusable(
      `BPD_EXECUTABLE is \`${named}\`, which is a relative path. the setting `
        + `this becomes, \`bpd.executable\`, refuses one — give an absolute path`,
    );
  }
  let found;
  try {
    found = fs.statSync(named).isFile();
  } catch {
    found = false;
  }
  if (!found) {
    throw new Unusable(
      `there is no bpd binary at \`${named}\`, so there is no debug adapter for `
        + `vs code to start. build one with \`cargo build --bin bpd\`, or set `
        + `BPD_EXECUTABLE to the absolute path of one`,
    );
  }
  return named;
}

/**
 * the interpreter, checked by asking `bpd doctor` about it
 *
 * doctor is the same check a launch makes, so an interpreter that gets past
 * here is one a session can use — and when it does not, the sentence a user
 * reads is doctor's own rather than a second one written here
 */
function interpreter(bpd) {
  const asked = childProcess.spawnSync(bpd, ["doctor", PYTHON], {
    encoding: "utf8",
  });
  if (asked.error) {
    throw new Unusable(
      `\`${bpd} doctor ${PYTHON}\` could not be run: ${asked.error.message}`,
    );
  }
  if (asked.status !== 0) {
    throw new Unusable(
      `\`${PYTHON}\` is not an interpreter bpd can debug, so no session could `
        + `start under it. set BPD_PYTHON to one that is. bpd doctor says:\n\n`
        + `${asked.stdout}${asked.stderr}`,
    );
  }
  return PYTHON;
}

/** a throwaway directory, named after what is in it */
function scratch(what) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `bpd-vscode-${what}-`));
}

/**
 * the workspace vs code opens: the program, and where to find `bpd`
 *
 * the setting is written into the workspace rather than passed to the extension
 * some other way, because reading it out of a workspace is the code path a user
 * takes
 */
function workspace(bpd) {
  const folder = scratch("workspace");
  fs.copyFileSync(
    path.join(__dirname, "program.py"),
    path.join(folder, "program.py"),
  );
  fs.mkdirSync(path.join(folder, ".vscode"));
  fs.writeFileSync(
    path.join(folder, ".vscode", "settings.json"),
    `${JSON.stringify({ "bpd.executable": bpd }, undefined, 2)}\n`,
    "utf8",
  );
  return folder;
}

/** delete a directory, and say so rather than failing the run over it */
function discard(folder) {
  try {
    fs.rmSync(folder, { recursive: true, force: true });
  } catch (reason) {
    console.error(`could not remove \`${folder}\`: ${reason.message}`);
  }
}

async function main() {
  const bpd = executable();
  const python = interpreter(bpd);

  const folder = workspace(bpd);
  const userData = scratch("user-data");
  const extensions = scratch("extensions");

  let failed;
  try {
    await runTests({
      version: VSCODE,
      extensionDevelopmentPath: extension,
      extensionTestsPath: path.join(__dirname, "session.js"),
      // `session.js` is handed the parts of the world it cannot work out for
      // itself. the program's path is the copy in the workspace, so the
      // breakpoint and the launch configuration name the same file vs code has
      // open
      extensionTestsEnv: {
        BPD_TEST_PROGRAM: path.join(folder, "program.py"),
        BPD_TEST_PYTHON: python,
        BPD_TEST_FINISHED: path.join(folder, "finished"),
      },
      // the workspace is the first argument, which is how vs code is told what
      // folder to open. the profile directories are throwaway so that a run
      // cannot read or write the settings of a vs code somebody uses
      launchArgs: [
        folder,
        "--user-data-dir",
        userData,
        "--extensions-dir",
        extensions,
        // other extensions, not this one: vs code keeps loading the extension
        // it was given a development path for. what it stops is a marketplace
        // python extension racing this one for the same debug type
        "--disable-extensions",
      ],
    });
  } catch (reason) {
    failed = reason;
  }

  discard(userData);
  discard(extensions);
  if (failed) {
    // the workspace is what the session ran against, so it is worth more than
    // the disk it costs when something went wrong
    console.error(`the workspace it ran in is left at \`${folder}\``);
    throw failed;
  }
  discard(folder);
}

main().catch((reason) => {
  console.error(
    reason instanceof Unusable
      ? reason.message
      : reason.stack || String(reason),
  );
  process.exit(1);
});
