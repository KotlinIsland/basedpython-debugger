//! a breakpoint in a django template, against a real django rendering it
//!
//! every number in this file was **measured** rather than designed. django
//! compiles a template to a tree of `Node` objects and renders it by calling
//! `Node.render_annotated` once per node, and which nodes that reaches is not
//! obvious from reading the template — an `{% extends %}` makes django skip
//! nearly the whole of the extending template's own tree. so the version is
//! pinned in [`bpd_test::django::VERSION`] and asserted inside the debuggee.
//! django's template internals are not a stable API, and a suite that silently
//! began measuring a different django would be worse than one that fails
//!
//! the stop discipline is the suite's: the program writes a marker before the
//! render and another after it, so a stop is proved by the second marker
//! **not** being there while the engine is told the program is stopped

// **unix.** the reloader fixture arms `signal.alarm`, which does not exist on
// windows. the sweep that gated the `os.fork()` files twice over missed this,
// because it looked for one construct rather than for the rule: a test fixture
// may use anything the platform has, and this platform is not every platform
#![cfg(unix)]

use std::path::{Path, PathBuf};

use bpd_core::python::Capabilities;
use bpd_core::{
    Binding, Content, Detail, Evaluated, Frame, FrameId, FrameKind, Resolved, Running, Scope,
    SourceBreakpoint, StopReason, TemplateContext, Unbound, Value,
};
use bpd_engine::{Debuggee, Launched};
use bpd_test::debuggee::{Fixture, line_of};

/// the template every other one in this fixture is reached through
///
/// it holds a `{% block %}` the child overrides and a variable outside every
/// block, so a render of the child reaches two of *this* file's lines
const BASE: &str = r"<html>
{% block content %}base default{% endblock %}
{{ footer }}
</html>
";

/// a template that extends, which is the shape nearly every django page has
///
/// `{{ stray }}` and `{{ trailing }}` sit under the `{% extends %}` and outside
/// every block, and django renders **neither** — the values are in the context
/// and never reach the output. the `{% block content %}` tag itself is not
/// rendered either: django renders the *parent's* block node, which reaches
/// back for this block's body and renders that directly. all three are lines a
/// breakpoint must not be reported bound to
const INDEX: &str = r#"{% extends "base.html" %}
{{ stray }}
{% block content %}
  {% with greeting=salute %}
  {{ greeting }}
  {% include "part.html" %}
  {% endwith %}
{% endblock %}
{{ trailing }}
"#;

/// a template reached only through `{% include %}`, and only while rendering
///
/// django parses it the first time the include runs, which is what makes it a
/// test of "a template breakpoint binds the moment django loads the template"
const PART: &str = r"part
{{ who }}
";

/// the program, without the django preamble that goes above it
///
/// `greeting` is in the render context **and** pushed again by the
/// `{% with %}`, which is what makes the layers of a context observable: a
/// debugger that merged them cannot say which one the template reads
const PROGRAM: &str = r#"

def main():
    template = get_template("index.html")
    MARKS.write_text("before")
    rendered = template.render(
        {
            "salute": "hello",
            "greeting": "outer",
            "who": "you",
            "footer": "footer",
            "stray": "stray",
            "trailing": "trailing",
            "shout": "quiet",
        }
    )
    MARKS.write_text("after")
    return rendered


main()
"#;

/// the interpreter the built agent matches, or a failure saying how to get one
fn interpreter() -> &'static Capabilities {
    bpd_test::agent::matching_interpreter()
}

/// a fixture with the three templates beside a program that renders them
///
/// `debug` is the template engine's `'OPTIONS': {'debug': ...}` — the setting
/// `DEBUG = True` turns on by default, and the one the design for this feature
/// expected to decide whether template debugging is possible at all
fn app(debug: bool) -> Fixture {
    let fixture = Fixture::new(
        "app",
        &format!("{}{PROGRAM}", bpd_test::django::preamble(debug)),
    );
    fixture.beside("templates/base.html", BASE);
    fixture.beside("templates/index.html", INDEX);
    fixture.beside("templates/part.html", PART);
    fixture
}

/// where a template of the fixture landed
fn template(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.directory().join("templates").join(name)
}

/// what the program has written by the time it is looked at
///
/// `before` means the render is under way and has not finished, which is what
/// proves a stop rather than the engine's word for one
fn marks(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.directory().join("marks")).unwrap_or_default()
}

