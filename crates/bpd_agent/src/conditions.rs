//! conditions, hit counts and log messages, all answered inside the debuggee
//!
//! everything here runs on the `LINE` event path, after a line has already
//! matched a bound breakpoint. that ordering is the whole design: deciding
//! whether a line is interesting costs a lookup on a code object's address and
//! an integer, and only once the answer is yes does anything reach for a frame
//!
//! ## what runs when
//!
//! a hit is processed in one order, for every breakpoint, with no exceptions:
//!
//! 1. the condition, if there is one. false means nothing else happens
//! 1. the hit counter, which only counts hits the condition let through
//! 1. the log message, if there is one — and then the program keeps running
//! 1. otherwise, the stop
//!
//! ## re-entrancy
//!
//! evaluating a condition runs arbitrary user python **inside a `LINE`
//! callback**. that code raises its own `PY_START` and `LINE` events, so a
//! condition that calls a function containing a breakpoint would otherwise stop
//! inside itself — or recurse without end, if the function it calls is the one
//! it is attached to
//!
//! so a breakpoint reached while this thread is evaluating does not fire, does
//! not count, and is not disabled. the alternative is a stop whose stack is
//! half debugger, which is the thing the stack rules exist to prevent
//!
//! the flag is per thread. another thread reaching the same breakpoint at the
//! same moment is a real hit and is treated as one

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use bpd_core::{
    Evaluation, HitCondition, LogRecord, Part, PythonError, SourceBreakpoint, TracebackFrame,
    Unbound,
};
use pyo3::basic::CompareOp;
use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyString};

use crate::events;

thread_local! {
    /// whether this thread is inside a condition or a log message
    static EVALUATING: Cell<bool> = const { Cell::new(false) };
}

/// whether the `LINE` event that is being reported came from an expression of
/// ours rather than from the program
pub(crate) fn evaluating() -> bool {
    EVALUATING.with(Cell::get)
}

/// suppresses this thread's breakpoints until it is dropped
#[derive(Debug)]
pub(crate) struct Suppressed(());

/// stop this thread's breakpoints from firing while user python of ours runs
pub(crate) fn suppress() -> Suppressed {
    debug_assert!(
        !evaluating(),
        "a hit is only processed when the thread is not already evaluating, \
         because `on_line` returns before it gets here"
    );
    EVALUATING.with(|flag| flag.set(true));
    Suppressed(())
}

impl Drop for Suppressed {
    fn drop(&mut self) {
        EVALUATING.with(|flag| flag.set(false));
    }
}

/// what one breakpoint decided about one hit
#[derive(Debug)]
pub(crate) enum Fired {
    /// the condition was false, or the hit count has not come round
    Nothing,
    /// a log record was produced, and the program keeps running
    Logged(LogRecord),
    /// the program should stop here
    Stop,
    /// an expression raised, and there is no answer to report
    Failed(Raised),
}

/// an expression of a breakpoint's that raised instead of answering
#[derive(Debug)]
pub(crate) struct Raised {
    /// whether it was the condition or the log message
    pub(crate) part: Part,
    /// the expression as the client wrote it
    pub(crate) expression: String,
    /// what the interpreter raised
    pub(crate) error: PythonError,
}

/// the namespaces of the frame that reached the line, fetched at most once
///
/// a hit whose breakpoints are all unconditional never touches a frame at all,
/// which is what keeps the plain breakpoint as cheap as it was before
/// conditions existed
pub(crate) struct Place<'py> {
    python: Python<'py>,
    namespaces: Option<(Bound<'py, PyAny>, Bound<'py, PyAny>)>,
}

impl<'py> Place<'py> {
    /// no frame yet
    pub(crate) const fn unfetched(python: Python<'py>) -> Self {
        Self {
            python,
            namespaces: None,
        }
    }

    /// the frame's globals and locals
    ///
    /// `f_locals` is PEP 667's write-through proxy on every interpreter `bpd`
    /// supports, so it is the frame's own state rather than a snapshot of it
    fn namespaces(&mut self) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        if self.namespaces.is_none() {
            let frame = events::current_frame(self.python)?;
            let globals = frame.getattr("f_globals")?;
            let locals = frame.getattr("f_locals")?;
            self.namespaces = Some((globals, locals));
        }
        let (globals, locals) = self
            .namespaces
            .as_ref()
            .expect("the branch above filled it in");
        Ok((globals.clone(), locals.clone()))
    }
}

