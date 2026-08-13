package com.github.kotlinisland.bpd

import com.intellij.execution.RunManager
import com.intellij.execution.configurations.RuntimeConfigurationError
import com.intellij.execution.executors.DefaultDebugExecutor
import com.intellij.execution.runners.ExecutionEnvironmentBuilder
import com.intellij.execution.runners.ProgramRunner
import com.intellij.openapi.application.WriteAction
import com.intellij.openapi.util.io.FileUtil
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.vfs.VfsUtil
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.platform.dap.DapLaunchArgumentsProvider
import com.intellij.platform.dap.DebugAdapterSupportProvider
import com.intellij.testFramework.HeavyPlatformTestCase
import com.intellij.testFramework.PlatformTestUtil
import com.intellij.xdebugger.XDebugSession
import com.intellij.xdebugger.XDebuggerManager
import com.intellij.xdebugger.breakpoints.XBreakpointManager
import com.intellij.xdebugger.breakpoints.XBreakpointProperties
import com.intellij.xdebugger.breakpoints.XLineBreakpointType
import com.intellij.openapi.editor.colors.TextAttributesKey
import com.intellij.ui.SimpleTextAttributes
import com.intellij.xdebugger.frame.XCompositeNode
import com.intellij.xdebugger.frame.XFullValueEvaluator
import com.intellij.xdebugger.frame.XStackFrame
import com.intellij.xdebugger.frame.XValue
import com.intellij.xdebugger.frame.XValueChildrenList
import com.intellij.xdebugger.frame.XValueContainer
import com.intellij.xdebugger.frame.XValueGroup
import com.intellij.xdebugger.frame.XValueNode
import com.intellij.xdebugger.frame.XValuePlace
import com.intellij.xdebugger.frame.presentation.XValuePresentation
import com.intellij.xdebugger.frame.XDebuggerTreeNodeHyperlink
import com.jetbrains.python.debugger.PyLineBreakpointType
import java.io.File
import javax.swing.Icon
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit

/**
 * a debug session, driven inside a real IDE
 *
 * this is the half no rust test can reach: that the platform loads the plugin,
 * that its extension point registrations are ones the platform accepts, and that
 * a session started from a run configuration reaches a breakpoint
 *
 * **the evidence is taken from the IDE, not from bpd.** whether the adapter
 * answers a `stackTrace` correctly is settled by the rust suite, and re-reading
 * bpd's output here would prove that again instead of the thing in question. so
 * what is asserted is the IDE's own debug state: that *XDebuggerManager* holds a
 * session, that *the session* is paused, that *the frame the IDE focused* is in
 * the file the breakpoint is in and on its line
 *
 * it is a [HeavyPlatformTestCase] rather than a light one because a light test's
 * files live in an in-memory filesystem, and bpd binds breakpoints against files
 * on disk — a `temp://` path is one no interpreter could open
 */
class BpdSessionTest : HeavyPlatformTestCase() {
    /** the comment in `program.py` that says which line to stop on */
    private val marker = "# bpd: the session stops here"

    /**
     * how long any one step of the session is given
     *
     * generous, because it covers an interpreter starting and an agent being
     * staged on a cold cache. it is a limit, not a measurement: nothing here
     * reports how long anything took
     */
    private val limit = 120

    private lateinit var program: Path
    private lateinit var finished: Path

    override fun setUp() {
        super.setUp()
        val directory = createTempDirectory().toPath()
        program = directory.resolve("program.py")
        Files.write(program, source().toByteArray(StandardCharsets.UTF_8))
        finished = directory.resolve("finished")
    }

    /** `program.py`, from this suite's own resources rather than written inline */
    private fun source(): String =
        checkNotNull(javaClass.classLoader.getResourceAsStream("program.py")) {
            "`program.py` is not on the test classpath, so there is no program to debug"
        }.use { it.readBytes().toString(StandardCharsets.UTF_8) }

    /** the `bpd` this drives, checked before a session is asked for */
    private fun executable(): String {
        val named =
            checkNotNull(System.getenv("BPD_EXECUTABLE")) {
                "BPD_EXECUTABLE is not set, so there is no bpd binary to drive. build one " +
                    "with `cargo build --bin bpd` and set BPD_EXECUTABLE to its absolute path"
            }
        check(File(named).isFile) {
            "there is no bpd binary at `$named`, so there is no debug adapter for the IDE " +
                "to start. build one with `cargo build --bin bpd`"
        }
        return named
    }

