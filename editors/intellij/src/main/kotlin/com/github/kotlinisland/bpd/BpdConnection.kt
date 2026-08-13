package com.github.kotlinisland.bpd

import com.google.gson.JsonParser
import com.google.gson.JsonSyntaxException
import com.intellij.execution.ExecutionException
import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.openapi.diagnostic.logger
import com.intellij.platform.dap.connection.DebugAdapterHandle
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import java.io.BufferedReader
import java.io.InputStream
import java.io.InputStreamReader
import java.io.OutputStream
import java.net.InetSocketAddress
import java.net.Socket
import java.nio.charset.StandardCharsets
import java.util.concurrent.TimeUnit
import kotlin.time.Duration.Companion.seconds

/**
 * a `bpd dap --listen 0` process, and the connection to it
 *
 * ## why a socket and not the pipes
 *
 * `CommandLineDebugAdapterHandle` exists in the platform and would have been one
 * line. it speaks to an adapter on its stdin and stdout, and that is the
 * transport `debugChildren` cannot be delivered on: DAP hands a second program
 * to a client with the `startDebugging` reverse request, which asks the client
 * to open a **second connection**, and nothing can open a second connection to a
 * pair of pipes somebody else spawned. `bpd dap` refuses `debugChildren` by name
 * on that transport rather than half delivering it, so the socket is what keeps
 * the refusal from being permanent
 *
 * ## the announcement
 *
 * `--listen 0` binds a port the operating system chooses, which is the only way
 * to start a listener without racing something else for a number. the port that
 * was really bound comes back on stdout as one line of json — and with it the
 * token, because a loopback port that runs the debuggee's code is reachable by
 * every other process on the machine
 *
 * ## the token
 *
 * bpd wants it as a header on the **first message**, and there is no way to ask
 * the platform's lsp4j launcher to add a header. so it is written onto the
 * socket here, before the launcher writes anything: a DAP header block is
 * `Name: value` lines terminated by a blank line and `Content-Length` was never
 * required to be the first of them, so `X-Bpd-Token` on the line above it is one
 * header block with two headers rather than two messages
 */