/// everything one breakpoint does when its line is reached, worked out once
#[derive(Debug)]
pub(crate) struct Plan {
    /// the client's id for the breakpoint
    pub(crate) id: u32,
    condition: Option<Condition>,
    hits: Option<HitCondition>,
    log: Option<Template>,
    /// how many hits have qualified, across every thread
    count: AtomicU64,
}

impl Plan {
    /// compile a breakpoint's expressions, or say why it cannot be set
    ///
    /// this happens while the debuggee is stopped, once per request — never on
    /// the event path, and never inside a monitoring callback that is running
    /// while the program is
    pub(crate) fn compile(python: Python<'_>, request: &SourceBreakpoint) -> Result<Self, Unbound> {
        let condition = request
            .condition
            .as_deref()
            .map(|source| Condition::compile(python, request.id, source))
            .transpose()?;
        let log = request
            .log
            .as_deref()
            .map(|template| Template::compile(python, request.id, template))
            .transpose()?;

        Ok(Self {
            id: request.id,
            condition,
            hits: request.hits,
            log,
            count: AtomicU64::new(0),
        })
    }

    /// how the condition will be answered, for the client to see
    pub(crate) fn evaluation(&self) -> Evaluation {
        match &self.condition {
            None => Evaluation::Always,
            Some(condition) if condition.comparison.is_some() => Evaluation::Comparison,
            Some(_) => Evaluation::Expression,
        }
    }

    /// decide what this breakpoint does about one hit
    pub(crate) fn fire(
        &self,
        python: Python<'_>,
        place: &mut Place<'_>,
        at: &Location<'_>,
    ) -> PyResult<Fired> {
        if let Some(condition) = &self.condition {
            let (globals, locals) = place.namespaces()?;
            match condition.holds(python, &globals, &locals) {
                Ok(true) => {}
                Ok(false) => return Ok(Fired::Nothing),
                Err(error) => {
                    return Ok(Fired::Failed(Raised {
                        part: Part::Condition,
                        expression: condition.source.clone(),
                        error: capture(python, &error),
                    }));
                }
            }
        }

        // relaxed because the count is the only thing being ordered: every
        // increment is still atomic and every hit still gets a distinct number,
        // which is all "the nth hit" means
        let hit = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if !reached(self.hits, hit) {
            return Ok(Fired::Nothing);
        }

        let Some(template) = &self.log else {
            return Ok(Fired::Stop);
        };
        let (globals, locals) = place.namespaces()?;
        match template.render(python, &globals, &locals) {
            Ok(message) => Ok(Fired::Logged(LogRecord {
                breakpoint: self.id,
                file: at.file.to_string(),
                line: at.line,
                thread: at.thread,
                hit,
                message,
            })),
            Err((expression, error)) => Ok(Fired::Failed(Raised {
                part: Part::LogMessage,
                expression,
                error: capture(python, &error),
            })),
        }
    }
}

/// where a hit happened, which every log record and every stop names
#[derive(Debug)]
pub(crate) struct Location<'a> {
    /// the `co_filename` of the code object that was running
    pub(crate) file: &'a str,
    /// the line it was on
    pub(crate) line: u32,
    /// the interpreter's identity for the thread that reached it
    pub(crate) thread: u64,
}

/// whether the nth qualifying hit is one this breakpoint acts on
fn reached(hits: Option<HitCondition>, hit: u64) -> bool {
    match hits {
        None => true,
        Some(HitCondition::Exactly { count }) => hit == u64::from(count.get()),
        Some(HitCondition::AtLeast { count }) => hit >= u64::from(count.get()),
        Some(HitCondition::Every { count }) => hit.is_multiple_of(u64::from(count.get())),
    }
}

/// a compiled condition, and the native comparison it reduces to when it can
#[derive(Debug)]
struct Condition {
    source: String,
    code: Py<PyAny>,
    comparison: Option<Comparison>,
}

impl Condition {
    fn compile(python: Python<'_>, id: u32, source: &str) -> Result<Self, Unbound> {
        // compiled even when the native comparison will answer it, because the
        // interpreter is the authority on whether an expression is one — and
        // because a name that turns out not to be a local of the frame is
        // resolved by evaluating this
        let code = events::compile_expression(
            python,
            source,
            &format!("<bpd condition of breakpoint {id}>"),
        )
        .map_err(|error| Unbound::ConditionInvalid {
            condition: source.to_string(),
            error: capture(python, &error),
        })?;

        Ok(Self {
            source: source.to_string(),
            code,
            comparison: Comparison::parse(python, source),
        })
    }

