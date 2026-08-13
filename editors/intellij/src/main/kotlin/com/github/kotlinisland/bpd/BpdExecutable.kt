package com.github.kotlinisland.bpd

import com.intellij.execution.ExecutionException
import com.intellij.openapi.util.SystemInfo
import java.io.File
import java.nio.file.InvalidPathException
import java.nio.file.Path

/**
 * where `bpd` is, given what a run configuration named
 *
 * this is the intellij half of the rule `editors/vscode/extension.js` carries,
 * and it is the same rule: a bare name is looked up on `PATH`, anything else has
 * to be absolute, and a relative path is refused rather than resolved against a
 * directory nobody chose — an IDE's working directory is not one the user picked
 *
 * the `PATH` walk is done here rather than left to the process spawn on purpose.
 * a spawn that cannot find its program fails with a message about a file that
 * does not exist, which says nothing about `PATH` or about how to fix it
 */
object BpdExecutable {
    /** the run configuration field this resolves, as a person reading an error would find it */
    const val FIELD: String = "bpd executable"

    /** the two ways out of every failure below */
    private const val REMEDY: String =
        "put `bpd` on PATH, or set the run configuration's `$FIELD` field to the " +
            "absolute path of the binary"

    /**
     * the `bpd` to start, or an [ExecutionException] saying which lookup failed
     *
     * [ExecutionException] rather than anything of our own because it is what the
     * platform renders in front of the user when a debug session refuses to
     * start
     */
    @Throws(ExecutionException::class)
    fun resolve(named: String): Path {
        val wanted = named.trim()
        if (wanted.isEmpty()) {
            throw ExecutionException(
                "`$FIELD` is empty, so there is no command to start the bpd debug " +
                    "adapter with. set it to `bpd` to look on PATH, or to the absolute " +
                    "path of the binary",
            )
        }

        val path =
            try {
                Path.of(wanted)
            } catch (invalid: InvalidPathException) {
                throw ExecutionException(
                    "`$FIELD` is `$wanted`, which this operating system cannot read as a " +
                        "path: ${invalid.reason}. $REMEDY",
                    invalid,
                )
            }

        if (path.nameCount == 1 && !path.isAbsolute) {
            return onPath(wanted)
        }
        if (!path.isAbsolute) {
            throw ExecutionException(
                "`$FIELD` is `$wanted`, which is a relative path. bpd will not resolve " +
                    "it against a directory nobody chose — give an absolute path, or a " +
                    "bare command name to look up on PATH",
            )
        }
        if (!runnable(path)) {
            throw ExecutionException(
                "`$FIELD` is `$wanted`, and there is no file there this machine would " +
                    "run. $REMEDY",
            )
        }
        return path
    }

    /** every directory `PATH` names, in the order it names them */
    private fun searched(): List<String> =
        (System.getenv("PATH") ?: "").split(File.pathSeparatorChar).filter { it.isNotEmpty() }

    /**
     * the extensions windows would append to a bare command name
     *
     * on windows the extension is what decides whether a file is executable and
     * there is no executable bit to ask about, so `PATHEXT` is the list and its
     * documented default is the fallback. everywhere else there is nothing to
     * append
     */
    private fun extensions(): List<String> =
        if (SystemInfo.isWindows) {
            (System.getenv("PATHEXT") ?: ".COM;.EXE;.BAT;.CMD")
                .split(';')
                .filter { it.isNotEmpty() }
        } else {
            listOf("")
        }

    /** whether a path is a file this platform would run */
    private fun runnable(candidate: Path): Boolean {
        val file = candidate.toFile()
        if (!file.isFile) {
            return false
        }
        return SystemInfo.isWindows || file.canExecute()
    }

    /** where `PATH` says a command is, or the sentence that says it is nowhere */
    @Throws(ExecutionException::class)
    private fun onPath(command: String): Path {
        val directories = searched()
        for (directory in directories) {
            for (extension in extensions()) {
                val candidate =
                    try {
                        Path.of(directory, command + extension)
                    } catch (ignored: InvalidPathException) {
                        // a `PATH` entry this operating system cannot read as a
                        // path is one nothing could be found in, and it is not
                        // this configuration's fault — the walk goes on
                        continue
                    }
                if (runnable(candidate)) {
                    return candidate
                }
            }
        }
        val count = directories.size
        throw ExecutionException(
            "`$command` is not on PATH, so this IDE cannot start the bpd debug adapter " +
                "— bpd is its own adapter, run as `$command dap --listen 0`. $count " +
                "director${if (count == 1) "y" else "ies"} on PATH were searched and none " +
                "has it. $REMEDY",
        )
    }
}
