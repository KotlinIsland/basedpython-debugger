import org.jetbrains.intellij.platform.gradle.TestFrameworkType

plugins {
    id("org.jetbrains.kotlin.jvm") version "2.4.10"
    id("org.jetbrains.intellij.platform") version "2.18.1"
}

group = "com.github.kotlinisland"
version = "0.0.0"

repositories {
    mavenCentral()
    intellijPlatform {
        defaultRepositories()
    }
}

dependencies {
    intellijPlatform {
        // the unified PyCharm, and not `pycharmCommunity`, because
        // `intellij.platform.dap` is **not in the community build** — see
        // `docs/development/intellij.md`. `useInstaller = false` takes the maven
        // artifact from the intellij repository rather than a `.dmg` or `.exe`,
        // which is the only form a CI runner can unpack
        pycharm("2026.2.1") {
            useInstaller = false
        }
        // `PyLineBreakpointType` and `PyExceptionBreakpointType` are the
        // breakpoints a person already sets in a python file, and naming them is
        // how a bpd session binds to those rather than adding a second gutter
        // breakpoint beside them
        bundledPlugin("PythonCore")
        // the DAP layer is a **content module**, not a plugin, so it is not on
        // the compile classpath by default and naming it in `plugin.xml` alone
        // gets `Unresolved reference 'dap'`
        bundledModule("intellij.platform.dap")
        testFramework(TestFrameworkType.Platform)
    }
    // `HeavyPlatformTestCase` is a `junit.framework.TestCase`, and the platform
    // test framework does not bring junit with it — without this the suite fails
    // to compile with "cannot access 'junit.framework.TestCase'"
    testImplementation("junit:junit:4.13.2")
}

kotlin {
    // the platform's own classes are compiled for java 25 — `product-info.json`
    // says `minRequiredJavaVersion: 25` — so anything older cannot read them
    jvmToolchain(25)
    compilerOptions {
        // the same standard `cargo clippy -- -D warnings` holds the rust to. a
        // deprecation here is an api that is going away under a plugin built on
        // one that is already `@ApiStatus.Experimental`
        allWarningsAsErrors = true
    }
}

intellijPlatform {
    pluginConfiguration {
        ideaVersion {
            // 262 is the first build carrying `intellij.platform.dap`. there is
            // no upper bound because the api is `@ApiStatus.Experimental`: a
            // build that breaks it should fail the verifier, not be excluded in
            // advance
            sinceBuild = "262"
            untilBuild = provider { null }
        }
    }
}

tasks.test {
    // the same two things `editors/vscode/test/run.js` needs, and for the same
    // reason: a session needs a `bpd` to start and an interpreter to run under,
    // and the agent is not abi3 so the interpreter has to be the one the agent
    // was built against
    for (name in listOf("BPD_EXECUTABLE", "BPD_PYTHON")) {
        System.getenv(name)?.let { environment(name, it) }
    }
    // the platform test framework wants a headless jvm and its own config and
    // system directories, so a run cannot read or write the settings of an IDE
    // somebody uses
    systemProperty("java.awt.headless", "true")
    systemProperty("idea.force.use.core.classloader", "true")
    testLogging {
        events("passed", "failed", "skipped")
        showStandardStreams = true
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}