    fn holds(
        &self,
        python: Python<'_>,
        globals: &Bound<'_, PyAny>,
        locals: &Bound<'_, PyAny>,
    ) -> PyResult<bool> {
        if let Some(comparison) = &self.comparison
            && let Some(answer) = comparison.compare(python, locals)?
        {
            return Ok(answer);
        }
        events::evaluate(python, &self.code, globals, locals)?.is_truthy()
    }
}

/// `name <op> literal`, read straight out of the frame's fast locals
///
/// this is the shape almost every breakpoint condition really has, and
/// answering it without building an evaluation frame is the difference between
/// a conditional breakpoint on a hot line being usable and being a reason to
/// take the breakpoint off
#[derive(Debug)]
struct Comparison {
    name: Py<PyString>,
    op: Op,
    value: Py<PyAny>,
}

/// the comparisons the native path can make
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
}

impl Op {
    /// the interpreter's own comparison, for everything but identity
    fn rich(self) -> CompareOp {
        match self {
            Self::Eq => CompareOp::Eq,
            Self::Ne => CompareOp::Ne,
            Self::Lt => CompareOp::Lt,
            Self::Le => CompareOp::Le,
            Self::Gt => CompareOp::Gt,
            Self::Ge => CompareOp::Ge,
            Self::Is | Self::IsNot => {
                unreachable!("identity is answered by pointer comparison and never reaches here")
            }
        }
    }
}

impl Comparison {
    /// read `name <op> literal` out of an expression, or decline it
    ///
    /// declining is the common answer and costs nothing: the expression was
    /// compiled anyway, and the interpreter evaluates it
    fn parse(python: Python<'_>, source: &str) -> Option<Self> {
        let (name, op, literal) = split(source.trim())?;
        let value = constant(python, op, literal)?;
        Some(Self {
            name: PyString::new(python, name).unbind(),
            op,
            value,
        })
    }

    /// the answer, or `None` when `name` is not a local of this frame
    ///
    /// a name that is not a local is a global, a builtin, or nothing at all,
    /// and working out which is `LOAD_NAME`'s job. reimplementing that is how a
    /// debugger reads a variable from the wrong scope, so the interpreter is
    /// handed the expression instead
    fn compare(&self, python: Python<'_>, locals: &Bound<'_, PyAny>) -> PyResult<Option<bool>> {
        let value = match locals.get_item(self.name.bind(python)) {
            Ok(value) => value,
            Err(error) if error.is_instance_of::<PyKeyError>(python) => return Ok(None),
            Err(error) => return Err(error),
        };

        let constant = self.value.bind(python);
        let answer = match self.op {
            Op::Is => value.is(constant),
            Op::IsNot => !value.is(constant),
            op => value.rich_compare(constant, op.rich())?.is_truthy()?,
        };
        Ok(Some(answer))
    }
}

/// split an expression into a name, an operator and the text of a literal
fn split(source: &str) -> Option<(&str, Op, &str)> {
    let name = identifier(source)?;
    let rest = source[name.len()..].trim_start();

    // `is` and `is not` are words. the identifier above stopped at a character
    // that cannot be part of a name, and `i` can be, so whitespace between the
    // two is already guaranteed
    if let Some(tail) = rest.strip_prefix("is") {
        if !tail.starts_with(char::is_whitespace) {
            return None;
        }
        let tail = tail.trim_start();
        return match tail.strip_prefix("not") {
            Some(negated) if negated.starts_with(char::is_whitespace) => {
                Some((name, Op::IsNot, negated.trim_start()))
            }
            Some(_) => None,
            None => Some((name, Op::Is, tail)),
        };
    }

    // longest first, so `<=` is never read as `<` followed by junk
    for (text, op) in [
        ("==", Op::Eq),
        ("!=", Op::Ne),
        ("<=", Op::Le),
        (">=", Op::Ge),
        ("<", Op::Lt),
        (">", Op::Gt),
    ] {
        if let Some(tail) = rest.strip_prefix(text) {
            return Some((name, op, tail.trim_start()));
        }
    }
    None
}

