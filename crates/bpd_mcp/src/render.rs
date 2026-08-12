//! turning what a session answered into the json an agent reads
//!
//! two rules run through all of it, and they are the same rule:
//!
//! - **nothing is elided silently.** every bound that bit is carried as the
//!   core's own sentence about it, which names what was left out, how much of it
//!   there was, and which field to raise. an agent cannot see the ellipsis a
//!   person would
//! - **structure and prose, not one or the other.** the machine readable form is
//!   the core type's own serde, so nothing is dropped on the way; the prose
//!   beside it is the core type's own `Display`, so there is one wording rather
//!   than one per front end

use bpd_core::{
    Binding, Difference, Evaluated, Jump, Jumped, LogRecord, Replaced, Replacement, Resolved,
    Snapshot, Source, SourceBreakpoint, Stack, Stop, Threads, Transcript, Variables, WorldStopped,
};

/// every session this debuggee holds
///
/// what makes a second one learnable. an agent that had to infer a session from
/// a stop number would be inferring it from a number two sessions can both hold
pub fn sessions(joined: &[bpd_core::Joined]) -> Vec<serde_json::Value> {
    joined
        .iter()
        .map(|session| {
            let mut rendered = serde_json::json!({
                "session": session.session.get(),
                // whether bpd started this process. `false` is a debugged fork,
                // and it is not a detail: bpd is not its parent, so it cannot be
                // terminated and its exit code is not bpd's to read
                "ours": session.ours,
                "held": session.held.iter().map(stop).collect::<Vec<_>>(),
            });
            match session.exit {
                None => rendered["outcome"] = "running".into(),
                Some(bpd_core::Exit::Code(code)) => {
                    rendered["outcome"] = "exited".into();
                    rendered["exit_code"] = code.into();
                }
                // deliberately without an `exit_code` field rather than with a
                // null one: the program is over and the number would be invented
                Some(bpd_core::Exit::Unknown) => {
                    rendered["outcome"] = "ended".into();
                    rendered["note"] = "the program is over and bpd cannot say what it \
                                        exited with — bpd did not start that process"
                        .into();
                }
            }
            rendered
        })
        .collect()
}

/// one held thread, as the answer to a control tool
pub fn stop(stopped: &Stop) -> serde_json::Value {
    serde_json::json!({
        // which debugged process this stop is of. two agents both count their
        // stops from one, so the number alone does not name a stop once a
        // program has forked into a debugged child
        "session": stopped.session.get(),
        "stop": stopped.stop,
        "thread": stopped.thread,
        "reason": stopped.reason,
        // what this thread holds that another can be waiting for. empty means
        // nothing bpd can know about was held, **not** that nothing was
        "holding": stopped
            .holding
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
    })
}

/// where one frame is, and **what kind of frame it is**
///
/// the kind is rendered rather than flattened away, because not every frame in
/// a stack is one the interpreter has: a django template frame is synthesised
/// from the node django is rendering, and an agent that read it as python would
/// go looking for a `.html` file's local variables. so `kind` is always present,
/// and the fields that belong to only one kind appear only for that kind
fn located(frame: &bpd_core::Frame) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "frame": frame.id.depth,
        "file": frame.file,
        "line": frame.line,
    });
    match &frame.kind {
        bpd_core::FrameKind::Python {
            function,
            first_line,
        } => {
            rendered["kind"] = serde_json::json!("python");
            rendered["function"] = serde_json::json!(function);
            rendered["first_line"] = serde_json::json!(first_line);
        }
        bpd_core::FrameKind::Template { node, python } => {
            rendered["kind"] = serde_json::json!("template");
            rendered["node"] = serde_json::json!(node);
            // the frame that is really running. python is evaluated there, and
            // its scopes are read there
            rendered["python_frame"] = serde_json::json!(python.depth);
        }
    }
    rendered
}

/// one held thread's stack
pub fn stack(walked: &Stack) -> serde_json::Value {
    let frames: Vec<serde_json::Value> = walked.frames.iter().map(located).collect();

    let mut rendered = serde_json::json!({
        "frames": frames,
        "depth": walked.depth,
        "mode": walked.mode.to_string(),
    });
    if walked.frames.len() < walked.depth {
        rendered["frames_omitted"] = serde_json::json!({
            "count": walked.depth - walked.frames.len(),
            "says": format!(
                "the walk was bounded at {} frames and the stack is {} deep. \
                 ask again with a larger `top`, or none at all for the whole \
                 stack",
                walked.frames.len(),
                walked.depth
            ),
        });
    }
    rendered
}