    /**
     * the interpreter, checked by asking `bpd doctor` about it
     *
     * doctor is the same check a launch makes, so an interpreter that gets past
     * here is one a session can use — and when it does not, what is read is
     * doctor's own sentence rather than a second one written here
     */
    private fun interpreter(bpd: String): String {
        val named = System.getenv("BPD_PYTHON") ?: "python3"
        val asked =
            ProcessBuilder(bpd, "doctor", named)
                .redirectErrorStream(true)
                .start()
        val said = asked.inputStream.use { it.readBytes().toString(StandardCharsets.UTF_8) }
        check(asked.waitFor(limit.toLong(), TimeUnit.SECONDS)) {
            "`$bpd doctor $named` did not answer, so nothing here knows whether a session " +
                "could start under it"
        }
        check(asked.exitValue() == 0) {
            "`$named` is not an interpreter bpd can debug, so no session could start under " +
                "it. set BPD_PYTHON to one that is. bpd doctor says:\n\n$said"
        }
        return named
    }

    /** the line holding the marker, as the IDE numbers lines — from zero */
    private fun marked(file: VirtualFile): Int {
        val lines = VfsUtil.loadText(file).lines()
        val line = lines.indexOfFirst { it.contains(marker) }
        check(line >= 0) {
            "no line of `${file.path}` holds `$marker`, so there is nowhere to put the " +
                "breakpoint this session is about"
        }
        return line
    }

    fun testTheProviderIsRegistered() {
        // if the platform did not accept the manifest there is no bpd adapter
        // and nothing below could start, so this is the first thing to fail
        val providers = DebugAdapterSupportProvider.EP_NAME.extensionList
        assertTrue(
            "the platform loaded no bpd debug adapter support provider, so its manifest is " +
                "not one it accepts — it registered ${providers.map { it.adapterId.type }}",
            providers.any { it.adapterId.type == "bpd" },
        )
        val arguments = DapLaunchArgumentsProvider.EP_NAME.extensionList
        assertTrue(
            "the platform loaded no bpd launch arguments provider, and without one its own " +
                "DapProgramRunner never claims a bpd run configuration",
            arguments.any { it is BpdLaunchArgumentsProvider },
        )
    }

    fun testASessionStopsOnABreakpoint() {
        val bpd = executable()
        val python = interpreter(bpd)

        val file =
            checkNotNull(LocalFileSystem.getInstance().refreshAndFindFileByNioFile(program)) {
                "`$program` was written and the IDE cannot see it"
            }
        val line = marked(file)

        // set through the IDE's own breakpoint manager, on the python plugin's
        // line breakpoint type — which is the breakpoint a click in the gutter
        // produces. the IDE owns it from here and sends it to the adapter itself
        val breakpoints: XBreakpointManager =
            XDebuggerManager.getInstance(project).breakpointManager
        val type = XLineBreakpointType.EXTENSION_POINT_NAME.findExtension(PyLineBreakpointType::class.java)
        checkNotNull(type) {
            "this IDE has no python line breakpoint type, so there is no breakpoint to set — " +
                "the bpd plugin names it as the type a session binds"
        }
        @Suppress("UNCHECKED_CAST")
        WriteAction.runAndWait<RuntimeException> {
            breakpoints.addLineBreakpoint(
                type as XLineBreakpointType<XBreakpointProperties<*>>,
                file.url,
                line,
                type.createBreakpointProperties(file, line),
            )
        }

        val settings =
            RunManager.getInstance(project)
                .createConfiguration("bpd drives this", BpdRunConfigurationType::class.java)
        val configuration = settings.configuration as BpdRunConfiguration
        configuration.executable = bpd
        configuration.interpreter = python
        configuration.program = program.toString()
        configuration.parameters = finished.toString()

        val executor = DefaultDebugExecutor.getDebugExecutorInstance()
        val runner = ProgramRunner.getRunner(executor.id, configuration)
        assertNotNull(
            "no program runner claims a bpd run configuration under the debug executor. the " +
                "platform's own DapProgramRunner is the one that should, and it asks every " +
                "DapLaunchArgumentsProvider whether it applies",
            runner,
        )

        ExecutionEnvironmentBuilder.create(executor, settings).buildAndExecute()

        val debugger = XDebuggerManager.getInstance(project)
        waitFor("the IDE holding a debug session") { debugger.currentSession != null }
        val session: XDebugSession = checkNotNull(debugger.currentSession)
        assertEquals("bpd drives this", session.sessionName)

        waitFor("the program stopping at the breakpoint") { session.isPaused }

        val frame =
            checkNotNull(session.currentStackFrame) {
                "the session is paused and the IDE focused no stack frame, so there is nothing " +
                    "a person could read"
            }
        val position =
            checkNotNull(frame.sourcePosition) {
                "the frame the IDE focused has no source position, so the editor could not " +
                    "show where the program stopped"
            }
        assertEquals(
            "the session stopped in a file other than the one the breakpoint is in",
            file.path,
            position.file.path,
        )
        assertEquals(
            "the session stopped on a line other than the one holding `$marker`",
            line,
            position.line,
        )

        // the variables view, read the way the view itself reads it: the frame
        // the IDE focused is asked for its children, and the answer walks the
        // scope groups the DAP layer puts them in
        val total =
            checkNotNull(named(frame, "total")) {
                "`total` is a local of `accumulate` at this line, and the frame the IDE " +
                    "focused offers ${names(frame)}"
            }
        assertEquals("`total` is 0 + 1 + 2 at the breakpoint", "3", presentation(total))

        // the program has not run past the breakpoint, so it has not written its file
        assertFalse(
            "the program reached its last statement before the breakpoint stopped it",
            Files.exists(finished),
        )

        // the IDE's own resume, which is what a person presses
        session.resume()
        waitFor("the session ending") { session.isStopped }

        // the program's own word that it ran to the end. a session that was
        // killed rather than resumed leaves no file here
        waitFor("the program writing its last file") { Files.exists(finished) }
        assertEquals("42", FileUtil.loadFile(finished.toFile(), StandardCharsets.UTF_8).trim())
    }

