//! `bpd cache` — see what the agent cache is holding, and reclaim it
//!
//! staging keeps one copy of the agent per build and removes none, so the
//! directory only grows: 89 entries and 448 MB of them on the machine this was
//! written on. nothing removes them on its own, and that is a decision rather
//! than an omission — see [`bpd_engine::cache`] for why neither "what is still
//! needed" nor "what can be deleted" is answerable from inside one `bpd`
//!
//! so this is the whole of it: `bpd cache` says what is there, and
//! `bpd cache clear` takes it away. both exit non-zero when the answer is not
//! the whole answer — an entry that would not go, or something in the directory
//! that staging never wrote — because a cache command that printed "cleared"
//! over four of five entries would be worse than one that did nothing

use std::process::ExitCode;

use bpd_engine::cache::{Cache, Cleared, Current};

use crate::report_error;

/// `bpd cache` arguments
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// what to do with it, or nothing to be told what is in it
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// remove cached agent builds
    Clear(ClearArgs),
}

/// `bpd cache clear` arguments
#[derive(Debug, clap::Args)]
pub(crate) struct ClearArgs {
    /// leave the entries the agents this `bpd` carries are already in, so the
    /// next launch does not pay a cold load of one
    #[arg(long)]
    keep_current: bool,
}

pub(crate) fn run(args: &Args) -> ExitCode {
    let cache = match bpd_engine::cache::open() {
        Ok(cache) => cache,
        Err(error) => {
            report_error(&error);
            return ExitCode::FAILURE;
        }
    };

    match &args.command {
        None => report(&cache),
        Some(Command::Clear(clear)) => self::clear(&cache, clear),
    }
}