/// what one scope of one frame holds
pub fn variables(read: &Variables) -> serde_json::Value {
    serde_json::json!({
        "entries": read.entries,
        // a name the scope has and the frame does not hold yet, and a name whose
        // value the frame does not expose. neither is a value and neither is
        // absent, so neither is left out
        "unbound": read.unbound,
        "unreadable": read.unreadable,
        "left_out": read
            .omitted
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
        "mode": read.mode.to_string(),
    })
}

/// what a template frame's django context holds, layer by layer
///
/// the layers stay layers, and the shadowing between them is named rather than
/// left to be worked out: which layer holds a name is what decides the render,
/// and an agent handed a merged mapping cannot see that at all
pub fn template_context(context: &bpd_core::TemplateContext) -> serde_json::Value {
    let layers: Vec<serde_json::Value> = context
        .layers
        .iter()
        .map(|layer| {
            serde_json::json!({
                "layer": layer.index,
                "entries": layer.entries,
                "left_out": layer
                    .omitted
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<String>>(),
            })
        })
        .collect();

    let shadowed: Vec<serde_json::Value> = context
        .shadowed()
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name.name,
                "layers": name.layers,
                "wins": name.layers.last(),
            })
        })
        .collect();

    serde_json::json!({
        "layers": layers,
        "shadowed": shadowed,
        "mode": context.mode.to_string(),
        "note": "django resolves a name by walking the layers from the last \
                 backwards and taking the first that holds it, so the **last** \
                 layer holding a name is the one the template renders",
    })
}

/// what an expression did, or what a write left behind
///
/// an expression that raised has an answer and the answer is the exception, so
/// this is a result rather than a failure
pub fn evaluated(result: &Evaluated) -> serde_json::Value {
    serde_json::json!({ "result": result })
}

/// what a jump did, and where the frame is now
///
/// `at` is the frame's own answer, read after the move, and it is the only
/// thing that can be: no `LINE` event is delivered for the line a jump moves
/// to. the notes are not decoration — each of them is a fact about the program
/// that nothing else in the session will ever mention, and an agent that was
/// not told would go on to make a wrong claim about a breakpoint that did not
/// fire or a local that is suddenly `None`
pub fn jumped(jump: &Jumped) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "at": jump.at,
        "outcome": jump.outcome,
        "mode": jump.mode.to_string(),
    });

    let mut notes: Vec<String> = Vec::new();
    match &jump.outcome {
        Jump::Moved {
            from,
            bound_to_none,
            unannounced,
        } => {
            notes.push(format!(
                "the thread is still held, at {}. the lines between {from} and \
                 {} were not executed, and the cleanup of any block the move \
                 left was not run — jumping out of a `with` does not call \
                 `__exit__` and jumping out of a `try` does not run its \
                 `finally`",
                jump.at, jump.at.line
            ));
            if !unannounced.is_empty() {
                notes.push(format!(
                    "breakpoint(s) {unannounced:?} are bound to line {} and will \
                     **not** fire for this pass: no line event is delivered for \
                     the line a jump moves to. they are still set, and fire the \
                     next time that line runs",
                    jump.at.line
                ));
            }
            if !bound_to_none.is_empty() {
                notes.push(format!(
                    "{bound_to_none:?} held nothing before the move and hold \
                     `None` now. cpython binds every unbound local of a frame as \
                     part of a jump — this is a change to the program that the \
                     jump made"
                ));
            }
        }
        Jump::Refused { wanted, error } => notes.push(format!(
            "cpython refused the move to line {wanted} — `{error}` — and the \
             frame did not move. it is still at {}",
            jump.at
        )),
    }
    rendered["notes"] = notes.into();
    rendered
}