    /**
     * a configuration whose `bpd executable` names nothing, which must not start
     *
     * what it is here for is the sentence. a `bpd` that cannot be found is the
     * likeliest thing to go wrong for a new user, and the whole reason
     * [BpdExecutable] walks `PATH` itself is so that what they read names the
     * command and says what to do — rather than the failed spawn a process that
     * does not exist otherwise produces
     *
     * the IDE's own validation is what is driven: `checkConfiguration` is what
     * the run configuration dialog calls, and what
     * `ExecutionEnvironmentBuilder.buildAndExecute` calls before it starts
     * anything
     */
    fun testAConfigurationThatCannotFindBpdIsRefused() {
        val missing = createTempDirectory().toPath().resolve("there-is-no-bpd-here")
        val settings =
            RunManager.getInstance(project)
                .createConfiguration("bpd cannot be found", BpdRunConfigurationType::class.java)
        val configuration = settings.configuration as BpdRunConfiguration
        configuration.executable = missing.toString()
        configuration.program = program.toString()

        val said =
            try {
                configuration.checkConfiguration()
                fail(
                    "the IDE accepted a configuration whose `${BpdExecutable.FIELD}` names " +
                        "nothing, so nothing told the person their path is wrong",
                )
                return
            } catch (refused: RuntimeConfigurationError) {
                refused.localizedMessage.orEmpty()
            }
        assertTrue(
            "what the IDE reported does not name the path that was wrong: $said",
            said.contains(missing.toString()),
        )
        assertTrue(
            "what the IDE reported does not say what to do about it: $said",
            said.contains("put `bpd` on PATH, or set the run configuration's"),
        )

        assertNull(
            "a session was started even though its adapter could not be named",
            XDebuggerManager.getInstance(project).currentSession,
        )
    }

    /**
     * a relative path is refused rather than resolved
     *
     * this is the rule `editors/vscode/extension.js` carries and the reason it
     * carries it: an IDE's working directory is not a directory the user chose,
     * so resolving against it would start whichever `bpd` happened to be there
     */
    fun testARelativeExecutableIsRefused() {
        val said =
            try {
                BpdExecutable.resolve("build/bpd")
                fail("a relative `${BpdExecutable.FIELD}` was resolved rather than refused")
                return
            } catch (refused: com.intellij.execution.ExecutionException) {
                refused.localizedMessage.orEmpty()
            }
        assertTrue(
            "the refusal does not say the path is relative: $said",
            said.contains("which is a relative path"),
        )
        assertTrue(
            "the refusal does not say what to give instead: $said",
            said.contains("give an absolute path, or a bare command name to look up on PATH"),
        )
    }

