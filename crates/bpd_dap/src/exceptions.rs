//! what a client has asked for of exceptions, through the two requests that ask
//!
//! DAP's own `setExceptionBreakpoints` carries the filters and
//! `exceptionOptions`, and an IDE's DAP layer owns it: the intellij platform
//! sends it itself, again on every change, with no field for a class's library
//! choice and none for anything a plugin wants to add. so a rule per class has
//! a second way in that no platform sends — `exceptionClasses` in the launch
//! configuration, and `bpd/setExceptionClasses` after it — and each request
//! replaces **only its own half**. a change of the filters never touches the
//! rules from the other request, and a change of the rules never touches the
//! filters
//!
//! the agent holds one table, so the two halves are sent as one: the rules from
//! `exceptionClasses` first, then each `exceptionOptions` class no rule names.
//! where both name a class, the rule decides, because it is the one that can
//! say what it does about a library's raise. everything else about how a rule
//! decides is the agent's — see [`bpd_core::ExceptionClass`]

use bpd_core::{ArmedClass, ClassFound, ExceptionClass};

/// both halves, as they were last asked for
#[derive(Debug, Clone, Default)]
pub struct Exceptions {
    /// stopping where an exception is raised, from `setExceptionBreakpoints`
    pub raised: bool,
    /// stopping where an exception leaves the program, from
    /// `setExceptionBreakpoints`
    pub uncaught: bool,
    /// the classes `exceptionOptions` named, with what each one's break mode
    /// asks
    pub options: Vec<ExceptionOption>,
    /// the rules `exceptionClasses` and `bpd/setExceptionClasses` set
    pub classes: Vec<ExceptionClass>,
    /// whether either request has asked for anything since the agent was last
    /// told, because there was no agent to tell
    pub held: bool,
}

/// one class an `exceptionOptions` entry named, and what its break mode asks
///
/// no library choice, because an option has nowhere to say one: it is the
/// session's `justMyCode`, decided when the option is sent as a rule, and one
/// asked for before the launch has no configuration yet to take it from
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionOption {
    /// the class, as the last segment of the option's path names it
    pub name: String,
    /// stopping where one is raised: `always`
    pub raised: bool,
    /// stopping where one leaves the program: `unhandled`
    pub uncaught: bool,
}

impl ExceptionOption {
    /// the rule this option is sent as, ignoring library files exactly when
    /// `just_my_code` is on
    pub fn rule(&self, just_my_code: bool) -> ExceptionClass {
        ExceptionClass {
            name: self.name.clone(),
            raised: self.raised,
            uncaught: self.uncaught,
            libraries: !just_my_code,
        }
    }
}

impl Exceptions {
    /// the one table the agent is sent: the rules, then each option no rule
    /// names, an option ignoring library files exactly when `just_my_code` is on
    pub fn merged(&self, just_my_code: bool) -> Vec<ExceptionClass> {
        let mut table = self.classes.clone();
        for option in &self.options {
            if !table.iter().any(|rule| rule.name == option.name) {
                table.push(option.rule(just_my_code));
            }
        }
        table
    }

    /// what each rule `exceptionClasses` or `bpd/setExceptionClasses` set is
    /// answered with, in the order they were asked
    pub fn class_verdicts(&self, armed: &[ArmedClass]) -> Vec<serde_json::Value> {
        self.classes
            .iter()
            .map(|asked| {
                verdict(
                    asked,
                    armed,
                    "another rule for the same class decides in this one's place: the \
                     first of them in the list",
                )
            })
            .collect()
    }

    /// what each `exceptionOptions` class is answered with, in the order they
    /// were asked, as the rule it became
    pub fn option_verdicts(
        &self,
        armed: &[ArmedClass],
        just_my_code: bool,
    ) -> Vec<serde_json::Value> {
        self.options
            .iter()
            .map(|option| {
                verdict(
                    &option.rule(just_my_code),
                    armed,
                    &format!(
                        "a rule `exceptionClasses` or `bpd/setExceptionClasses` set for `{}` \
                         decides for it in this option's place",
                        option.name
                    ),
                )
            })
            .collect()
    }
}

/// what one rule or option is answered with, read back off what the agent holds
///
/// a DAP `Breakpoint`'s three fields, which is what both requests answer each
/// entry with: `verified` when the agent holds exactly this rule **and** its
/// class is one the program has now. a rule whose class the program does not
/// have yet is held all the same and is `pending`, the word a breakpoint in a
/// module not imported yet gets — it decides for its class the moment one is
/// raised. anything that will not decide as asked is `failed`, with why
pub fn verdict(
    asked: &ExceptionClass,
    armed: &[ArmedClass],
    passed_over: &str,
) -> serde_json::Value {
    let Some(held) = armed.iter().find(|held| held.rule.name == asked.name) else {
        return failed(&format!(
            "the debuggee does not hold a rule for `{}`",
            asked.name
        ));
    };
    if held.rule != *asked {
        return failed(passed_over);
    }
    let name = &asked.name;
    match &held.found {
        ClassFound::Class => serde_json::json!({ "verified": true }),
        ClassFound::Unresolved { why } => serde_json::json!({
            "verified": false,
            "reason": "pending",
            "message": format!(
                "no class the program has now is `{name}`: {why}. the rule is held, and \
                 decides for a class that gives itself this name when one is raised"
            ),
        }),
        ClassFound::NotAnException { what } => failed(&format!(
            "`{name}` is {what}, so nothing the program raises is one. the rule is held, and \
             decides only for a class that gives itself this name"
        )),
        ClassFound::Renamed { named } => failed(&format!(
            "`{name}` is a class that gives itself the name `{named}`, and a rule decides for \
             a class by the name the class gives itself, so this one never decides for it. \
             name it `{named}`"
        )),
    }
}

/// an entry that will not decide as asked, and why
pub fn failed(message: &str) -> serde_json::Value {
    serde_json::json!({
        "verified": false,
        "reason": "failed",
        "message": message,
    })
}

/// what every entry is answered with while there is no program to hold it
pub fn waiting() -> serde_json::Value {
    serde_json::json!({
        "verified": false,
        "reason": "pending",
        "message": "no program has been started yet, so there is no interpreter to hold \
                    this rule. it is kept exactly as it was asked for and armed before the \
                    program runs its first line",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, raised: bool, libraries: bool) -> ExceptionClass {
        ExceptionClass {
            name: name.to_string(),
            raised,
            uncaught: false,
            libraries,
        }
    }

    #[test]
    fn a_rule_decides_in_place_of_an_option_for_the_same_class_and_the_rest_are_both() {
        let option = |name: &str| ExceptionOption {
            name: name.to_string(),
            raised: true,
            uncaught: false,
        };
        let exceptions = Exceptions {
            options: vec![option("KeyError"), option("OSError")],
            classes: vec![
                rule("KeyError", false, true),
                rule("ValueError", true, true),
            ],
            ..Exceptions::default()
        };
        assert_eq!(
            exceptions.merged(false),
            [
                rule("KeyError", false, true),
                rule("ValueError", true, true),
                rule("OSError", true, true),
            ],
            "the rules first, then the options no rule names, an option's library \
             choice the session's"
        );
        assert_eq!(exceptions.merged(true)[2], rule("OSError", true, false));
    }
}