/// what replacing a file's code did to the process, or what stopped it
///
/// the refusals are rendered as their own sentences rather than summarised.
/// every one of them names the thing that blocked it, and an agent handed
/// "cannot" would be left guessing which of its edits to undo
pub fn replaced(replacement: &Replaced) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "file": replacement.file,
        "outcome": replacement.outcome,
        "mode": replacement.mode.to_string(),
    });

    let mut notes: Vec<String> = Vec::new();
    match &replacement.outcome {
        Replacement::Applied {
            changed,
            rebound,
            unchanged,
        } => {
            if changed.is_empty() {
                notes.push(format!(
                    "nothing needed replacing: the {} code objects of that file \
                     on disk are exactly what the process is running",
                    unchanged.len()
                ));
            } else {
                for one in changed {
                    let moved = if one.was_at == one.now_at {
                        String::new()
                    } else {
                        format!(
                            ", which has moved from line {} to {}",
                            one.was_at, one.now_at
                        )
                    };
                    notes.push(format!(
                        "`{}` now runs the code on disk{moved}. {} function \
                         object(s) in the process held it — every one of them was \
                         rebound, including any a decorator kept",
                        one.function, one.objects
                    ));
                }
            }
            for one in rebound {
                notes.push(match &one.binding {
                    Binding::Bound { line, .. } | Binding::BoundInTemplate { line, .. } => {
                        format!(
                            "breakpoint {} is bound to line {line} now — it was \
                             armed on a code object nothing will execute any \
                             more, and was resolved again against the code that \
                             is running",
                            one.id
                        )
                    }
                    Binding::Unbound { reason } => {
                        format!("breakpoint {} is **unbound** now: {reason}", one.id)
                    }
                });
            }
            notes.push(
                "the top level was **not** re-run: no name was bound or unbound \
                 and no object was created, so every instance and every \
                 reference the program already had is the one it had before"
                    .to_string(),
            );
        }
        Replacement::Refused { because } => {
            notes.push(format!(
                "nothing was changed. a replacement is never applied partially, \
                 because a process half way between two versions of a file \
                 produces evidence about neither — all {} reasons follow",
                because.len()
            ));
            notes.extend(because.iter().map(ToString::to_string));
        }
    }
    rendered["notes"] = notes.into();
    rendered
}

/// what every thread of the debuggee was doing
pub fn threads(census: &Threads) -> serde_json::Value {
    serde_json::json!({
        "threads": census.threads,
        "settle_ms": u64::try_from(census.settle.as_millis()).unwrap_or(u64::MAX),
        "mode": census.mode.to_string(),
        "note": "everything here about a thread bpd is not holding is a sample \
                 taken twice, `settle_ms` apart. `still` means it was in the \
                 same place both times, which is where to look rather than what \
                 is wrong",
    })
}

/// what stopping the world managed to stop
pub fn world(stopped: &WorldStopped) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "held": stopped.held,
        "native": stopped.native,
        "whole_program": stopped.native.is_empty(),
    });
    if !stopped.native.is_empty() {
        rendered["note"] = format!(
            "{} thread(s) could not be held and are still running: they are \
             parked in a C call, where there is no monitoring event to hold one \
             at. a read taken now is not a whole-program snapshot",
            stopped.native.len()
        )
        .into();
    }
    rendered
}

/// how every breakpoint of a set resolved
///
/// `requested` is what was asked for, by the same index. a breakpoint that moved
/// says which line it moved to, and one that did not bind says why in the core's
/// own words — never "set"
pub fn breakpoints(
    resolved: &[Resolved],
    requested: &[SourceBreakpoint],
) -> Vec<serde_json::Value> {
    resolved
        .iter()
        .map(|entry| {
            let asked = requested.iter().find(|wanted| wanted.id == entry.id);
            let mut rendered = serde_json::json!({ "id": entry.id });
            if let Some(asked) = asked {
                rendered["file"] = asked.file.display().to_string().into();
                rendered["requested_line"] = asked.line.into();
            }
            match &entry.binding {
                Binding::Bound {
                    line,
                    sites,
                    evaluation,
                } => {
                    rendered["bound"] = true.into();
                    rendered["line"] = (*line).into();
                    rendered["evaluation"] = serde_json::json!(evaluation);
                    // one source line can belong to several code objects — a
                    // `def` line is in the class body and is the method's first
                    // line — and every one of them is armed
                    rendered["sites"] = serde_json::json!(sites);
                    if asked.is_some_and(|asked| asked.line != *line) {
                        rendered["moved"] = format!(
                            "line {} is not executable, so this moved to line \
                             {line}, which is",
                            asked.map_or(0, |asked| asked.line)
                        )
                        .into();
                    }
                }
                Binding::BoundInTemplate {
                    line,
                    nodes,
                    evaluation,
                } => {
                    rendered["bound"] = true.into();
                    rendered["line"] = (*line).into();
                    rendered["evaluation"] = serde_json::json!(evaluation);
                    // a template has no code object and no offset. what is
                    // behind the breakpoint is a django node, reached through
                    // `Node.render_annotated`, and saying so is what stops this
                    // reading as a python binding
                    rendered["template_nodes"] = serde_json::json!(nodes);
                    if asked.is_some_and(|asked| asked.line != *line) {
                        rendered["moved"] = format!(
                            "line {} renders no django node, so this moved to \
                             line {line}, which does",
                            asked.map_or(0, |asked| asked.line)
                        )
                        .into();
                    }
                }
                Binding::Unbound { reason } => {
                    rendered["bound"] = false.into();
                    rendered["unbound"] = serde_json::json!(reason);
                    rendered["says"] = reason.to_string().into();
                }
            }
            rendered
        })
        .collect()
}

