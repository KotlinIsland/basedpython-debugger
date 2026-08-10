//! walking a debug script against a debuggee, and writing down what happened
//!
//! the script runs **here**, in the engine, driving the session. only the
//! predicates inside it reach the debuggee, through the machinery a breakpoint
//! condition already uses — so the program under test is disturbed by exactly
//! the evaluations that were asked for and nothing else
//!
//! ## it drives one thread
//!
//! a stop holds one thread, so a script does too: the one its starting stop
//! holds. every control step resumes that thread by name and no other, and a
//! stop that arrives on a different thread halts the script rather than being
//! taken as where the step landed. the thread the step was about is still
//! running at that point, and there is nothing truthful to say about where a
//! running thread is
//!
//! ## why a `run_to` belongs in here and not in a front end
//!
//! it is a composition: arm a breakpoint, run, take it back off. an adapter
//! that performed it would be making a decision about the program, and under a
//! deadline it could not finish the composition at all — a one-shot breakpoint
//! cannot be removed from a program that is running. in here the engine owns
//! the whole of it, including the removal, and the transcript says what was
//! armed and what became of it. the one case where it cannot be taken off is
//! reported as [`Disarmed::StillArmed`] rather than left to be discovered

use std::num::NonZeroU32;
use std::path::Path;
use std::time::Instant;

use bpd_core::{
    Answered, At, Binding, Bound, Budget, Content, Detail, Did, Disarmed, Evaluated, FrameId,
    Halted, HitCondition, Landed, Outcome, Predicate, Record, Refusal, Reporting, Resolved,
    Running, Script, SourceBreakpoint, Step, StepKind, Stop, StopReason, Transcript, Which,
    exit_code,
};

use super::Debuggee;
use crate::{Error, Result};

impl Debuggee {
    /// run a whole debug script against the thread `stop` holds
    ///
    /// the transcript is the answer, whatever became of the script. an error
    /// out of here describes `bpd`'s own machinery — a socket, a process —
    /// rather than the program, because everything the *program* did is a
    /// record
    pub(super) fn execute(
        &mut self,
        stop: u64,
        script: &Script,
        reporting: &mut dyn Reporting,
    ) -> Result<Transcript> {
        // examined before anything is touched. that a step tree can be read
        // before it runs is the whole reason it is not submitted python
        script
            .examine()
            .map_err(|reason| bpd_core::Error::ScriptRefused { reason })?;

        let cursor = self
            .held
            .iter()
            .find(|held| held.stop == stop)
            .cloned()
            .ok_or_else(|| bpd_core::Error::Refused {
                reason: Refusal::NoSuchStop {
                    stop,
                    held: self.held.iter().map(|held| held.stop).collect(),
                },
            })?;

        let own = own_id(&self.armed, script)?;
        let at_most = script.at_most();
        let mut run = Run {
            debuggee: self,
            reporting,
            budget: script.budget,
            until: Instant::now() + script.budget.wall(),
            cursor,
            records: Vec::new(),
            rebound: Vec::new(),
            bytes: 0,
            spent: 0,
            own,
            doing: "the script",
        };

        let outcome = match run.block(&script.steps, "")? {
            Flow::Carry => Outcome::Ran,
            Flow::Over(outcome) => outcome,
        };
        Ok(Transcript {
            at_most,
            bytes: run.bytes,
            records: run.records,
            rebound: run.rebound,
            outcome,
        })
    }
}

/// the id a `run_to` arms its own breakpoint under
///
/// one no breakpoint of the client's set uses, so a stop that names it is
/// unambiguously the script's. a set that already uses the largest id there is
/// gets the script refused rather than a breakpoint whose reports belong to two
/// things at once
fn own_id(armed: &[SourceBreakpoint], script: &Script) -> Result<u32> {
    let highest = armed
        .iter()
        .map(|breakpoint| breakpoint.id)
        .max()
        .unwrap_or(0);
    if highest == u32::MAX {
        // a script with no `run_to` arms nothing, so a set with no room left in
        // it is not that script's problem — refusing it would be refusing a
        // script that works
        if arms(&script.steps) {
            return Err(bpd_core::Error::ScriptRefused {
                reason: bpd_core::script::Refused::NoBreakpointIdLeft { highest },
            }
            .into());
        }
        return Ok(u32::MAX);
    }
    Ok(highest + 1)
}