    /**
     * one level of a frame's children, as the variables view would ask for them
     *
     * [XCompositeNode] is a callback rather than a return value because the view
     * is asynchronous, so the answer is collected and the queue is pumped until
     * the node says it is finished — or says why it is not
     */
    private fun children(container: XValueContainer): List<Pair<String, XValue>> {
        val collected = mutableListOf<Pair<String, XValue>>()
        var refused: String? = null
        var done = false
        container.computeChildren(
            object : XCompositeNode {
                override fun addChildren(
                    children: XValueChildrenList,
                    last: Boolean,
                ) {
                    for (index in 0 until children.size()) {
                        collected += children.getName(index) to children.getValue(index)
                    }
                    for (index in 0 until children.topGroups.size) {
                        val group = children.topGroups[index]
                        collected += group.name.orEmpty() to GroupAsValue(group)
                    }
                    for (index in 0 until children.bottomGroups.size) {
                        val group = children.bottomGroups[index]
                        collected += group.name.orEmpty() to GroupAsValue(group)
                    }
                    if (last) {
                        done = true
                    }
                }

                // the platform deprecated this in favour of a form that takes
                // a callback for the rest, and left it abstract
                @Suppress("OVERRIDE_DEPRECATION")
                override fun tooManyChildren(remaining: Int) {
                    done = true
                }

                override fun setAlreadySorted(alreadySorted: Boolean) = Unit

                override fun setErrorMessage(errorMessage: String) {
                    refused = errorMessage
                    done = true
                }

                override fun setErrorMessage(
                    errorMessage: String,
                    link: XDebuggerTreeNodeHyperlink?,
                ) {
                    refused = errorMessage
                    done = true
                }

                override fun setMessage(
                    message: String,
                    icon: Icon?,
                    attributes: SimpleTextAttributes,
                    link: XDebuggerTreeNodeHyperlink?,
                ) = Unit
            },
        )
        waitFor("the frame answering with its children") { done }
        check(refused == null) {
            "the IDE could not read the frame's children: $refused"
        }
        return collected
    }

    /** an [XValueGroup] read as the container it is, so one walk covers both */
    private class GroupAsValue(private val group: XValueGroup) : XValue() {
        override fun computePresentation(
            node: XValueNode,
            place: XValuePlace,
        ) = Unit

        override fun computeChildren(node: XCompositeNode) = group.computeChildren(node)
    }

    /** every name the frame offers, across the scopes it groups them into */
    private fun names(frame: XStackFrame): List<String> =
        children(frame).flatMap { (name, value) ->
            val nested = children(value).map { it.first }
            if (nested.isEmpty()) listOf(name) else nested
        }

    /** the value the frame holds under a name, wherever the IDE grouped it */
    private fun named(
        frame: XStackFrame,
        wanted: String,
    ): XValue? {
        for ((name, value) in children(frame)) {
            if (name == wanted) {
                return value
            }
            children(value).firstOrNull { it.first == wanted }?.let { return it.second }
        }
        return null
    }

    /** what the variables view would show for a value */
    private fun presentation(value: XValue): String {
        var shown: String? = null
        value.computePresentation(
            object : XValueNode {
                override fun setPresentation(
                    icon: Icon?,
                    type: String?,
                    presentation: String,
                    hasChildren: Boolean,
                ) {
                    shown = presentation
                }

                override fun setPresentation(
                    icon: Icon?,
                    presentation: XValuePresentation,
                    hasChildren: Boolean,
                ) {
                    val text = StringBuilder()
                    presentation.renderValue(
                        object : XValuePresentation.XValueTextRenderer {
                            override fun renderValue(value: String) {
                                text.append(value)
                            }

                            override fun renderStringValue(value: String) {
                                text.append(value)
                            }

                            override fun renderNumericValue(value: String) {
                                text.append(value)
                            }

                            override fun renderKeywordValue(value: String) {
                                text.append(value)
                            }

                            override fun renderValue(
                                value: String,
                                key: TextAttributesKey,
                            ) {
                                text.append(value)
                            }

                            override fun renderStringValue(
                                value: String,
                                additionalSpecialCharsToHighlight: String?,
                                maxLength: Int,
                            ) {
                                text.append(value)
                            }

                            override fun renderComment(comment: String) = Unit

                            override fun renderSpecialSymbol(symbol: String) {
                                text.append(symbol)
                            }

                            override fun renderError(error: String) {
                                text.append(error)
                            }
                        },
                    )
                    shown = text.toString()
                }

                override fun setFullValueEvaluator(evaluator: XFullValueEvaluator) = Unit
            },
            XValuePlace.TREE,
        )
        waitFor("the IDE rendering the value") { shown != null }
        return checkNotNull(shown)
    }

    /**
     * wait for something to become true, pumping the IDE's event queue
     *
     * the message names the thing that never happened, because "timed out" on
     * its own says nothing about which step of a session stalled
     */
    private fun waitFor(
        what: String,
        condition: () -> Boolean,
    ) {
        PlatformTestUtil.waitWithEventsDispatching(
            "$what did not happen",
            condition,
            limit,
        )
    }
}