/// what a debug script did, step by step
///
/// the whole transcript, because the transcript **is** the answer — an agent
/// given only where a script ended cannot tell why, and will guess. the
/// structure is the core type's own serde and the sentence beside it is the
/// core type's own `Display`, so there is one wording of what happened rather
/// than one per front end
pub fn transcript(ran: &Transcript) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "at_most": ran.at_most,
        "bytes": ran.bytes,
        "records": ran.records,
        "outcome": ran.outcome,
        "says": ran.outcome.to_string(),
        "partial": ran.partial(),
    });
    if !ran.rebound.is_empty() {
        // loading a file changes what a breakpoint resolves to, and it happened
        // while the script was running the program
        rendered["rebound"] = serde_json::json!(ran.rebound);
    }
    rendered
}

/// a stop's state, at the level of detail the query asked for
///
/// the id comes first because it is what a later `diff` is written against, and
/// what did not fit the budget is named rather than being absent
pub fn state(snapshot: &Snapshot) -> serde_json::Value {
    let frames: Vec<serde_json::Value> = snapshot
        .state
        .frames
        .iter()
        .map(|described| {
            let mut rendered = located(&described.frame);
            rendered["scopes"] = serde_json::json!(
                described
                    .scopes
                    .iter()
                    .map(|read| serde_json::json!({
                        "scope": read.scope,
                        "entries": read.entries,
                        "unbound": read.unbound,
                        "unreadable": read.unreadable,
                        "left_out": read
                            .omitted
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<String>>(),
                    }))
                    .collect::<Vec<serde_json::Value>>()
            );
            if let Some(source) = &described.source {
                rendered["source"] = source_of(source);
            }
            rendered
        })
        .collect();

    let mut rendered = serde_json::json!({
        "snapshot": snapshot.id.to_string(),
        "stop": snapshot.state.stop,
        "thread": snapshot.state.thread,
        "reason": snapshot.state.reason,
        "frames": frames,
        "depth": snapshot.state.depth,
        "values": snapshot.state.values,
        "mode": snapshot.state.mode.to_string(),
        "bytes": snapshot.state.bytes,
    });
    if !snapshot.state.left_out.is_empty() {
        // an agent cannot see the elision a person would. a part the budget cut
        // says which part it was and what to raise
        rendered["left_out"] = serde_json::json!(
            snapshot
                .state
                .left_out
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>()
        );
    }
    rendered
}

/// the source around a frame's line, or the reason bpd will not claim any
fn source_of(source: &Source) -> serde_json::Value {
    match source {
        Source::Lines {
            first,
            at,
            lines,
            total,
        } => serde_json::json!({
            "verified": true,
            "first_line": first,
            "at": at,
            "lines": lines,
            "file_lines": total,
            "says": "the file was compiled in the debuggee and this frame's own \
                     code object is in what came out, so these lines are the \
                     lines that are running. they are clamped to that code \
                     object, because nothing outside it was checked",
        }),
        Source::Unverified { why } => serde_json::json!({
            "verified": false,
            "says": why.to_string(),
        }),
    }
}

/// what changed between two states
///
/// the difference is the answer. `not_compared` is the part that keeps it
/// honest: a reading a bound cut short is not evidence that something is
/// unchanged, and it is never counted as though it were
pub fn difference(difference: &Difference) -> serde_json::Value {
    let side = |taken: &bpd_core::Taken| {
        serde_json::json!({
            "snapshot": taken.id.to_string(),
            "stop": taken.stop,
            "thread": taken.thread,
            "mode": taken.mode.to_string(),
            "stop_has_ended": taken.stop_has_ended,
        })
    };

    let mut rendered = serde_json::json!({
        "before": side(&difference.before),
        "after": side(&difference.after),
        "frames": difference.frames,
        "changed": difference.changed,
        "added": difference.added,
        "removed": difference.removed,
        "unchanged": difference
            .unchanged
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
        "not_compared": difference
            .not_compared
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>(),
    });
    let sampled = |mode: &bpd_core::Mode| matches!(mode, bpd_core::Mode::NonStop);
    if sampled(&difference.before.mode) || sampled(&difference.after.mode) {
        rendered["note"] = "at least one of these was read in non-stop mode, \
             where a stop holds one thread and the rest of the program keeps \
             running. each is a sample rather than a whole state, so what \
             changed is what changed between two samples. `stop_the_world` is \
             what makes a reading a whole-program one"
            .into();
    }
    rendered
}