/// whether any step of this tree arms a breakpoint of the script's own
fn arms(steps: &[Step]) -> bool {
    steps.iter().any(|step| match step {
        Step::RunTo { .. } => true,
        Step::If {
            then, otherwise, ..
        } => arms(then) || arms(otherwise),
        Step::While { body, .. } => arms(body),
        _ => false,
    })
}

/// what the executor does after one step
#[expect(
    clippy::large_enum_variant,
    reason = "the whole of how a script ended is what `Over` carries, and there \
              is exactly one of these alive at a time. boxing it would put an \
              allocation on the path of every step to save stack bytes on a \
              value that is returned once"
)]
enum Flow {
    /// carry on with the next step
    Carry,
    /// the script is over, and this is how
    Over(Outcome),
}

/// why a step ended the script
///
/// separate from [`Halted`] because one of them is not a halt: a wall clock
/// budget that ran out is the transcript being **partial**, and labelling it a
/// failure of the step would say the program did something it did not
enum Ending {
    /// the step failed
    Halted(Halted),
    /// the script's clock ran out while the program was running
    OutOfTime,
}

/// what stop reason a control step is waiting for
///
/// a step that landed somewhere else has not done what it said, and the steps
/// after it would run at a place the script did not intend
#[derive(Clone, Copy)]
enum Wanted {
    /// a step of the debugger's
    Stepped,
    /// whatever the thread does next
    Anything,
    /// the script's own breakpoint
    Breakpoint(u32),
}

impl Wanted {
    fn matches(self, reason: &StopReason) -> bool {
        match (self, reason) {
            (Self::Anything, _) | (Self::Stepped, StopReason::Stepped { .. }) => true,
            (Self::Breakpoint(own), StopReason::Breakpoint { breakpoints, .. }) => {
                breakpoints.contains(&own)
            }
            _ => false,
        }
    }
}

/// one script, part way through
struct Run<'a> {
    debuggee: &'a mut Debuggee,
    reporting: &'a mut dyn Reporting,
    budget: Budget,
    /// when the wall clock budget runs out
    until: Instant,
    /// the stop the script is at, which is where every step happens
    cursor: Stop,
    records: Vec<Record>,
    rebound: Vec<Resolved>,
    bytes: u64,
    spent: u32,
    /// the id a `run_to` arms its own breakpoint under
    own: u32,
    /// what the step being run is called, for a refusal to name
    ///
    /// set in [`Run::one`], which every step of a script goes through, so it is
    /// the name of the step a refusal is about and never of the one before it
    doing: &'static str,
}