/// the leading identifier of an expression, if it starts with one
///
/// ascii only, and never a keyword. a unicode name is a name the native path
/// declines, which costs nothing but an interpreter evaluation
fn identifier(source: &str) -> Option<&str> {
    let end = source
        .find(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .unwrap_or(source.len());
    let name = &source[..end];

    let first = name.chars().next()?;
    if first.is_ascii_digit() {
        return None;
    }
    // `None == x` is not a name being read from a frame, and neither is any
    // other keyword. the interpreter can have all of them
    if matches!(
        name,
        "None"
            | "True"
            | "False"
            | "and"
            | "or"
            | "not"
            | "in"
            | "is"
            | "if"
            | "else"
            | "lambda"
            | "await"
    ) {
        return None;
    }
    Some(name)
}

/// build the literal on the right of the comparison, or decline it
///
/// only the literals whose value rust can produce **exactly** what python would
/// are accepted. a float is declined because parsing one is a second
/// implementation of python's own parser, and a debugger whose fast path
/// rounds differently from the interpreter is worse than one with no fast path
fn constant(python: Python<'_>, op: Op, literal: &str) -> Option<Py<PyAny>> {
    let literal = literal.trim_end();

    let singleton = match literal {
        "None" => Some(python.None()),
        "True" => Some(PyBool::new(python, true).to_owned().into_any().unbind()),
        "False" => Some(PyBool::new(python, false).to_owned().into_any().unbind()),
        _ => None,
    };
    if let Some(singleton) = singleton {
        return Some(singleton);
    }

    // identity is only knowable for the three singletons. `value is 1000`
    // compares against whichever object the interpreter put in `co_consts`,
    // which is not the one this would build, so the two paths could disagree —
    // and a fast path that disagrees is a bug, not a trade-off
    if matches!(op, Op::Is | Op::IsNot) {
        return None;
    }

    if let Some(text) = quoted(literal) {
        return Some(PyString::new(python, text).into_any().unbind());
    }

    // no underscores and nothing that does not fit an i64: python takes both
    // and rust does not, so both decline rather than differ
    let integer: i64 = literal.parse().ok()?;
    Some(
        integer
            .into_pyobject(python)
            .expect("an i64 is always a python int")
            .into_any()
            .unbind(),
    )
}

/// the contents of a plain string literal, or nothing
///
/// a literal holding a backslash or its own quote character needs python's
/// escape rules to read, and guessing at them would put a different string on
/// the two paths
fn quoted(literal: &str) -> Option<&str> {
    for quote in ['\'', '"'] {
        if let Some(rest) = literal.strip_prefix(quote)
            && let Some(text) = rest.strip_suffix(quote)
            && !text.contains('\\')
            && !text.contains(quote)
        {
            return Some(text);
        }
    }
    None
}

/// a log message, split into the text and the expressions between the braces
#[derive(Debug)]
struct Template {
    segments: Vec<Segment>,
}

/// one piece of a log message
#[derive(Debug)]
enum Segment {
    /// text that is emitted as it is written
    Text(String),
    /// an expression evaluated in the frame and converted with `str()`
    Expression {
        /// the expression as the client wrote it
        source: String,
        /// compiled once, when the breakpoint was set
        code: Py<PyAny>,
    },
}

impl Template {
    fn compile(python: Python<'_>, id: u32, template: &str) -> Result<Self, Unbound> {
        let raw = parse_template(template).map_err(|reason| Unbound::LogMessageInvalid {
            log: template.to_string(),
            expression: None,
            reason,
        })?;

        let mut segments = Vec::with_capacity(raw.len());
        for piece in raw {
            match piece {
                Raw::Text(text) => segments.push(Segment::Text(text)),
                Raw::Expression(source) => {
                    let code = events::compile_expression(
                        python,
                        &source,
                        &format!("<bpd log message of breakpoint {id}>"),
                    )
                    .map_err(|error| Unbound::LogMessageInvalid {
                        log: template.to_string(),
                        expression: Some(source.clone()),
                        reason: format!("does not compile: {}", capture(python, &error)),
                    })?;
                    segments.push(Segment::Expression { source, code });
                }
            }
        }
        Ok(Self { segments })
    }

    /// the message, or the expression that raised and what it raised
    fn render(
        &self,
        python: Python<'_>,
        globals: &Bound<'_, PyAny>,
        locals: &Bound<'_, PyAny>,
    ) -> Result<String, (String, PyErr)> {
        let mut message = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => message.push_str(text),
                Segment::Expression { source, code } => {
                    let rendered = events::evaluate(python, code, globals, locals)
                        .and_then(|value| value.str()?.extract::<String>())
                        .map_err(|error| (source.clone(), error))?;
                    message.push_str(&rendered);
                }
            }
        }
        Ok(message)
    }
}

/// a piece of a log message before anything has been compiled
#[derive(Debug, PartialEq, Eq)]
enum Raw {
    Text(String),
    Expression(String),
}