/// one child process the program started
///
/// `says` carries the same sentence a person is shown, so an agent and a human
/// looking at the same session are looking at the same words. the structured
/// fields are beside it rather than instead of it, because an agent that has to
/// parse a sentence to find the command is an agent that will parse it wrongly
///
/// `certain` is the field that matters most, and it is why the verdict is not a
/// boolean: `bpd` reads an argument vector, so it can be sure a child runs this
/// interpreter and it cannot be sure what `/usr/bin/env python3` will do. an
/// agent that treated the two the same would act on a guess
pub fn spawned(child: &bpd_core::Spawn) -> serde_json::Value {
    serde_json::json!({
        "says": child.to_string(),
        "event": child.event,
        "executable": child.executable,
        "arguments": child.arguments,
        "verdict": child.verdict,
        "certain": child.verdict.certain(),
        "debugged": false,
    })
}

/// a way of starting a child this interpreter does not let `bpd` see
///
/// the field an agent has to act on is `silence_is_not_evidence`. everything
/// else `bpd` reports is a positive claim, and this is the one message that
/// says what an *absence* of claims no longer rules out — an agent that missed
/// it would conclude from an empty `started` that the program has no children
pub fn blind_to(blindspot: &bpd_core::Blindspot) -> serde_json::Value {
    serde_json::json!({
        "says": blindspot.to_string(),
        "silence_is_not_evidence": true,
    })
}

/// one logpoint record
pub fn logged(record: &LogRecord) -> serde_json::Value {
    serde_json::json!({
        "breakpoint": record.breakpoint,
        "file": record.file,
        "line": record.line,
        "thread": record.thread,
        "hit": record.hit,
        "message": record.message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpd_core::{Evaluation, Mode, Site, Unbound};

    fn frame(depth: u32) -> bpd_core::Frame {
        bpd_core::Frame {
            id: bpd_core::FrameId { stop: 1, depth },
            file: "/tmp/app.py".to_string(),
            line: 4,
            kind: bpd_core::FrameKind::Python {
                function: "work".to_string(),
                first_line: 1,
            },
        }
    }

    #[test]
    fn a_stack_that_was_cut_says_how_much_of_it_is_missing() {
        let whole = stack(&Stack {
            frames: vec![frame(0)],
            depth: 1,
            mode: Mode::NonStop,
        });
        assert!(
            whole.get("frames_omitted").is_none(),
            "nothing was left out and it said something was: {whole}"
        );

        let cut = stack(&Stack {
            frames: vec![frame(0)],
            depth: 9,
            mode: Mode::NonStop,
        });
        assert_eq!(cut["frames_omitted"]["count"], 8);
        assert!(
            cut["frames_omitted"]["says"]
                .as_str()
                .expect("a cut says why")
                .contains("`top`"),
            "an elision has to say how to ask for the rest, and said {cut}"
        );
    }

    #[test]
    fn a_breakpoint_that_moved_says_so_and_one_that_did_not_bind_says_why() {
        let requested = vec![
            SourceBreakpoint::at(1, "/tmp/app.py", 7),
            SourceBreakpoint::at(2, "/tmp/later.py", 3),
        ];
        let rendered = breakpoints(
            &[
                Resolved {
                    id: 1,
                    binding: Binding::Bound {
                        line: 9,
                        sites: vec![Site {
                            qualname: "main".to_string(),
                            first_line: 1,
                            offset: 12,
                        }],
                        evaluation: Evaluation::Always,
                    },
                },
                Resolved {
                    id: 2,
                    binding: Binding::Unbound {
                        reason: Unbound::NotLoaded {
                            file: "/tmp/later.py".into(),
                            templates_available: false,
                        },
                    },
                },
            ],
            &requested,
        );

        assert_eq!(rendered[0]["bound"], true);
        assert_eq!(rendered[0]["line"], 9);
        assert!(
            rendered[0]["moved"]
                .as_str()
                .expect("it moved")
                .contains("line 9"),
            "said {}",
            rendered[0]
        );

        // never "set". a breakpoint with nothing behind it is reported as
        // having nothing behind it, with the reason
        assert_eq!(rendered[1]["bound"], false);
        assert!(
            rendered[1]["says"]
                .as_str()
                .expect("an unbound breakpoint says why")
                .contains("imported later"),
            "said {}",
            rendered[1]
        );
    }
}