class BpdConnection private constructor(
    private val process: Process,
    private val socket: Socket,
) : DebugAdapterHandle {
    override val input: InputStream = socket.getInputStream()

    override val output: OutputStream = socket.getOutputStream()

    override suspend fun disconnect() {
        withContext(Dispatchers.IO) {
            // closing the socket is what ends the adapter: `Listening::serve`
            // returns when its first client hangs up, and `bpd dap` exits with
            // it. the process is waited for rather than destroyed so that a
            // debuggee it is still tearing down is torn down
            runCatching { socket.close() }
                .onFailure { LOG.warn("the bpd adapter's socket would not close", it) }
            if (!process.waitFor(SHUTDOWN.inWholeMilliseconds, TimeUnit.MILLISECONDS)) {
                LOG.warn(
                    "`bpd dap --listen 0` did not exit after its client disconnected, so " +
                        "it is being ended — a debuggee it still holds is ended with it",
                )
                process.destroy()
            }
        }
    }

    companion object {
        private val LOG = logger<BpdConnection>()

        /**
         * how long the adapter has to say where it bound
         *
         * a limit rather than a measurement. it covers a cold start of the
         * binary and nothing else — the port is bound before the program is
         * launched, so nothing an interpreter does is inside it
         */
        private val ANNOUNCEMENT = 30.seconds

        /** how long the adapter has to exit after its client has gone */
        private val SHUTDOWN = 10.seconds

        /**
         * start the adapter, read where it bound, and connect with the token
         *
         * every failure below names what did not happen. a debug session that
         * refuses to start is the one moment a user cannot inspect anything, so
         * "the debug adapter exited unexpectedly" is the outcome this exists to
         * rule out
         */
        @Throws(ExecutionException::class)
        suspend fun open(command: GeneralCommandLine): BpdConnection {
            val process =
                withContext(Dispatchers.IO) {
                    command.createProcess()
                }
            val announced =
                try {
                    announcement(process, command)
                } catch (failed: Throwable) {
                    process.destroy()
                    throw failed
                }

            val socket =
                try {
                    withContext(Dispatchers.IO) {
                        Socket().apply {
                            connect(InetSocketAddress(announced.host, announced.port))
                            // the token before anything the launcher writes.
                            // flushed here because the launcher's first write is
                            // the `initialize` the token has to arrive with
                            getOutputStream().write(
                                "${announced.header}: ${announced.token}\r\n"
                                    .toByteArray(StandardCharsets.US_ASCII),
                            )
                            getOutputStream().flush()
                        }
                    }
                } catch (failed: Throwable) {
                    process.destroy()
                    throw ExecutionException(
                        "`${command.commandLineString}` said it is listening on " +
                            "${announced.host}:${announced.port}, and connecting to it failed: " +
                            "${failed.message}",
                        failed,
                    )
                }

            return BpdConnection(process, socket)
        }

        /** the one line `bpd dap --listen` prints before it accepts anything */
        @Throws(ExecutionException::class)
        private suspend fun announcement(
            process: Process,
            command: GeneralCommandLine,
        ): Announcement {
            val stderr = StringBuilder()
            val draining =
                Thread {
                    BufferedReader(InputStreamReader(process.errorStream, StandardCharsets.UTF_8))
                        .forEachLine { line ->
                            synchronized(stderr) { stderr.appendLine(line) }
                            LOG.info("bpd dap: $line")
                        }
                }
            draining.isDaemon = true
            draining.start()

            val line =
                try {
                    withTimeout(ANNOUNCEMENT) {
                        withContext(Dispatchers.IO) {
                            BufferedReader(
                                InputStreamReader(process.inputStream, StandardCharsets.UTF_8),
                            ).readLine()
                        }
                    }
                } catch (expired: TimeoutCancellationException) {
                    throw ExecutionException(
                        "`${command.commandLineString}` did not say where it is listening " +
                            "within $ANNOUNCEMENT. it prints one line of json on stdout before " +
                            "it accepts a connection, and nothing arrived" +
                            said(stderr),
                        expired,
                    )
                }

            if (line == null) {
                throw ExecutionException(
                    "`${command.commandLineString}` closed its stdout without saying where " +
                        "it is listening. it exited with ${process.waitFor()}" + said(stderr),
                )
            }

            val listening =
                try {
                    JsonParser.parseString(line).asJsonObject.getAsJsonObject("listening")
                } catch (malformed: RuntimeException) {
                    throw ExecutionException(
                        "`${command.commandLineString}` wrote `$line` on stdout, and that is " +
                            "not the `{\"listening\":{...}}` announcement a client reads the " +
                            "port and token out of",
                        malformed,
                    )
                }
                    ?: throw ExecutionException(
                        "`${command.commandLineString}` wrote `$line` on stdout, which is json " +
                            "with no `listening` object in it, so there is no port to connect to",
                    )

            return try {
                Announcement(
                    host = listening.get("host").asString,
                    port = listening.get("port").asInt,
                    header = listening.get("header").asString,
                    token = listening.get("token").asString,
                )
            } catch (incomplete: RuntimeException) {
                throw ExecutionException(
                    "`${command.commandLineString}` announced `$line`, which is missing one of " +
                        "`host`, `port`, `header` and `token` — and all four are needed to " +
                        "reach the adapter",
                    incomplete,
                )
            } catch (malformed: JsonSyntaxException) {
                throw ExecutionException(
                    "`${command.commandLineString}` announced `$line`, whose fields are not the " +
                        "types an announcement has",
                    malformed,
                )
            }
        }

        /** what the adapter wrote on stderr, when it wrote anything */
        private fun said(stderr: StringBuilder): String {
            val said = synchronized(stderr) { stderr.toString() }.trim()
            return if (said.isEmpty()) "" else ". it said on stderr:\n\n$said"
        }
    }

    /** where the adapter bound, and what a client has to present to be served */
    private data class Announcement(
        val host: String,
        val port: Int,
        val header: String,
        val token: String,
    )
}