/// launch a fixture and require that it stopped before running anything
fn launch(fixture: &Fixture) -> Debuggee {
    match bpd_engine::launch(
        interpreter(),
        &bpd_engine::Program::Script(fixture.path()),
        &[],
    ) {
        Ok(Launched::Stopped(debuggee)) => debuggee,
        Ok(Launched::ExitedBeforeStopping(status)) => {
            panic!("the debuggee exited with {status} instead of stopping")
        }
        Err(error) => panic!("the debuggee did not launch: {error}"),
    }
}

/// a template breakpoint is unbound until django parses the template
///
/// the program has run nothing at the entry stop, so django has not been
/// imported and no template has been loaded. every template breakpoint set here
/// is `NotLoaded`, and it is the rebinding announced later that says where it
/// went
fn set(debuggee: &mut Debuggee, breakpoints: Vec<SourceBreakpoint>) {
    for resolution in debuggee
        .set_breakpoints(breakpoints)
        .expect("the breakpoint request was answered")
    {
        match &resolution.binding {
            Binding::Unbound {
                reason: Unbound::NotLoaded { .. },
            } => {}
            other => panic!(
                "breakpoint {} was answered {other:?}, and nothing is loaded yet",
                resolution.id
            ),
        }
    }
}

/// the last thing the agent said about each breakpoint, by id
///
/// every parse of a template re-resolves the whole set, so a breakpoint is
/// spoken about more than once and only the newest answer is its answer
fn latest(rebound: Vec<Resolved>) -> Vec<(u32, Binding)> {
    let mut newest: std::collections::BTreeMap<u32, Binding> = std::collections::BTreeMap::new();
    for resolution in rebound {
        newest.insert(resolution.id, resolution.binding);
    }
    newest.into_iter().collect()
}

/// where a breakpoint bound in a template, or a failure naming what it did
fn in_template(binding: &Binding) -> (u32, &[String]) {
    match binding {
        Binding::BoundInTemplate { line, nodes, .. } => (*line, nodes),
        other => panic!("expected a template binding, got {other:?}"),
    }
}

/// run to the next stop, and hand back the reason with what was rebound on the way
fn run_to_stop(debuggee: &mut Debuggee) -> (StopReason, Vec<(u32, Binding)>) {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Stopped { stop, rebound } => (stop.reason, latest(rebound)),
        Running::Exited {
            status, rebound, ..
        } => panic!(
            "it exited with {status} instead of stopping. what it said about \
             the breakpoints was {:?}",
            latest(rebound)
        ),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }
}

/// run to the end, and hand back what was rebound on the way
fn run_to_exit(debuggee: &mut Debuggee) -> Vec<(u32, Binding)> {
    match debuggee
        .run(&mut bpd_test::reporting::Unreported)
        .expect("the debuggee was resumed")
    {
        Running::Exited {
            status, rebound, ..
        } => {
            assert!(status.success(), "the debuggee exited with {status}");
            latest(rebound)
        }
        Running::Stopped { stop, .. } => panic!("it stopped for {stop:?} and was not supposed to"),
        Running::StillRunning { waited, .. } => unreachable!(
            "this wait carries no deadline and was answered after {waited:?} \
             with the program still running"
        ),
        // bpd launched this program and holds its child, so it is bpd that
        // reads the exit
        Running::Ended { .. } => unreachable!(
            "the program bpd launched ended without an exit status, and bpd \
             holds its child"
        ),
        Running::Finishing { threads, .. } => {
            panic!("nothing was held, and the debuggee ended holding {threads:?}")
        }
    }
}

/// clear the breakpoints and let the program finish
fn to_exit(debuggee: &mut Debuggee) {
    debuggee
        .set_breakpoints(Vec::new())
        .expect("the breakpoint set was cleared");
    run_to_exit(debuggee);
}