/// what is in there, and what clearing it would cost
fn report(cache: &Cache) -> ExitCode {
    field("cache", &cache.root().display().to_string());
    if cache.present() {
        field("entries", &cache.entries().len().to_string());
        field("size", &size(cache.size()));
    } else {
        // not a failure and not an error: it is what a machine that has never
        // launched a debuggee looks like
        note("it is not there — nothing has been staged yet, and it holds nothing");
    }

    // the current entries are a separate question from what is in the cache,
    // and asking can fail on its own — a `bpd` that carries no agent cannot
    // launch anything either, and naming a current entry would be a guess. the
    // report above is still true, so it is printed either way
    //
    // there is one per interpreter tag this `bpd` carries, because each is what
    // a launch on the interpreter it is for would stage
    let current = bpd_engine::cache::current();
    let mut ok = true;
    match &current {
        Ok(builds) => {
            for build in builds {
                field("current", build.digest());
                let held = match cache.entry(build.digest()) {
                    Some(entry) => format!(
                        "staged, {} — clearing it costs the next launch a cold \
                         load of the agent",
                        size(entry.size())
                    ),
                    None => "not staged yet — the next launch will put it there".to_owned(),
                };
                note(&format!("{} — {held}", carries(build)));
            }
        }
        Err(_) => {
            field("current", "unknown");
            ok = false;
        }
    }

    for stray in cache.strays() {
        field("unexpected", &stray.path().display().to_string());
        note(stray.reason());
    }

    if let Err(error) = &current {
        println!();
        report_error(error);
    }
    if !cache.strays().is_empty() {
        println!();
        eprintln!(
            "error: {} in `{}` {} not put there by bpd, and it will not clear a \
             cache holding something it cannot account for",
            things(cache.strays().len()),
            cache.root().display(),
            if cache.strays().len() == 1 {
                "was"
            } else {
                "were"
            }
        );
        ok = false;
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// which interpreter a build is for, as a person would say it
fn carries(build: &Current) -> String {
    match build.tag() {
        Some(tag) => format!("the agent for python {tag}"),
        // the untagged artifact beside the binary. nothing about it names an
        // interpreter — the agent's own check at import is what decides — so
        // nothing is claimed about it here either
        None => "the development build".to_owned(),
    }
}

/// take the entries away, and say exactly which ones went
fn clear(cache: &Cache, args: &ClearArgs) -> ExitCode {
    let keep = if args.keep_current {
        match bpd_engine::cache::current() {
            Ok(builds) => builds,
            // refused rather than cleared: `--keep-current` names an entry to
            // keep, and clearing everything because the current one could not
            // be identified is the opposite of what was asked
            Err(error) => {
                report_error(&error);
                eprintln!(
                    "nothing has been removed. `bpd cache clear` without \
                     `--keep-current` takes every entry, cold load and all"
                );
                return ExitCode::FAILURE;
            }
        }
    } else {
        Vec::new()
    };

    if !cache.present() {
        field("cache", &cache.root().display().to_string());
        note("it is not there — nothing to remove");
        return ExitCode::SUCCESS;
    }

    let digests: Vec<&str> = keep.iter().map(Current::digest).collect();
    let cleared = match cache.clear(&digests) {
        Ok(cleared) => cleared,
        Err(error) => {
            report_error(&error);
            return ExitCode::FAILURE;
        }
    };

    field("removed", &entries(cleared.removed().len()));
    field("reclaimed", &size(cleared.reclaimed()));
    if args.keep_current {
        for entry in cleared.kept() {
            field("kept", entry.digest());
            note(&format!(
                "{} — an agent this bpd stages, so no launch pays a cold load",
                size(entry.size())
            ));
        }
        // an agent this `bpd` carries that was not in the cache to begin with.
        // saying so matters: the next launch stages it and pays the cold load
        // anyway, and a silent "kept nothing" would look like it had been kept
        for build in &keep {
            if cache.entry(build.digest()).is_none() {
                note(&format!(
                    "{} was not in the cache, so nothing was kept for it",
                    carries(build)
                ));
            }
        }
    }

    if cleared.succeeded() {
        return ExitCode::SUCCESS;
    }
    failed(&cleared)
}

/// every entry that would not go, named with what stopped it
fn failed(cleared: &Cleared) -> ExitCode {
    println!();
    for failure in cleared.failures() {
        field("failed", &failure.path().display().to_string());
        note(&failure.source().to_string());
    }
    println!();
    eprintln!(
        "error: {} could not be removed, and {} still there. on windows that is \
         a debuggee with the agent loaded, and the entry goes once it exits",
        entries(cleared.failures().len()),
        if cleared.failures().len() == 1 {
            "it is"
        } else {
            "they are"
        }
    );
    ExitCode::FAILURE
}

/// a count of entries, in the number a person would write
fn entries(count: usize) -> String {
    if count == 1 {
        "1 entry".to_owned()
    } else {
        format!("{count} entries")
    }
}

/// the same for what is in the cache and is not an entry
fn things(count: usize) -> String {
    if count == 1 {
        "1 thing".to_owned()
    } else {
        format!("{count} things")
    }
}

/// a size in the unit somebody reads it in, and in the bytes it really is
///
/// integer arithmetic throughout. a float would print a rounded number next to
/// the exact one it was rounded from
fn size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    let (unit, scale) = if bytes >= GIB {
        ("GiB", GIB)
    } else if bytes >= MIB {
        ("MiB", MIB)
    } else if bytes >= KIB {
        ("KiB", KIB)
    } else {
        return format!("{bytes} bytes");
    };

    format!(
        "{}.{} {unit} ({bytes} bytes)",
        bytes / scale,
        bytes % scale * 10 / scale
    )
}

fn field(name: &str, value: &str) {
    println!("{name:<12} {value}");
}

/// a second line under a field, aligned under its value
fn note(what: &str) {
    println!("{:<12} {what}", "");
}

#[cfg(test)]
mod tests {
    use super::size;

    /// the exact byte count is in there because it is the number a person
    /// checks against `du`, and the rounded one is what they read
    #[test]
    fn a_size_reads_in_the_unit_it_belongs_in_and_says_what_it_really_is() {
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(1024), "1.0 KiB (1024 bytes)");
        assert_eq!(size(5_620_704), "5.3 MiB (5620704 bytes)");
        assert_eq!(size(500_242_656), "477.0 MiB (500242656 bytes)");
        assert_eq!(size(2_147_483_648), "2.0 GiB (2147483648 bytes)");
    }

    /// the tenth is truncated rather than rounded, so a size never reads as
    /// more than it is
    #[test]
    fn a_size_never_reads_as_larger_than_it_is() {
        assert_eq!(size(2047), "1.9 KiB (2047 bytes)");
        assert_eq!(size(1024 * 1024 - 1), "1023.9 KiB (1048575 bytes)");
    }
}
