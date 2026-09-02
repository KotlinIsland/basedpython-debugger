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
    Binding, Difference, Evaluated, Jump, Jumped, LogRecord, Replacement, Replacements, Resolved,
    Restarted, Snapshot, Source, SourceBreakpoint, Stack, Stop, Threads, Transcript, Variables,
    WorldStopped,
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
    // what a source map made of the two fields above, when one did. an agent
    // reading `demo.by:11` needs to be able to find out where the interpreter
    // really is — the map is the reason the two differ, and an answer that
    // showed only one of them would leave nothing to reconcile them with. the
    // sentence is the core's, so this and DAP's `Source.origin` say the same
    // thing about the same frame
    if let Some(mapping) = &frame.mapping {
        rendered["mapped"] = serde_json::json!(mapping.to_string());
        if let bpd_core::Mapping::FromSource { generated } = mapping {
            rendered["generated"] = serde_json::json!({
                "file": generated.file,
                "line": generated.line,
            });
        }
    }
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
    // a **separate** key, never merged into `frames`. these frames scheduled the
    // ones above rather than calling them, and an agent handed one seamless list
    // would reason about a call chain that never happened

    // an empty list when the stack **is** in a task, and nothing recorded how it
    // was made — a task made before the hook was armed, or one whose creating
    // stack could not be read. an agent shown the same empty answer as a
    // synchronous stack would read a gap in the debugger as a fact about the
    // program, so the two are told apart by `in_a_task` and by the sentence
    if walked.in_a_task && walked.scheduled_by.is_empty() {
        rendered["scheduled_by"] = serde_json::json!([]);
        rendered["scheduled_note"] = bpd_core::TASK_NOT_SEEN.into();
    }
    if !walked.scheduled_by.is_empty() {
        rendered["scheduled_by"] = serde_json::json!(walked.scheduled_by);
        rendered["scheduled_note"] = "this stack is inside an asyncio task. `scheduled_by` is \
                                      where that task was created, innermost first — those frames \
                                      **scheduled** these rather than calling them, and the real \
                                      caller of the outermost frame above is the event loop. it is \
                                      a record taken when the task was made, so its lines are \
                                      where things were then and its frames cannot be read for \
                                      variables"
            .into();
    }
    // and whether that record reaches the program's own entry. the frames a
    // bounded record drops are the **outermost**, so a cut one reads as a task
    // scheduled from the middle of a call chain — which is a fact about the
    // bound rather than about the program
    if walked.scheduling_cut {
        rendered["scheduled_by_cut"] = serde_json::json!(true);
        rendered["scheduled_by_cut_says"] = "this record is the innermost frames \
             only and does not reach the program's own entry. what was scheduled \
             from is above the outermost frame shown, and bpd did not keep it"
            .into();
    }
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