/// require that the stop is the breakpoint asked for, at the file and line
fn stopped_at(reason: &StopReason, id: u32, file: &Path, at: u32) {
    match reason {
        StopReason::Breakpoint {
            breakpoints,
            file: stopped_file,
            line,
        } => {
            assert_eq!(breakpoints, &[id]);
            assert_eq!(Path::new(stopped_file), file);
            assert_eq!(*line, at);
        }
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

/// the template frames of a stack, as `(file, line, node class)`
///
/// the python frames are dropped because there are twenty of django's under
/// every template line and none of them is what the test is about — but that
/// they are still *there* is asserted separately, because a stack that hid them
/// would be a different lie
fn template_frames(frames: &[Frame]) -> Vec<(String, u32, String)> {
    frames
        .iter()
        .filter_map(|frame| match &frame.kind {
            FrameKind::Template { node, .. } => Some((
                Path::new(&frame.file)
                    .file_name()
                    .expect("a template's origin is a file")
                    .to_string_lossy()
                    .into_owned(),
                frame.line,
                node.clone(),
            )),
            FrameKind::Python { .. } => None,
        })
        .collect()
}

/// the text of a string value, or a failure naming what it was instead
fn text_of(value: &Value) -> &str {
    match &value.content {
        Content::Str { text, .. } => text,
        other => panic!("expected a string, got {other:?}"),
    }
}

/// what one layer of a context holds, sorted
fn names(context: &TemplateContext, layer: usize) -> Vec<&str> {
    let mut held: Vec<&str> = context.layers[layer]
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    held.sort_unstable();
    held
}

#[test]
fn a_breakpoint_in_a_template_stops_while_it_renders_on_the_line_it_bound_to() {
    let fixture = app(true);
    let index = template(&fixture, "index.html");
    let at = line_of(INDEX, "{{ greeting }}");
    let mut debuggee = launch(&fixture);

    set(&mut debuggee, vec![SourceBreakpoint::at(1, &index, at)]);

    let (reason, rebound) = run_to_stop(&mut debuggee);
    assert_eq!(
        rebound
            .iter()
            .map(|(id, binding)| (*id, in_template(binding)))
            .collect::<Vec<_>>(),
        [(1, (at, ["VariableNode".to_string()].as_slice()))],
        "django compiles `{{{{ greeting }}}}` to one `VariableNode` on the line \
         it was asked for"
    );
    stopped_at(&reason, 1, &index, at);

    // the render has not finished, which is what proves the program is really
    // held rather than the engine merely saying so
    assert_eq!(marks(&fixture), "before");

    // the template frame is the top of the stack, and the python that renders
    // it is still underneath — a stack that dropped django's frames would be
    // hiding the program from the person debugging it
    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert!(matches!(stack.frames[0].kind, FrameKind::Template { .. }));
    assert_eq!(stack.frames[0].file, index.to_string_lossy());
    assert_eq!(stack.frames[0].line, at);
    assert_eq!(stack.frames[0].name(), "VariableNode");
    assert_eq!(
        stack.frames[1].name(),
        "Node.render_annotated",
        "the frame under a template frame is the django method that renders it"
    );

    // a template frame carries the id of the python frame under it, and that is
    // where python is asked about
    let FrameKind::Template { python, .. } = stack.frames[0].kind else {
        unreachable!("the assertion above pinned it to a template frame");
    };
    assert_eq!(python, stack.frames[1].id);

    to_exit(&mut debuggee);
    assert_eq!(marks(&fixture), "after");
}

#[test]
fn a_template_frame_has_a_layered_context_rather_than_python_scopes() {
    let fixture = app(true);
    let index = template(&fixture, "index.html");
    let at = line_of(INDEX, "{{ greeting }}");
    let mut debuggee = launch(&fixture);

    set(&mut debuggee, vec![SourceBreakpoint::at(1, &index, at)]);
    let (reason, _) = run_to_stop(&mut debuggee);
    stopped_at(&reason, 1, &index, at);

    let top = FrameId { stop: 2, depth: 0 };
    let context = debuggee
        .template_context(top, Detail::default())
        .expect("the template context was answered");

    // measured against django 6.1: the builtins django pushes, the dictionary
    // the render was given, the layer `{% block %}` pushes for `block`, and the
    // one `{% with %}` pushed
    assert_eq!(
        context
            .layers
            .iter()
            .map(|layer| layer.index)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(names(&context, 0), ["False", "None", "True"]);
    assert_eq!(
        names(&context, 1),
        [
            "footer", "greeting", "salute", "shout", "stray", "trailing", "who"
        ]
    );
    assert_eq!(names(&context, 3), ["greeting"]);

    // the whole point of not flattening: two layers hold `greeting`, they hold
    // different text, and the one django reads is the last
    assert_eq!(
        context
            .shadowed()
            .into_iter()
            .map(|shadowed| (shadowed.name, shadowed.layers))
            .collect::<Vec<_>>(),
        [("greeting".to_string(), vec![1, 3])]
    );
    assert_eq!(
        text_of(context.layers[1].get("greeting").expect("layer 1 holds it")),
        "outer"
    );
    let (layer, value) = context.resolve("greeting").expect("the context holds it");
    assert_eq!(layer.index, 3);
    assert_eq!(text_of(value), "hello");

    // and a python scope is refused rather than answered from the frame
    // underneath, which holds `self` and `context` and not one template name
    let refused = debuggee
        .variables(top, Scope::Local, Detail::default())
        .expect_err("a template frame has no python scopes");
    let message = refused.to_string();
    for expected in ["django template frame", "the template context", "frame 1"] {
        assert!(
            message.contains(expected),
            "the refusal has to say {expected:?}, and it said {message}"
        );
    }

    to_exit(&mut debuggee);
}

#[test]
fn an_include_and_an_extends_are_the_nested_chain_of_frames_they_really_are() {
    let fixture = app(true);
    let part = template(&fixture, "part.html");
    let at = line_of(PART, "{{ who }}");
    let mut debuggee = launch(&fixture);

    // nothing has parsed `part.html` when this is set, and nothing will until
    // the `{% include %}` runs. that it binds at all is the claim under test
    set(&mut debuggee, vec![SourceBreakpoint::at(1, &part, at)]);
    let (reason, rebound) = run_to_stop(&mut debuggee);
    assert_eq!(
        rebound
            .iter()
            .map(|(id, binding)| (*id, in_template(binding)))
            .collect::<Vec<_>>(),
        [(1, (at, ["VariableNode".to_string()].as_slice()))]
    );
    stopped_at(&reason, 1, &part, at);

    let stack = debuggee.the_stack(None).expect("the stack was answered");
    assert_eq!(
        template_frames(&stack.frames),
        [
            ("part.html".to_string(), 2, "VariableNode".to_string()),
            ("index.html".to_string(), 6, "IncludeNode".to_string()),
            ("index.html".to_string(), 4, "WithNode".to_string()),
            ("base.html".to_string(), 2, "BlockNode".to_string()),
            ("index.html".to_string(), 1, "ExtendsNode".to_string()),
        ],
        "the chain is the one django really has: `{{% extends %}}` renders the \
         parent, and the parent's `{{% block %}}` renders the child's body"
    );

    // there is python between every pair of them, and it is still in the stack
    for pair in stack.frames.windows(2) {
        if matches!(pair[0].kind, FrameKind::Template { .. }) {
            assert!(
                matches!(pair[1].kind, FrameKind::Python { .. }),
                "a template frame sits over the python frame that renders it"
            );
        }
    }

    assert_eq!(marks(&fixture), "before");
    to_exit(&mut debuggee);
}

#[test]
fn nothing_an_extends_stops_django_rendering_is_reported_bound() {
    let fixture = app(true);
    let index = template(&fixture, "index.html");
    let mut debuggee = launch(&fixture);

    let stray = line_of(INDEX, "{{ stray }}");
    let block = line_of(INDEX, "{% block content %}");
    let with = line_of(INDEX, "{% with greeting=salute %}");
    let trailing = line_of(INDEX, "{{ trailing }}");
    let include = line_of(INDEX, r#"{% include "part.html" %}"#);

    set(
        &mut debuggee,
        vec![
            SourceBreakpoint::at(1, &index, stray),
            SourceBreakpoint::at(2, &index, block),
            SourceBreakpoint::at(3, &index, trailing),
        ],
    );

    // the two that could move did, onto the same line, and the stop names both
    // of them. that they stopped **at all** is the other half of the claim: a
    // line reported bound has to be a line django really renders
    let (reason, rebound) = run_to_stop(&mut debuggee);
    match &reason {
        StopReason::Breakpoint {
            breakpoints,
            file,
            line,
        } => {
            assert_eq!(breakpoints, &[1, 2]);
            assert_eq!(Path::new(file), index);
            assert_eq!(*line, with);
        }
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
    assert_eq!(
        rebound.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [1, 2, 3]
    );

    // `{{ stray }}` is under the `{% extends %}` and outside every block, and
    // the `{% block content %}` tag is never rendered either — django renders
    // the *parent's* block node and hands it this block's body. so both move
    // down to the first line of the body that django does render
    assert_eq!(
        in_template(&rebound[0].1),
        (with, ["WithNode".to_string()].as_slice())
    );
    assert_eq!(
        in_template(&rebound[1].1),
        (with, ["WithNode".to_string()].as_slice())
    );

    // and `{{ trailing }}` is past the last line django renders, so there is
    // nowhere for it to move to and it is unbound, naming that line
    match &rebound[2].1 {
        Binding::Unbound {
            reason:
                reason @ Unbound::NoRenderedNode {
                    requested,
                    last_rendered,
                    ..
                },
        } => {
            assert_eq!(*requested, trailing);
            assert_eq!(*last_rendered, Some(include));
            let message = reason.to_string();
            assert!(
                message.contains("no node of it renders at or after line"),
                "the reason has to say what is wrong, and it said {message}"
            );
        }
        other => panic!("`{{{{ trailing }}}}` is never rendered, and was answered {other:?}"),
    }

    assert_eq!(marks(&fixture), "before");
    to_exit(&mut debuggee);
    assert_eq!(marks(&fixture), "after");
}

#[test]
fn the_template_engines_debug_option_changes_nothing_bpd_reads() {
    // the design for this feature said `node.token` and `node.origin` are only
    // populated in debug mode, and that bpd would have to refuse template
    // debugging by name when it is off. measured against django 6.1 that is
    // not true: `Parser.extend_nodelist` sets both unconditionally, and debug
    // mode only chooses `DebugLexer` over `Lexer`, whose sole difference is
    // that it records each token's character `position`. nothing bpd reads is
    // `position`, so there is nothing to refuse — and a refusal would be bpd
    // declining to do something it can do correctly
    let mut lines = Vec::new();
    for debug in [true, false] {
        let fixture = app(debug);
        let index = template(&fixture, "index.html");
        let at = line_of(INDEX, "{{ greeting }}");
        let mut debuggee = launch(&fixture);

        set(&mut debuggee, vec![SourceBreakpoint::at(1, &index, at)]);
        let (reason, rebound) = run_to_stop(&mut debuggee);
        assert_eq!(
            rebound
                .iter()
                .map(|(id, binding)| (*id, in_template(binding)))
                .collect::<Vec<_>>(),
            [(1, (at, ["VariableNode".to_string()].as_slice()))],
            "the engine's `debug` option was {debug} and it changed the binding"
        );
        stopped_at(&reason, 1, &index, at);
        assert_eq!(marks(&fixture), "before");

        let stack = debuggee.the_stack(None).expect("the stack was answered");
        lines.push(template_frames(&stack.frames));

        to_exit(&mut debuggee);
        assert_eq!(marks(&fixture), "after");
    }

    assert_eq!(
        lines[0], lines[1],
        "the same template stopped in the same places with the engine's \
         `debug` option on and off"
    );
}

#[test]
fn an_expression_in_a_template_frame_is_template_syntax_and_not_python() {
    let fixture = app(true);
    let index = template(&fixture, "index.html");
    let at = line_of(INDEX, "{{ greeting }}");
    let mut debuggee = launch(&fixture);

    set(&mut debuggee, vec![SourceBreakpoint::at(1, &index, at)]);
    let (reason, _) = run_to_stop(&mut debuggee);
    stopped_at(&reason, 1, &index, at);

    let top = FrameId { stop: 2, depth: 0 };
    let evaluated = |debuggee: &mut Debuggee, expression: &str| match debuggee
        .evaluate(top, expression, Detail::default())
        .expect("the evaluation was answered")
    {
        Evaluated::Value { value } => value,
        Evaluated::Raised { error } => panic!("`{expression}` raised {error}"),
    };

    // django's own resolution, which is why `{% with %}` wins and a filter
    // applies. a python `eval` of either text is a `NameError` and a
    // `SyntaxError`
    assert_eq!(text_of(&evaluated(&mut debuggee, "greeting")), "hello");
    assert_eq!(text_of(&evaluated(&mut debuggee, "shout|upper")), "QUIET");

    // a name the context does not hold is django's own `VariableDoesNotExist`
    // rather than the engine's `string_if_invalid`, which is `''` and would be
    // a debugger reporting an empty string for a variable that is not there
    match debuggee
        .evaluate(top, "absent", Detail::default())
        .expect("the evaluation was answered")
    {
        Evaluated::Raised { error } => {
            assert_eq!(error.kind, "django.template.base.VariableDoesNotExist");
            assert!(
                error.message.contains("Failed lookup for key [absent]"),
                "django's own message says what it looked for, and it said {}",
                error.message
            );
        }
        Evaluated::Value { value } => {
            panic!("`absent` is in no layer of the context, and was answered {value:?}")
        }
    }

    to_exit(&mut debuggee);
}

// ---- the reloader --------------------------------------------------------

/// the program `django.utils.autoreload.restart_with_reloader` produces, in the
/// shape it produces it — read in django 6.1
///
/// ```py
/// def restart_with_reloader():
///     new_environ = {**os.environ, DJANGO_AUTORELOAD_ENV: "true"}
///     args = get_child_arguments()
///     while True:
///         p = subprocess.run(args, env=new_environ, close_fds=False)
///         if p.returncode != 3:
///             return p.returncode
/// ```
///
/// so under the default `runserver` the parent starts `sys.executable` on the
/// same command line with a marker in the environment, and then does **nothing
/// but wait on the exit code**. every request is served by the child, and every
/// breakpoint anyone sets is in code the child runs
///
/// the two details that are django's rather than this test's are both here and
/// both matter: `env=` is a **copy of `os.environ`**, which is what the channel
/// has to survive, and `close_fds=False`, which hands the child every descriptor
/// the parent holds — including this session's
const RELOADER: &str = r#"
import os
import subprocess
import sys

if not os.environ.get("RUN_MAIN"):
    new_environ = {**os.environ, "RUN_MAIN": "true"}
    finished = subprocess.run([sys.executable, __file__], env=new_environ, close_fds=False)
    raise SystemExit(finished.returncode)

import signal

signal.alarm(300)


def serve():
    MARKS.write_text("before")
    rendered = get_template("index.html").render(
        {
            "salute": "hello",
            "greeting": "outer",
            "who": "you",
            "footer": "footer",
            "stray": "stray",
            "trailing": "trailing",
        }
    )
    MARKS.write_text("after")
    return rendered


serve()
"#;

/// how long a wait on a program that is busy with its child is given
const A_MOMENT: std::time::Duration = std::time::Duration::from_secs(2);

/// how long a wait that has to finish is given
const LONG_ENOUGH: std::time::Duration = std::time::Duration::from_mins(1);

/// ask one session for something, collecting whatever the program says on the
/// way
///
/// the sink is a [`bpd_test::reporting::Children`] rather than `Unreported`,
/// because a reloader **does** start a child and says so — that report is what
/// the single-process tests above are entitled to refuse and this one is about
fn ask(
    debuggee: &mut Debuggee,
    at: bpd_core::SessionId,
    request: bpd_core::Request,
    seen: &mut bpd_test::reporting::Children,
) -> bpd_core::Response {
    match debuggee.dispatch(bpd_core::Addressed::to(at, request), seen) {
        Ok(answer) => answer,
        Err(error) => panic!("{at} was not answered: {error}"),
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "it is one reloader's whole life — the setting, the child the \
              reloader execs, the session it opens, a template breakpoint bound \
              in the child's own django and hit mid-render, and both processes \
              ending. splitting it would start django several times and assert \
              on a different half of one sequence each time"
)]
fn a_breakpoint_in_a_template_the_reloaders_child_renders_is_hit_in_the_child() {
    // the whole reason this feature exists. `bpd launch manage.py runserver`
    // attaches to a supervisor that never imports the template engine, so
    // before this a template breakpoint here was reported **unbound** — true,
    // and useless
    let fixture = Fixture::new(
        "manage",
        &format!("{}{RELOADER}", bpd_test::django::preamble(false)),
    );
    fixture.beside("templates/base.html", BASE);
    fixture.beside("templates/index.html", INDEX);
    fixture.beside("templates/part.html", PART);

    let mut seen = bpd_test::reporting::Children::default();
    let mut debuggee = launch(&fixture);
    let parent = match debuggee.sessions().as_slice() {
        [only] => *only,
        open => panic!("one program was launched and the debuggee holds {open:?}"),
    };
    assert!(
        debuggee
            .debug_children(true)
            .expect("the debuggee took the setting"),
        "the agent has to say the setting took"
    );

    match ask(
        &mut debuggee,
        parent,
        bpd_core::Request::Run {
            deadline: Some(A_MOMENT),
        },
        &mut seen,
    ) {
        bpd_core::Response::Ran(Running::StillRunning { .. }) => {}
        other => panic!("the reloader waits on its child: {other:?}"),
    }

    // the child is the fresh interpreter `subprocess.run` started, entered
    // through the staged `sitecustomize` on the end of the `PYTHONPATH` django
    // copied out of `os.environ`
    let child = {
        let mut found = None;
        for _ in 0..60 {
            let open = debuggee.sessions();
            if let Some(joined) = open.iter().find(|id| **id != parent) {
                found = Some(*joined);
                break;
            }
            match ask(
                &mut debuggee,
                parent,
                bpd_core::Request::Wait {
                    deadline: Some(A_MOMENT),
                },
                &mut seen,
            ) {
                bpd_core::Response::Ran(Running::StillRunning { .. }) => {}
                other => panic!("the reloader had nothing to say and answered {other:?}"),
            }
        }
        found.expect("the reloader's child opened a session of its own")
    };

    match ask(
        &mut debuggee,
        child,
        bpd_core::Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        bpd_core::Response::Ran(Running::Stopped { stop, .. }) => assert!(
            matches!(stop.reason, StopReason::Started { .. }),
            "the child arrives held before its program: {:?}",
            stop.reason
        ),
        other => panic!("the child was supposed to arrive held: {other:?}"),
    }

    // `{{ greeting }}` inside the `{% with %}`, which is a line django really
    // renders — the same line the single-process tests above bind against
    let line = line_of(INDEX, "{{ greeting }}");
    match ask(
        &mut debuggee,
        child,
        bpd_core::Request::SetBreakpoints {
            breakpoints: vec![SourceBreakpoint::at(
                1,
                template(&fixture, "index.html"),
                line,
            )],
        },
        &mut seen,
    ) {
        bpd_core::Response::BreakpointsResolved { resolved } => {
            // django has not been asked for the template yet, so it is not
            // loaded and the binding is announced later, while the child runs
            assert!(
                matches!(
                    latest(resolved).as_slice(),
                    [(
                        1,
                        Binding::Unbound { .. }
                            | Binding::Bound { .. }
                            | Binding::BoundInTemplate { .. }
                    )]
                ),
                "the child answered the breakpoint set"
            );
        }
        other => panic!("the child's breakpoints were answered with {other:?}"),
    }

    let (reason, rebound) = match ask(
        &mut debuggee,
        child,
        bpd_core::Request::Run {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        bpd_core::Response::Ran(Running::Stopped { stop, rebound }) => {
            (stop.reason, latest(rebound))
        }
        other => panic!(
            "the child renders the template and was supposed to stop in it: \
             {other:?}"
        ),
    };

    // it bound **in the template**, in the child's own interpreter, and the
    // render is under way rather than finished
    let (bound, nodes) = in_template(&rebound[0].1);
    assert_eq!(bound, line);
    assert!(!nodes.is_empty(), "{nodes:?}");
    let StopReason::Breakpoint { file, line: at, .. } = reason.clone() else {
        panic!("the child stopped for {reason:?}")
    };
    assert_eq!(file, template(&fixture, "index.html").display().to_string());
    assert_eq!(at, line);
    assert_eq!(
        marks(&fixture),
        "before",
        "the child is held inside the render, not after it"
    );

    match ask(
        &mut debuggee,
        child,
        bpd_core::Request::Run {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        bpd_core::Response::Ran(Running::Ended { .. }) => {}
        other => panic!("the child did not end: {other:?}"),
    }
    match ask(
        &mut debuggee,
        parent,
        bpd_core::Request::Wait {
            deadline: Some(LONG_ENOUGH),
        },
        &mut seen,
    ) {
        bpd_core::Response::Ran(Running::Exited { status, .. }) => {
            assert!(status.success(), "the reloader exited {status}");
        }
        other => panic!("the reloader did not end: {other:?}"),
    }
    assert_eq!(marks(&fixture), "after");
}
