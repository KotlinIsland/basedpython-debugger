package com.github.kotlinisland.bpd

import com.intellij.execution.Executor
import com.intellij.execution.configuration.EmptyRunProfileState
import com.intellij.execution.configurations.ConfigurationFactory
import com.intellij.execution.configurations.LocatableConfigurationBase
import com.intellij.execution.configurations.LocatableRunConfigurationOptions
import com.intellij.execution.configurations.RunConfiguration
import com.intellij.execution.configurations.RunProfileState
import com.intellij.execution.configurations.RuntimeConfigurationError
import com.intellij.execution.configurations.SimpleConfigurationType
import com.intellij.execution.runners.ExecutionEnvironment
import com.intellij.execution.runners.RunConfigurationWithSuppressedDefaultRunAction
import com.intellij.icons.AllIcons
import com.intellij.openapi.components.BaseState
import com.intellij.openapi.options.SettingsEditor
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.TextFieldWithBrowseButton
import com.intellij.openapi.util.NotNullLazyValue
import com.intellij.ui.components.JBCheckBox
import com.intellij.ui.components.JBTextField
import com.intellij.ui.dsl.builder.AlignX
import com.intellij.ui.dsl.builder.panel
import com.intellij.util.execution.ParametersListUtil
import javax.swing.JComponent

/**
 * what a bpd run configuration remembers
 *
 * every field here is one the launch request carries — see
 * [BpdLaunchArgumentsProvider]. a field that were stored and not sent would be a
 * setting a person could fill in, see saved, and never get
 */
class BpdRunConfigurationOptions : LocatableRunConfigurationOptions() {
    /** the `bpd` to start, resolved by [BpdExecutable] */
    var executable: String? by string("bpd")

    /** the interpreter to run the program under, resolved on `PATH` like any other command */
    var interpreter: String? by string("python3")

    /** the script to run */
    var program: String? by string("")

    /** arguments for the program, exactly as it receives them */
    var parameters: String? by string("")

    /** stay stopped before the program's first statement */
    var stopOnEntry: Boolean by property(false)

    /** hold every thread that can be held for the duration of each stop */
    var stopTheWorld: Boolean by property(false)

    /** debug a child the program forks */
    var debugChildren: Boolean by property(false)
}

/**
 * a program to debug under `bpd`
 *
 * [getState] answers with [EmptyRunProfileState] because **nothing is executed
 * from here**. the platform's own `DapProgramRunner` takes the session over when
 * a [com.intellij.platform.dap.DapLaunchArgumentsProvider] says it applies, and
 * what starts the program is the `launch` request that runner sends — see
 * [BpdLaunchArgumentsProvider]
 *
 * it suppresses the default run action because bpd has no path that launches a
 * program without its agent. DAP calls that `noDebug` and the adapter refuses
 * it by name, so a Run button here would offer an affordance whose only outcome
 * is a refusal
 */
class BpdRunConfiguration(
    project: Project,
    factory: ConfigurationFactory,
    name: String,
) : LocatableConfigurationBase<BpdRunConfigurationOptions>(project, factory, name),
    RunConfigurationWithSuppressedDefaultRunAction {
    public override fun getOptions(): BpdRunConfigurationOptions =
        super.getOptions() as BpdRunConfigurationOptions

    var executable: String
        get() = options.executable.orEmpty()
        set(value) {
            options.executable = value
        }

    var interpreter: String
        get() = options.interpreter.orEmpty()
        set(value) {
            options.interpreter = value
        }

    var program: String
        get() = options.program.orEmpty()
        set(value) {
            options.program = value
        }

    var parameters: String
        get() = options.parameters.orEmpty()
        set(value) {
            options.parameters = value
        }

    var stopOnEntry: Boolean
        get() = options.stopOnEntry
        set(value) {
            options.stopOnEntry = value
        }

    var stopTheWorld: Boolean
        get() = options.stopTheWorld
        set(value) {
            options.stopTheWorld = value
        }

    var debugChildren: Boolean
        get() = options.debugChildren
        set(value) {
            options.debugChildren = value
        }

    /** the program's arguments, split the way a shell would split them */
    fun arguments(): List<String> = ParametersListUtil.parse(parameters)

    override fun getState(
        executor: Executor,
        environment: ExecutionEnvironment,
    ): RunProfileState = EmptyRunProfileState.INSTANCE

    override fun getConfigurationEditor(): SettingsEditor<out RunConfiguration> = BpdRunConfigurationEditor()

    /**
     * refuse a configuration that could not start, before anything is started
     *
     * the executable is resolved here as well as at launch, and deliberately:
     * this is the moment the answer is cheap and the person is looking at the
     * field that is wrong
     */
    override fun checkConfiguration() {
        if (program.isBlank()) {
            throw RuntimeConfigurationError(
                "there is no program to debug. name the python script this configuration runs",
            )
        }
        if (interpreter.isBlank()) {
            throw RuntimeConfigurationError(
                "there is no interpreter to run `$program` under. name one — it is resolved " +
                    "on PATH like any other command",
            )
        }
        try {
            BpdExecutable.resolve(executable)
        } catch (missing: com.intellij.execution.ExecutionException) {
            throw RuntimeConfigurationError(missing.message)
        }
    }

    /**
     * the file name of the program, when there is one to take
     *
     * `Path` rather than a split on `/`, because a windows configuration names
     * its program with backslashes and would otherwise be suggested whole
     */
    override fun suggestedName(): String? =
        try {
            java.nio.file.Path.of(program).fileName?.toString()
        } catch (ignored: java.nio.file.InvalidPathException) {
            // a name is a convenience, and a program that is not a path yet is
            // one somebody is still typing
            null
        }
}