/// what is provable about a frame's names, and for how long
///
/// the stability is rendered as the sentence it is rather than as a tag. an
/// agent reading `until the object's contents are mutable` knows what it may
/// carry the reading past; one reading `"contents"` has to be told separately
pub fn facts(facts: &bpd_core::Facts) -> serde_json::Value {
    let proved: Vec<serde_json::Value> = facts
        .proved
        .iter()
        .map(|fact| {
            serde_json::json!({
                "name": fact.name,
                "scope": fact.scope.to_string(),
                "observed": fact.observed,
                "permanent": fact.stability.is_permanent(),
                "stability": fact.stability.to_string(),
            })
        })
        .collect();

    let silent: Vec<serde_json::Value> = facts
        .silent
        .iter()
        .map(|silent| {
            serde_json::json!({
                "name": silent.name,
                "why": silent.why.to_string(),
            })
        })
        .collect();

    serde_json::json!({
        "proved": proved,
        // a name that produced nothing and said nothing would be
        // indistinguishable from one bound to something uninteresting
        "silent": silent,
        "mode": facts.mode.to_string(),
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

/// what restarting a frame arranged, or cpython's refusal to arrange it
///
/// the notes carry what an agent cannot see and would otherwise assume: that
/// the **thread was let go** and a stop is coming, that the whole caller line
/// runs again rather than only the call, and that the caller holds a value the
/// program never computed until the restarted call finishes. an agent told only
/// "restarted" would ask for the stack of a thread that is running
pub fn restarted(restarted: &Restarted) -> serde_json::Value {
    let mut rendered = serde_json::json!({ "outcome": restarted });
    let mut notes: Vec<String> = Vec::new();

    match restarted {
        Restarted::Arranged(restarting) => {
            rendered["mode"] = restarting.mode.to_string().into();
            // the shared half, so that what DAP says about the program and what
            // this says about it cannot be two different claims
            notes.extend(restarting.told());
            // and the half that is this protocol's: an agent has no `stopped`
            // event telling it to wait, so it would ask this stop another
            // question and be answered about a thread that is running
            notes.push(
                "wait for the next stop rather than asking this one anything \
                 more: it has ended. the stop that arrives is `restarted` at the \
                 first line of the fresh frame, or `restart_abandoned` when the \
                 restart could not be finished — the frame gone and the call not \
                 made again. that stop carries **which** of the reasons it was; \
                 there is more than one and the list grows, so read it rather \
                 than assuming"
                    .to_string(),
            );
            notes.push(
                "and a **third** thing can happen, which is a known gap rather \
                 than an outcome: if a breakpoint, an exception, a pause or a \
                 stopped world holds this thread before the fresh frame is \
                 entered, the restart is taken off and **neither** stop arrives. \
                 you are told about the stop you got and nothing says the restart \
                 ended — so do not wait indefinitely for one of the two above"
                    .to_string(),
            );
        }
        Restarted::Unwinding(unwinding) => {
            notes.extend(unwinding.told());
            // the same warning the rewinding mechanism carries, and for the same
            // reason: the thread is gone, so this stop is no longer a thing to
            // ask questions of
            notes.push(
                "do not ask this stop anything more: it has ended. the frame it \
                 named is still there and is what the next stop will be in"
                    .to_string(),
            );
        }
        Restarted::Reset(reset) => {
            notes.extend(reset.told());
            // and this protocol's half: an agent told "restarted" by the other
            // mechanism is told to wait for a stop, and doing that here would be
            // waiting for something that is never sent
            notes.push(
                "do not wait for another stop. this one is still current and \
                 still answers questions — the frame it names is the restarted \
                 frame, at its first line"
                    .to_string(),
            );
        }
        Restarted::Refused { tried, error } => {
            notes.push(format!(
                "cpython would not move the frame to an exit on any of the lines \
                 {tried:?} — `{error}` — so **none of the program's code ran**. \
                 the frame did not move, no local was bound, and the thread is \
                 still held exactly where it was"
            ));
            notes.push(bpd_core::WHAT_READING_THE_BYTECODE_COSTS.to_string());
        }
    }

    rendered["notes"] = notes.into();
    rendered
}

/// what replacing a file's code did to the process, or what stopped it
///
/// the refusals are rendered as their own sentences rather than summarised.
/// every one of them names the thing that blocked it, and an agent handed
/// "cannot" would be left guessing which of its edits to undo
pub fn replaced(replacement: &Replacements) -> serde_json::Value {
    let mut rendered = serde_json::json!({
        "files": replacement.files,
        "rebound": replacement.rebound,
        "remapped": replacement.remapped,
        // `None` for a refusal decided out of process — an unplaceable `.by` — where
        // the debuggee was never asked and no thread of it was sampled
        "mode": replacement.mode.as_ref().map(ToString::to_string),
    });

    let mut notes: Vec<String> = Vec::new();

    // first, because it happened first and because everything below is read
    // through it: the tables moved under every `.by` line of the build before a
    // single `__code__` was assigned
    if let Some(remapped) = &replacement.remapped {
        notes.push(remapped.to_string());
    }

    for one in &replacement.files {
        let named = one.file.display();
        match &one.outcome {
            Replacement::Applied {
                changed,
                unchanged,
                still_running,
            } => {
                // first, because it qualifies everything under it: a replacement
                // applied under a live frame did not put the process on one
                // version of the code, and an agent reading the changes below
                // without this would act on a process it has the wrong model of
                for running in still_running {
                    notes.push(format!("{named}: {running}"));
                }
                if changed.is_empty() {
                    notes.push(format!(
                        "{named}: nothing needed replacing — the {} code objects of that file \
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
                            "{named}: `{}` now runs the code on disk{moved}. {} function \
                             object(s) in the process held it — every one of them was rebound, \
                             including any a decorator kept",
                            one.function, one.objects
                        ));
                    }
                }
            }
            Replacement::Refused { because } => {
                notes.push(format!(
                    "{named}: nothing was changed. a replacement is never applied partially, \
                     because a process half way between two versions of a file produces \
                     evidence about neither — all {} reasons follow",
                    because.len()
                ));
                notes.extend(because.iter().map(|reason| format!("{named}: {reason}")));
            }
        }
    }

    // the whole set at once rather than per file: binding walks down from each
    // file's root code object, and a replacement that swapped several roots
    // resolved every breakpoint of the build against the code that is running now
    for one in &replacement.rebound {
        notes.push(match &one.binding {
            Binding::Bound { line, .. }
            | Binding::BoundInTemplate { line, .. }
            | Binding::BoundInSource { line, .. } => {
                format!(
                    "breakpoint {} is bound to line {line} now — it was armed on a code object \
                     nothing will execute any more, and was resolved again against the code \
                     that is running",
                    one.id
                )
            }
            Binding::Unbound { reason } => {
                format!("breakpoint {} is **unbound** now: {reason}", one.id)
            }
        });
    }

    if replacement
        .files
        .iter()
        .any(|one| matches!(one.outcome, Replacement::Applied { .. }))
    {
        notes.push(
            "the top level was **not** re-run: no name was bound or unbound and no object was \
             created, so every instance and every reference the program already had is the one \
             it had before"
                .to_string(),
        );
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
            // beside `bound` rather than instead of it, because both are true:
            // the interpreter has somewhere to stop and is not watching it yet.
            // an agent that read `bound` alone would wait at a line nothing is
            // going to offer, and conclude the debugger is broken
            if let Some(after) = entry.waiting_for {
                rendered["armed"] = false.into();
                rendered["waiting_for"] = after.into();
                rendered["note"] = format!(
                    "bound, and not armed yet: it is watched only once breakpoint {after} has been \
                     hit. until then its location has no line events at all, which is what makes \
                     waiting free"
                )
                .into();
            }
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
                Binding::BoundInSource {
                    line,
                    generated,
                    sites,
                    evaluation,
                } => {
                    rendered["bound"] = true.into();
                    rendered["line"] = (*line).into();
                    rendered["evaluation"] = serde_json::json!(evaluation);
                    rendered["sites"] = serde_json::json!(sites);
                    // both locations, because both are real and neither stands
                    // in for the other. `line` is the `.by` the agent reading
                    // this asked about; `generated` is where the interpreter
                    // will really stop, and it is what makes the code objects
                    // in `sites` mean anything
                    rendered["generated"] = serde_json::json!(generated);
                    if asked.is_some_and(|asked| asked.line != *line) {
                        rendered["moved"] = format!(
                            "line {} generated nothing bpd can stop on, so this moved to line \
                             {line}, which did",
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
///
/// `taking_up` is the second one, and it replaced a constant `debugged: false`
/// that became wrong the moment `debug_children` existed. it is deliberately not
/// spelled `debugged`: the report is made in the parent at the instant the child
/// is asked for, so what is knowable is the **attempt**, and `attached.sessions`
/// is what says a child really arrived
pub fn spawned(child: &bpd_core::Spawn) -> serde_json::Value {
    serde_json::json!({
        "says": child.to_string(),
        "event": child.event,
        "executable": child.executable,
        "arguments": child.arguments,
        "verdict": child.verdict,
        "certain": child.verdict.certain(),
        "taking_up": child.taking_up,
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

/// what is holding an object, and what the walk could not see
///
/// `coverage` is rendered on **every** answer rather than only when something
/// is missing, because an agent cannot tell "nothing was hidden" from "this
/// server does not say" — the same reason `output_complete` is always present
/// on an exit
pub fn retainers(found: &bpd_core::Retainers) -> serde_json::Value {
    serde_json::json!({
        "of": found.of,
        "held_by": found
            .found
            .iter()
            .map(|retainer| serde_json::json!({
                "kind": retainer.kind,
                "is": retainer.described,
                // absent rather than null when it could not be read: `through`
                // saying nothing is different from a retainer that holds it
                // nowhere, and nowhere is not a thing that happens
                "through": retainer.through,
            }))
            .collect::<Vec<_>>(),
        "coverage": {
            "untracked": found.coverage.untracked,
            "not_python": found.coverage.not_python,
            "mode": found.coverage.mode.to_string(),
        },
        "says": "what holds this object, as the collector's referent graph has \
                 it. `coverage` is not a footnote: a walk of this kind is blind \
                 to whole categories of holder, and a list without it answers a \
                 narrower question than the one asked",
    })
}

/// whether recording is on, and what the window holds
pub fn recording(on: bool, held: u64, dropped: u64) -> serde_json::Value {
    serde_json::json!({
        "recording": on,
        "held": held,
        "dropped": dropped,
        "says": if on {
            "recording where the program goes. **this is the one mode that turns \
             off what makes bpd fast**: a line is normally watched once and then \
             disabled, and a recorder needs every execution of it — measured at \
             4x a bare run. it records where, never what: a copy of the values \
             per line costs five times as much again and is unbounded"
        } else {
            "not recording. the trail is kept when recording stops, because \
             stopping is what somebody does in order to read it"
        },
    })
}

/// where the program has been
pub fn trail(went: &bpd_core::Trail) -> serde_json::Value {
    serde_json::json!({
        "went": went.went,
        "dropped": went.dropped,
        "window": went.window,
        "recording": went.recording,
        "says": if went.dropped == 0 {
            "every step since recording started".to_string()
        } else {
            format!(
                "the last {} steps. **{} fell out of the window before these**, \
                 so the oldest entry here is not where the recording began",
                went.went.len(),
                went.dropped
            )
        },
        // what a step's `held` means, stated **without inferring which depth was
        // used**. `Trail` does not carry the depth, and the first cut of this
        // guessed it from whether any values had turned up — so an empty trail,
        // or one whose frames could not be read, told an agent to re-record at
        // the depth it was already using
        "values": "each step carries `held`: absent when the recording keeps only \
                   the location, `null` when that step's frame could not be read, \
                   and otherwise the names it bound with `dropped` counting what \
                   one step does not keep. the text is read without running any \
                   of the program, so it is weaker than a repr and cannot be \
                   wrong",
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
            mapping: None,
            kind: bpd_core::FrameKind::Python {
                function: "work".to_string(),
                first_line: 1,
            },
        }
    }

    #[test]
    fn a_by_breakpoint_is_rendered_with_both_of_its_locations() {
        use bpd_core::source_map::Located;

        // an agent reading this has one answer with two true locations in it.
        // the `.by` line is the one it asked about; the generated location is
        // what makes the code objects beside it mean anything
        let rendered = breakpoints(
            &[Resolved {
                waiting_for: None,
                id: 1,
                binding: Binding::BoundInSource {
                    line: 7,
                    generated: Located {
                        file: std::path::PathBuf::from("/tmp/build/app.py"),
                        line: 19,
                    },
                    sites: vec![Site {
                        qualname: "main".to_string(),
                        first_line: 12,
                        offset: 4,
                    }],
                    evaluation: Evaluation::Always,
                },
            }],
            &[SourceBreakpoint::at(1, "/src/app.by", 7)],
        );

        let [only] = &rendered[..] else {
            panic!("one breakpoint was asked about, and {rendered:?} came back")
        };
        assert_eq!(only["bound"], true);
        assert_eq!(only["line"], 7);
        assert_eq!(only["file"], "/src/app.by");
        assert_eq!(only["generated"]["file"], "/tmp/build/app.py");
        assert_eq!(only["generated"]["line"], 19);
        assert!(
            only["moved"].is_null(),
            "it bound on the line that was asked for: {only}"
        );
    }

    #[test]
    fn a_stack_that_was_cut_says_how_much_of_it_is_missing() {
        let whole = stack(&Stack {
            in_a_task: false,
            scheduled_by: Vec::new(),
            scheduling_cut: false,
            frames: vec![frame(0)],
            depth: 1,
            mode: Mode::NonStop,
        });
        assert!(
            whole.get("frames_omitted").is_none(),
            "nothing was left out and it said something was: {whole}"
        );

        let cut = stack(&Stack {
            in_a_task: false,
            scheduled_by: Vec::new(),
            scheduling_cut: false,
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
                    waiting_for: None,
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
                    waiting_for: None,
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