/// split a log message on its braces
///
/// `{{` and `}}` are a literal brace, and everything between a `{` and the next
/// `}` is an expression. a brace that does not pair up is refused rather than
/// emitted as itself: a log message that silently drops the value the user
/// asked for is a log message that lies about the program
fn parse_template(template: &str) -> Result<Vec<Raw>, String> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut characters = template.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                text.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                text.push('}');
            }
            '}' => {
                return Err(
                    "there is a `}` with no `{` before it. write `}}` for a literal brace"
                        .to_string(),
                );
            }
            '{' => {
                let mut expression = String::new();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some(character) => expression.push(character),
                        None => {
                            return Err("there is a `{` that is never closed".to_string());
                        }
                    }
                }
                if expression.trim().is_empty() {
                    return Err("there is an empty `{}`, which is not an expression".to_string());
                }
                if !text.is_empty() {
                    segments.push(Raw::Text(std::mem::take(&mut text)));
                }
                segments.push(Raw::Expression(expression));
            }
            character => text.push(character),
        }
    }

    if !text.is_empty() {
        segments.push(Raw::Text(text));
    }
    Ok(segments)
}

/// read an exception off the object, without importing anything
///
/// `traceback.format_exception` would be the obvious way and it is the wrong
/// one twice over: importing a module from inside a monitoring callback is what
/// corrupted line numbers once already, and a debuggee that has imported
/// `traceback` when it otherwise would not have is a debuggee the debugger
/// changed
pub(crate) fn capture(python: Python<'_>, error: &PyErr) -> PythonError {
    let value = error.value(python);
    let kind = value.get_type().fully_qualified_name().map_or_else(
        |_| "<an exception whose type has no name>".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );

    // `__str__` is user code and can raise. saying so is a report; letting it
    // escape here would replace the exception being described with a different
    // one
    let message = value
        .str()
        .and_then(|text| text.extract::<String>())
        .unwrap_or_else(|failure| format!("<the exception's __str__ raised {failure}>"));

    PythonError {
        kind,
        message,
        traceback: frames(python, error),
    }
}

/// the frames a traceback carries, outermost first
fn frames(python: Python<'_>, error: &PyErr) -> Vec<TracebackFrame> {
    let mut frames = Vec::new();
    let mut current = error.traceback(python).map(Bound::into_any);

    while let Some(entry) = current {
        let frame = entry
            .getattr("tb_frame")
            .expect("a traceback entry has `tb_frame`");
        let code = frame.getattr("f_code").expect("a frame has `f_code`");
        frames.push(TracebackFrame {
            file: code
                .getattr("co_filename")
                .and_then(|name| name.extract())
                .expect("a code object's `co_filename` is a string"),
            line: entry
                .getattr("tb_lineno")
                .and_then(|line| line.extract())
                .expect("a traceback entry's `tb_lineno` is an integer"),
            function: code
                .getattr("co_qualname")
                .and_then(|name| name.extract())
                .expect("a code object's `co_qualname` is a string"),
        });

        let next = entry
            .getattr("tb_next")
            .expect("a traceback entry has `tb_next`");
        current = (!next.is_none()).then_some(next);
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_message_splits_on_its_braces() {
        assert_eq!(
            parse_template("value is {value}!").expect("that template is well formed"),
            [
                Raw::Text("value is ".to_string()),
                Raw::Expression("value".to_string()),
                Raw::Text("!".to_string()),
            ]
        );
        assert_eq!(
            parse_template("{{literal}} braces").expect("that template is well formed"),
            [Raw::Text("{literal} braces".to_string())]
        );
    }

    #[test]
    fn a_brace_that_does_not_pair_up_is_refused_with_the_reason() {
        for (template, expected) in [
            ("count is {", "never closed"),
            ("count is }", "no `{` before it"),
            ("count is {}", "empty"),
        ] {
            let reason = parse_template(template).expect_err("that template is malformed");
            assert!(
                reason.contains(expected),
                "expected {expected:?} in {reason:?}"
            );
        }
    }

    #[test]
    fn the_shape_the_native_comparison_reads_is_exactly_name_op_literal() {
        assert_eq!(split("value == 3"), Some(("value", Op::Eq, "3")));
        assert_eq!(split("value==3"), Some(("value", Op::Eq, "3")));
        assert_eq!(split("value <= 3"), Some(("value", Op::Le, "3")));
        assert_eq!(split("value is None"), Some(("value", Op::Is, "None")));
        assert_eq!(
            split("value is not None"),
            Some(("value", Op::IsNot, "None"))
        );

        // anything that is not that shape is the interpreter's
        assert_eq!(split("(value == 3)"), None);
        assert_eq!(split("value.attribute == 3"), None);
        assert_eq!(split("3 == value"), None);
        assert_eq!(split("not value"), None);
        assert_eq!(split("island"), None);
    }
}