/** the fields of a bpd run configuration, as a person edits them */
class BpdRunConfigurationEditor : SettingsEditor<BpdRunConfiguration>() {
    private val executable = TextFieldWithBrowseButton()
    private val interpreter = JBTextField()
    private val program = TextFieldWithBrowseButton()
    private val parameters = JBTextField()
    private val stopOnEntry = JBCheckBox()
    private val stopTheWorld = JBCheckBox()
    private val debugChildren = JBCheckBox()

    override fun resetEditorFrom(configuration: BpdRunConfiguration) {
        executable.text = configuration.executable
        interpreter.text = configuration.interpreter
        program.text = configuration.program
        parameters.text = configuration.parameters
        stopOnEntry.isSelected = configuration.stopOnEntry
        stopTheWorld.isSelected = configuration.stopTheWorld
        debugChildren.isSelected = configuration.debugChildren
    }

    override fun applyEditorTo(configuration: BpdRunConfiguration) {
        configuration.executable = executable.text
        configuration.interpreter = interpreter.text
        configuration.program = program.text
        configuration.parameters = parameters.text
        configuration.stopOnEntry = stopOnEntry.isSelected
        configuration.stopTheWorld = stopTheWorld.isSelected
        configuration.debugChildren = debugChildren.isSelected
    }

    override fun createEditor(): JComponent =
        panel {
            row("Script:") {
                cell(program).align(AlignX.FILL)
            }
            row("Arguments:") {
                cell(parameters).align(AlignX.FILL)
            }
            row("Interpreter:") {
                cell(interpreter).align(AlignX.FILL)
                    .comment("resolved on PATH like any other command")
            }
            row("${BpdExecutable.FIELD}:") {
                cell(executable).align(AlignX.FILL)
                    .comment(
                        "a bare name is looked up on PATH; anything else must be an absolute " +
                            "path. a relative path is refused rather than resolved against a " +
                            "directory nobody chose",
                    )
            }
            row {
                cell(stopOnEntry).comment(
                    "stay stopped before the program's first statement. bpd holds every " +
                        "program there, so this decides whether the IDE is told about it",
                )
                    .label("Stop on entry:")
            }
            row {
                cell(stopTheWorld).comment(
                    "hold every thread that can be held for the duration of each stop. off by " +
                        "default: a stop holds one thread and the rest of the program keeps " +
                        "running",
                )
                    .label("Stop the world:")
            }
            row {
                cell(debugChildren).comment(
                    "debug a child the program forks. it needs the IDE to support the " +
                        "startDebugging reverse request, and the adapter refuses it by name " +
                        "when the IDE does not",
                )
                    .label("Debug children:")
            }
        }
}

/**
 * the run configuration type bpd contributes
 *
 * its own type rather than a Mode beside jetbrains' debugpy backend: that switch
 * belongs to the python plugin and selects between **its** two backends. bpd is
 * a third one, registered on the platform's own extension point, so what it adds
 * is a configuration of its own — the same shape vs code's `"type": "bpd"` takes
 */
class BpdRunConfigurationType : SimpleConfigurationType(
    "BpdRunConfiguration",
    "bpd",
    "debug a python program with bpd",
    NotNullLazyValue.createValue { AllIcons.RunConfigurations.Application },
) {
    override fun createTemplateConfiguration(project: Project): RunConfiguration =
        BpdRunConfiguration(project, this, "bpd")

    override fun getOptionsClass(): Class<out BaseState> = BpdRunConfigurationOptions::class.java
}
