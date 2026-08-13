package com.github.kotlinisland.bpd

import com.intellij.execution.ExecutionException
import com.intellij.execution.ExecutionResult
import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.execution.runners.ExecutionEnvironment
import com.intellij.openapi.project.Project
import com.intellij.platform.dap.DapBreakpointsDescription
import com.intellij.platform.dap.DapLaunchArgumentsProvider
import com.intellij.platform.dap.DapStartRequest
import com.intellij.platform.dap.DebugAdapterDescriptor
import com.intellij.platform.dap.DebugAdapterId
import com.intellij.platform.dap.DebugAdapterSupportProvider
import com.intellij.platform.dap.LaunchRequestArguments
import com.intellij.platform.dap.connection.DebugAdapterHandle
import com.intellij.execution.configurations.RunProfile
import com.jetbrains.python.debugger.PyExceptionBreakpointType
import com.jetbrains.python.debugger.PyLineBreakpointType

/**
 * the adapter id bpd registers under
 *
 * its own id rather than an extension of jetbrains' `PythonDapAdapter`: that one
 * names debugpy, and its plugin's extension points (`debugpyConfigProvider`,
 * `breakpointHandler`) are debugpy shaped. `com.intellij.platform.dap` is the
 * layer underneath both, and this registers on the same extension point beside
 * it
 */
object BpdAdapterId : DebugAdapterId("bpd", "bpd")

/** what tells the platform that bpd is a debug adapter it can drive */
class BpdAdapterSupportProvider : DebugAdapterSupportProvider<BpdAdapterId> {
    override val adapterId: BpdAdapterId = BpdAdapterId

    override fun createDebugAdapterDescriptor(project: Project): DebugAdapterDescriptor<BpdAdapterId> =
        BpdDebugAdapterDescriptor()
}

/**
 * one bpd session, from the process that serves it to the breakpoints it binds
 *
 * this is the whole plugin: it says where `bpd` is, starts it listening, and
 * hands the platform a connection. everything a session does after that is DAP,
 * which `bpd dap` already speaks
 */
class BpdDebugAdapterDescriptor : DebugAdapterDescriptor<BpdAdapterId>() {
    override val id: BpdAdapterId = BpdAdapterId

    /**
     * the breakpoints a bpd session binds
     *
     * the python plugin's own types, and not types of our own, because they are
     * the breakpoints a person has already set: a second line breakpoint type
     * for the same `.py` file would put two of them in one gutter and make which
     * one a click produces a matter of priority. jetbrains' debugpy backend
     * names the same two
     */
    override val breakpointsDescription: DapBreakpointsDescription =
        DapBreakpointsDescription(
            PyLineBreakpointType::class.java,
            PyExceptionBreakpointType::class.java,
        )

    override suspend fun launchDebugAdapter(
        environment: ExecutionEnvironment,
        executionResult: ExecutionResult?,
        sessionId: String,
    ): DebugAdapterHandle {
        val configuration =
            environment.runProfile as? BpdRunConfiguration
                ?: throw ExecutionException(
                    "a bpd debug session was started for `${environment.runProfile.name}`, which " +
                        "is not a bpd run configuration. bpd's adapter is only reachable from " +
                        "one, because that is where the interpreter, the program and the path to " +
                        "`bpd` are",
                )

        val command =
            GeneralCommandLine(BpdExecutable.resolve(configuration.executable).toString())
                .withParameters("dap", "--listen", "0")
                // the debuggee inherits the adapter's working directory and a
                // program can see it. the project the configuration came from is
                // the directory a person means; whatever the IDE was started in
                // is not
                .withWorkingDirectory(environment.project.basePath?.let { java.nio.file.Path.of(it) })
                .withCharset(Charsets.UTF_8)

        return BpdConnection.open(command)
    }
}

/**
 * what a bpd run configuration sends in its `launch` request
 *
 * the platform's `DapProgramRunner` asks every provider whether it applies and
 * takes the session over when one does, so this is both the switch that makes a
 * bpd configuration debuggable and the place its fields become DAP
 *
 * every attribute here is one `bpd_dap::Configuration` reads, and the names are
 * its names — see `docs/development/dap.md`
 */
class BpdLaunchArgumentsProvider : DapLaunchArgumentsProvider {
    override fun isApplicable(
        executorId: String,
        profile: RunProfile,
    ): Boolean = executorId == DEBUG && profile is BpdRunConfiguration

    override fun getLaunchArguments(
        project: Project,
        profile: RunProfile,
    ): LaunchRequestArguments {
        val configuration =
            profile as? BpdRunConfiguration
                ?: throw IllegalStateException(
                    "the platform asked bpd for the launch arguments of " +
                        "`${profile.name}`, which `isApplicable` had already said is not a bpd " +
                        "run configuration",
                )
        return LaunchRequestArguments(
            BpdAdapterId,
            DapStartRequest.Launch,
            mapOf(
                "program" to configuration.program,
                "args" to configuration.arguments(),
                "python" to configuration.interpreter,
                "stopOnEntry" to configuration.stopOnEntry,
                "stopTheWorld" to configuration.stopTheWorld,
                "debugChildren" to configuration.debugChildren,
            ),
        )
    }

    private companion object {
        /**
         * the executor a bpd session runs under, and the only one
         *
         * `DefaultDebugExecutor.EXECUTOR_ID` by value rather than by reference
         * so that this file does not reach into the debugger's ui module for a
         * string. bpd has no undebugged run: it refuses DAP's `noDebug` because
         * there is no path that launches a program without its agent
         */
        const val DEBUG = "Debug"
    }
}