impl Run<'_> {
    /// run a block of steps in order, stopping at the first one that ends the
    /// script
    fn block(&mut self, steps: &[Step], prefix: &str) -> Result<Flow> {
        for (index, step) in steps.iter().enumerate() {
            let path = position(prefix, index);
            match self.one(step, &path)? {
                Flow::Carry => {}
                over @ Flow::Over(_) => return Ok(over),
            }
        }
        Ok(Flow::Carry)
    }

    fn one(&mut self, step: &Step, path: &str) -> Result<Flow> {
        self.doing = step.name();
        match step {
            Step::If {
                predicate,
                then,
                otherwise,
            } => self.branch(predicate, then, otherwise, path),
            Step::While {
                predicate,
                limit,
                body,
            } => self.repeat(predicate, *limit, body, path),
            leaf => {
                if let Some(exhausted) = self.charge(path) {
                    return Ok(Flow::Over(exhausted));
                }
                self.leaf(leaf, path)
            }
        }
    }

    // ---- the steps that are not control flow -----------------------------

    fn leaf(&mut self, step: &Step, path: &str) -> Result<Flow> {
        let from = At::of(&self.cursor);
        match step {
            Step::StepOver => self.stepped(StepKind::Over, path, from),
            Step::StepIn => self.stepped(StepKind::In, path, from),
            Step::StepOut => self.stepped(StepKind::Out, path, from),
            Step::Continue => self.carried_on(path, from),
            Step::RunTo {
                file,
                line,
                condition,
                hits,
            } => self.ran_to(file, *line, condition.as_deref(), *hits, path, from),
            Step::Eval {
                expression,
                frame,
                detail,
            } => self.evaluated(expression, *frame, *detail, path, from),
            Step::Stack { top } => self.walked(*top, path, from),
            Step::Log { note } => {
                Ok(self.then(path, from, Did::Logged { note: note.clone() }, None))
            }
            Step::Finish { because } => {
                let flow = self.then(
                    path,
                    from,
                    Did::Finished {
                        because: because.clone(),
                    },
                    None,
                );
                Ok(match flow {
                    // a budget that ran out on the very record that ends the
                    // script is still a budget that ran out, and the transcript
                    // says so rather than reading as complete
                    Flow::Over(exhausted) => Flow::Over(exhausted),
                    Flow::Carry => Flow::Over(Outcome::Finished {
                        at: path.to_string(),
                        because: because.clone(),
                    }),
                })
            }
            Step::If { .. } | Step::While { .. } => {
                unreachable!("control flow is handled before a step reaches here, and {step:?} did")
            }
        }
    }

    fn stepped(&mut self, kind: StepKind, path: &str, from: At) -> Result<Flow> {
        let stop = self.cursor.stop;
        if let Err(error) = self.debuggee.step_thread(stop, kind, self.reporting) {
            return self.refused(path, from, error);
        }
        let (landed, ending) = self.what_happened(Wanted::Stepped)?;
        Ok(self.then(path, from, Did::Stepped { kind, landed }, ending))
    }

    fn carried_on(&mut self, path: &str, from: At) -> Result<Flow> {
        let thread = self.cursor.thread;
        let letting = self.debuggee.let_go(
            Which::Named {
                threads: vec![thread],
            },
            self.reporting,
        );
        if let Err(error) = letting {
            return self.refused(path, from, error);
        }
        let (landed, ending) = self.what_happened(Wanted::Anything)?;
        Ok(self.then(path, from, Did::Continued { landed }, ending))
    }

    fn evaluated(
        &mut self,
        expression: &str,
        frame: u32,
        detail: Detail,
        path: &str,
        from: At,
    ) -> Result<Flow> {
        let result = match self.evaluate(expression, frame, detail) {
            Ok(result) => result,
            Err(error) => return self.refused(path, from, error),
        };
        // an expression that raised has not answered. the record carries the
        // exception, which is the answer to what it did, and the script stops
        let raised = match &result {
            Evaluated::Raised { error } => Some(Halted::Raised {
                expression: expression.to_string(),
                error: error.clone(),
            }),
            Evaluated::Value { .. } => None,
        };
        Ok(self.then(
            path,
            from,
            Did::Evaluated {
                expression: expression.to_string(),
                frame,
                result,
            },
            raised.map(Ending::Halted),
        ))
    }

    fn walked(&mut self, top: Option<u32>, path: &str, from: At) -> Result<Flow> {
        let stop = self.cursor.stop;
        let walked = match self.debuggee.walk_stack(stop, top, self.reporting) {
            Ok(walked) => walked,
            Err(error) => return self.refused(path, from, error),
        };
        Ok(self.then(
            path,
            from,
            Did::Walked {
                frames: walked.frames,
                depth: walked.depth,
                mode: walked.mode,
            },
            None,
        ))
    }

    // ---- run_to ----------------------------------------------------------

    fn ran_to(
        &mut self,
        file: &Path,
        line: u32,
        condition: Option<&str>,
        hits: Option<HitCondition>,
        path: &str,
        from: At,
    ) -> Result<Flow> {
        let mut set = self.debuggee.armed.clone();
        set.push(SourceBreakpoint {
            id: self.own,
            file: file.to_path_buf(),
            line,
            condition: condition.map(ToString::to_string),
            hits,
            log: None,
        });

        let resolved = match self.debuggee.resolve_breakpoints(set, self.reporting) {
            Ok(resolved) => resolved,
            Err(error) => return self.refused(path, from, error),
        };
        let binding = resolved
            .into_iter()
            .find(|entry| entry.id == self.own)
            .expect("the set that was sent held the script's own breakpoint")
            .binding;

        // a breakpoint that did not bind will never be reached, so running to
        // it would spend the whole wall clock budget arriving nowhere. the
        // reason says what to do about it — an unbound breakpoint binds later
        // if the module it names is imported, which is exactly why the set has
        // to go back to what the client asked for before this returns
        if let Binding::Unbound { reason } = &binding {
            let disarmed = if self.put_the_set_back()? {
                Disarmed::NothingArmed
            } else {
                self.still_armed(file, line, Vec::new())
            };
            return Ok(self.then(
                path,
                from,
                Did::RanTo {
                    file: file.to_path_buf(),
                    line,
                    armed_as: self.own,
                    binding: binding.clone(),
                    landed: None,
                    disarmed,
                },
                Some(Ending::Halted(Halted::Unbound {
                    reason: reason.clone(),
                })),
            ));
        }

        let thread = self.cursor.thread;
        let letting = self.debuggee.let_go(
            Which::Named {
                threads: vec![thread],
            },
            self.reporting,
        );
        if let Err(error) = letting {
            return self.refused(path, from, error);
        }

        let (landed, ending) = self.what_happened(Wanted::Breakpoint(self.own))?;
        let disarmed = self.disarm(&landed, file, line)?;
        Ok(self.then(
            path,
            from,
            Did::RanTo {
                file: file.to_path_buf(),
                line,
                armed_as: self.own,
                binding,
                landed: Some(landed),
                disarmed,
            },
            ending,
        ))
    }

    /// take the script's own breakpoint back off, whatever the program is doing
    ///
    /// the one case it cannot be taken off is a program that is still running,
    /// because the agent binds breakpoints on a python thread it is holding and
    /// there is none. a pause is what turns one into something that can be
    /// asked, and it is armed here rather than left to the client — a script
    /// that ended mid-`run_to` must not leave the program armed with something
    /// nobody asked for
    fn disarm(&mut self, landed: &Landed, file: &Path, line: u32) -> Result<Disarmed> {
        if matches!(landed, Landed::Exited { .. }) {
            return Ok(Disarmed::ProgramEnded);
        }
        if !self.debuggee.held.is_empty() {
            return Ok(if self.put_the_set_back()? {
                Disarmed::Removed
            } else {
                self.still_armed(file, line, Vec::new())
            });
        }

        let running = match self.debuggee.arm_pause(self.reporting) {
            Ok(running) => running,
            // nothing can be asked of a program with nothing held, and a pause
            // that could not even be armed leaves the breakpoint exactly where
            // it is. that is what has to be said
            Err(Error::Session(_)) => Vec::new(),
            Err(other) => return Err(other),
        };
        // the same bound the script itself had: a program that reaches no line
        // in that long is one no pause reaches in it either
        let waited = self
            .debuggee
            .wait_for(Some(self.budget.wall()), self.reporting)?;
        self.keep_rebindings(&waited);
        match waited {
            Running::Stopped { stop, .. } => {
                let at = At::of(&stop);
                Ok(if self.put_the_set_back()? {
                    Disarmed::PausedToRemove { at }
                } else {
                    self.still_armed(file, line, running)
                })
            }
            _ => Ok(self.still_armed(file, line, running)),
        }
    }

    /// set the breakpoints back to what the client last asked for
    ///
    /// the resolutions it answers with are not carried into the transcript:
    /// this set is the client's own, it was resolved when the client asked for
    /// it, and anything that *changed* about it while the program ran arrived
    /// as a rebinding on the wait that saw it
    ///
    /// `false` means the session would not take it, which is only possible with
    /// nothing held — and then the script's own breakpoint is still in the set
    fn put_the_set_back(&mut self) -> Result<bool> {
        let set = self.debuggee.armed.clone();
        match self.debuggee.resolve_breakpoints(set, self.reporting) {
            Ok(resolved) => {
                debug_assert!(
                    resolved.len() == self.debuggee.armed.len(),
                    "the set that went back is the one the client asked for"
                );
                Ok(true)
            }
            Err(Error::Session(_)) => Ok(false),
            Err(other) => Err(other),
        }
    }

    fn still_armed(&self, file: &Path, line: u32, running: Vec<u64>) -> Disarmed {
        Disarmed::StillArmed {
            file: file.to_path_buf(),
            line,
            id: self.own,
            running,
        }
    }

    // ---- control flow ----------------------------------------------------

    fn branch(
        &mut self,
        predicate: &Predicate,
        then: &[Step],
        otherwise: &[Step],
        path: &str,
    ) -> Result<Flow> {
        if let Some(exhausted) = self.charge(path) {
            return Ok(Flow::Over(exhausted));
        }
        let from = At::of(&self.cursor);
        let answered = match self.decide(predicate) {
            Ok(answered) => answered,
            Err(error) => return self.refused(path, from, error),
        };
        let ending = ending_of(&answered, &predicate.expression);
        let flow = self.then(
            path,
            from,
            Did::Branched {
                expression: predicate.expression.clone(),
                frame: predicate.frame,
                answered: answered.clone(),
            },
            ending,
        );
        match flow {
            Flow::Over(outcome) => Ok(Flow::Over(outcome)),
            Flow::Carry => {
                let Answered::Value { value } = answered else {
                    unreachable!("a predicate that did not answer ended the script above")
                };
                let (taken, label) = if value {
                    (then, "then")
                } else {
                    (otherwise, "otherwise")
                };
                self.block(taken, &format!("{path}.{label}"))
            }
        }
    }

    fn repeat(
        &mut self,
        predicate: &Predicate,
        limit: NonZeroU32,
        body: &[Step],
        path: &str,
    ) -> Result<Flow> {
        for pass in 1..=limit.get() {
            if let Some(exhausted) = self.charge(path) {
                return Ok(Flow::Over(exhausted));
            }
            let from = At::of(&self.cursor);
            let answered = match self.decide(predicate) {
                Ok(answered) => answered,
                Err(error) => return self.refused(path, from, error),
            };
            let ending = ending_of(&answered, &predicate.expression);
            let flow = self.then(
                path,
                from,
                Did::Tested {
                    expression: predicate.expression.clone(),
                    frame: predicate.frame,
                    pass,
                    answered: answered.clone(),
                },
                ending,
            );
            match flow {
                Flow::Over(outcome) => return Ok(Flow::Over(outcome)),
                Flow::Carry => {}
            }

            let Answered::Value { value } = answered else {
                unreachable!("a predicate that did not answer ended the script above")
            };
            if !value {
                return Ok(Flow::Carry);
            }
            match self.block(body, &format!("{path}.body"))? {
                Flow::Carry => {}
                over @ Flow::Over(_) => return Ok(over),
            }
        }

        // the allowance is spent and the predicate was still true on the last
        // pass, so the loop did not finish what it was for. running the steps
        // after it would run them somewhere they did not expect
        if let Some(exhausted) = self.charge(path) {
            return Ok(Flow::Over(exhausted));
        }
        let from = At::of(&self.cursor);
        Ok(self.then(
            path,
            from,
            Did::Bounded { limit },
            Some(Ending::Halted(Halted::Bounded { limit })),
        ))
    }

    /// what a predicate answered, which has to be a `bool`
    ///
    /// the interpreter decides, through an operator the script wrote down. bpd
    /// re-deriving cpython's truthiness in rust would be a second
    /// implementation of a rule cpython owns, and wrapping the expression in
    /// `bool(...)` would be running whatever the program has bound that name to
    fn decide(&mut self, predicate: &Predicate) -> Result<Answered> {
        let evaluated = self.evaluate(
            &predicate.expression,
            predicate.frame,
            Detail {
                // a predicate is a `bool` or it is a failure, so nothing about the
                // shape of a value is wanted here. the read never opens anything
                depth: 0,
                ..Detail::default()
            },
        )?;
        Ok(match evaluated {
            Evaluated::Raised { error } => Answered::Raised { error },
            Evaluated::Value { value } => match value.content {
                Content::Bool { value } => Answered::Value { value },
                _ => Answered::NotABool { kind: value.kind },
            },
        })
    }

    fn evaluate(&mut self, expression: &str, frame: u32, detail: Detail) -> Result<Evaluated> {
        let frame = FrameId {
            stop: self.cursor.stop,
            depth: frame,
        };
        self.debuggee
            .evaluate_in(frame, expression, detail, self.reporting)
    }

    // ---- letting the thread go -------------------------------------------

    /// wait for what the script's thread does, under the script's own clock
    ///
    /// the wall clock budget is the deadline. a script that is waiting for a
    /// program which never stops is spending exactly that, and a second
    /// deadline per step would be a second place to say the same thing
    fn what_happened(&mut self, wanted: Wanted) -> Result<(Landed, Option<Ending>)> {
        let left = self.until.saturating_duration_since(Instant::now());
        let ran = self.debuggee.wait_for(Some(left), self.reporting)?;
        self.keep_rebindings(&ran);

        Ok(match ran {
            Running::Stopped { stop, .. } => {
                let to = At::of(&stop);
                if stop.thread == self.cursor.thread {
                    // the thread really is there, whether or not it is where the
                    // step was going, so the cursor follows it either way
                    self.cursor = stop;
                    if wanted.matches(&to.why) {
                        (Landed::Stopped { to }, None)
                    } else {
                        (
                            Landed::Elsewhere { to: to.clone() },
                            Some(Ending::Halted(Halted::Elsewhere { to })),
                        )
                    }
                } else {
                    let expected = self.cursor.thread;
                    (
                        Landed::OtherThread {
                            to: to.clone(),
                            expected,
                        },
                        Some(Ending::Halted(Halted::OtherThread { to, expected })),
                    )
                }
            }
            Running::Exited { status, .. } => {
                let code = exit_code(status);
                (
                    Landed::Exited { exit_code: code },
                    Some(Ending::Halted(Halted::Exited { exit_code: code })),
                )
            }
            Running::Finishing { threads, .. } => (
                Landed::Finishing {
                    threads: threads.clone(),
                },
                Some(Ending::Halted(Halted::Finishing { threads })),
            ),
            // not a stop, and it carries no location. the wall clock budget is
            // what ran out, so the transcript is partial rather than halted
            Running::StillRunning { .. } => (Landed::StillRunning, Some(Ending::OutOfTime)),
        })
    }

    /// keep what loading a file changed about the breakpoint set
    fn keep_rebindings(&mut self, ran: &Running) {
        let rebound = match ran {
            Running::Stopped { rebound, .. }
            | Running::Exited { rebound, .. }
            | Running::Finishing { rebound, .. }
            | Running::StillRunning { rebound, .. } => rebound,
        };
        self.rebound.extend(rebound.iter().cloned());
    }

    // ---- the budget, and writing a record down ---------------------------

    /// spend one step of the budget, or say which bound ran out
    fn charge(&mut self, path: &str) -> Option<Outcome> {
        if self.spent >= self.budget.steps.get() {
            return Some(self.exhausted(
                path,
                Bound::Steps {
                    limit: self.budget.steps,
                },
            ));
        }
        if Instant::now() >= self.until {
            return Some(self.exhausted(
                path,
                Bound::Wall {
                    limit_ms: self.budget.wall_ms,
                },
            ));
        }
        self.spent += 1;
        None
    }

    /// write down what a step did, and say whether the script goes on
    ///
    /// a step's own ending is reported ahead of the byte budget: it is why the
    /// rest did not run, and what was recorded is on the transcript either way
    fn then(&mut self, path: &str, at: At, did: Did, ending: Option<Ending>) -> Flow {
        let over_budget = self.record(path, at, did);
        match ending {
            Some(Ending::Halted(why)) => Flow::Over(Outcome::Halted {
                at: path.to_string(),
                why,
            }),
            Some(Ending::OutOfTime) => Flow::Over(self.exhausted(
                path,
                Bound::Wall {
                    limit_ms: self.budget.wall_ms,
                },
            )),
            None => match over_budget {
                Some(outcome) => Flow::Over(outcome),
                None => Flow::Carry,
            },
        }
    }

    /// a step the session would not answer, which is the end of the script
    fn refused(&mut self, path: &str, at: At, error: Error) -> Result<Flow> {
        // only a failure that describes the **program** is a record. one that
        // describes bpd's own machinery — a socket, a process — is not
        // something a transcript can carry as a step's outcome
        let Error::Session(refusal) = error else {
            return Err(error);
        };
        let reason = refusal.to_string();
        Ok(self.then(
            path,
            at,
            Did::Refused {
                doing: self.doing.to_string(),
                reason: reason.clone(),
            },
            Some(Ending::Halted(Halted::Refused { reason })),
        ))
    }

    fn record(&mut self, path: &str, at: At, did: Did) -> Option<Outcome> {
        let record = Record {
            step: path.to_string(),
            at,
            did,
        };
        let size = serde_json::to_vec(&record)
            .expect("a record is built from types whose serde is derived and cannot fail")
            .len();
        self.bytes += size as u64;
        self.records.push(record);

        if self.bytes > u64::from(self.budget.bytes.get()) {
            return Some(self.exhausted(
                path,
                Bound::Bytes {
                    limit: self.budget.bytes,
                    recorded: self.bytes,
                },
            ));
        }
        None
    }

    fn exhausted(&self, path: &str, bound: Bound) -> Outcome {
        Outcome::Exhausted {
            at: path.to_string(),
            bound,
            made: u32::try_from(self.records.len())
                .expect("one record costs one step, and the step budget is a u32"),
        }
    }
}

/// what a predicate's answer does to the script
///
/// anything but a `bool` ends it. truth-testing an arbitrary object means
/// running the program's own `__bool__` or `__len__` and branching on the
/// result, which is the program deciding rather than the script
fn ending_of(answered: &Answered, expression: &str) -> Option<Ending> {
    match answered {
        Answered::Value { .. } => None,
        Answered::Raised { error } => Some(Ending::Halted(Halted::Raised {
            expression: expression.to_string(),
            error: error.clone(),
        })),
        Answered::NotABool { kind } => Some(Ending::Halted(Halted::NotABool {
            expression: expression.to_string(),
            kind: kind.clone(),
        })),
    }
}

/// where a step sits in the submitted tree
///
/// counting from one, with a branch named on the way in — `3`, `3.then.1`,
/// `4.body.2`. it is how a record says which step of the script it came from
/// without the reader counting anything
fn position(prefix: &str, index: usize) -> String {
    let place = index + 1;
    if prefix.is_empty() {
        place.to_string()
    } else {
        format!("{prefix}.{place}")
    }
}
