// the whole extension: it says where `bpd` is, and contributes nothing else
//
// vs code resolves a launch configuration's `type` through an extension, and
// there is no way to name an adapter executable from `launch.json` alone. the
// `debuggers` entry in `package.json` is that registration; this file is the
// half of it that cannot be declared, because the answer to "where is bpd" is
// a lookup rather than a constant
//
// everything a session does after that is DAP, which `bpd dap` already speaks

const fs = require("node:fs");
const path = require("node:path");
const vscode = require("vscode");

/** the setting that names the executable, in the form a user would search for */
const SETTING = "bpd.executable";

/** what a failure to find it should tell someone to do about it */
const REMEDY =
  `put \`bpd\` on PATH, or set \`${SETTING}\` in your settings to its `
  + `absolute path`;

/**
 * whether a path is a file this platform would run
 *
 * on windows the extension is what decides, and there is no executable bit to
 * ask about — `fs.accessSync(X_OK)` answers for the file's ACL in a way that
 * does not correspond to whether the shell would run it
 */
function runnable(candidate) {
  try {
    if (!fs.statSync(candidate).isFile()) {
      return false;
    }
  } catch {
    return false;
  }
  if (process.platform === "win32") {
    return true;
  }
  try {
    fs.accessSync(candidate, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/** every directory PATH names, in the order it names them */
function searched() {
  return (process.env.PATH || "").split(path.delimiter).filter(Boolean);
}

/**
 * where PATH says a command is, or `undefined`
 *
 * the lookup is done here rather than left to the process spawn so that not
 * finding it is a sentence about PATH and this setting, instead of the
 * "the debug adapter exited unexpectedly" a failed spawn produces
 */
function onPath(command) {
  const extensions = process.platform === "win32"
    ? (process.env.PATHEXT || ".COM;.EXE;.BAT;.CMD")
      .split(";")
      .filter(Boolean)
    : [""];
  for (const directory of searched()) {
    for (const extension of extensions) {
      const candidate = path.join(directory, command + extension);
      if (runnable(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

/**
 * the `bpd` to run, or an error saying which lookup failed and what to do
 *
 * a bare name is looked up on PATH. anything else has to be absolute: a
 * relative path would be resolved against whatever directory vs code happened
 * to be started in, which is not a directory the user chose
 */
function resolve(folder) {
  const configured = vscode.workspace
    .getConfiguration("bpd", folder)
    .get("executable");
  const named = typeof configured === "string" ? configured.trim() : "";
  if (named === "") {
    throw new Error(
      `\`${SETTING}\` is empty, so there is no command to start the bpd debug `
        + `adapter with. set it to \`bpd\` to look on PATH, or to the absolute `
        + `path of the binary`,
    );
  }

  const bare = path.basename(named) === named;
  if (!bare) {
    if (!path.isAbsolute(named)) {
      throw new Error(
        `\`${SETTING}\` is \`${named}\`, which is a relative path. bpd will `
          + `not resolve it against a directory nobody chose — give an absolute `
          + `path, or a bare command name to look up on PATH`,
      );
    }
    if (!runnable(named)) {
      throw new Error(
        `\`${SETTING}\` is \`${named}\`, and there is no file there this `
          + `machine would run. ${REMEDY}`,
      );
    }
    return named;
  }

  const found = onPath(named);
  if (found === undefined) {
    const directories = searched().length;
    throw new Error(
      `\`${named}\` is not on PATH, so vs code cannot start the bpd debug `
        + `adapter — bpd is its own adapter, run as \`${named} dap\`. `
        + `${directories} director${directories === 1 ? "y" : "ies"} on PATH `
        + `were searched and none has it. ${REMEDY}`,
    );
  }
  return found;
}

/** the adapter descriptor for one session, or the reason there is none */
const factory = {
  createDebugAdapterDescriptor(session) {
    const folder = session.workspaceFolder;
    // throwing is the whole of the failure path: it stops the session, and vs
    // code puts the message in front of the user itself. that last part used to
    // be a guess, and the error was shown with `showErrorMessage` here as well
    // in case it was wrong — until `test/session.js` drove a session whose
    // `bpd.executable` names nothing and found vs code rendering the thrown
    // error, so the belt and the braces were the same sentence twice
    const command = resolve(folder);
    // the debuggee inherits the adapter's working directory, and a program can
    // see it. the folder the configuration came from is the one a user means;
    // whatever vs code itself was started in is not
    return new vscode.DebugAdapterExecutable(command, ["dap"], {
      cwd: folder ? folder.uri.fsPath : undefined,
    });
  },
};

function activate(context) {
  context.subscriptions.push(
    vscode.debug.registerDebugAdapterDescriptorFactory("bpd", factory),
  );
}

module.exports = { activate };
