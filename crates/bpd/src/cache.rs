//! `bpd cache` — see what the staging caches are holding, and reclaim them
//!
//! staging keeps one copy of the agent per build and removes none, so the
//! directory only grows: 89 entries and 448 MB of them on the machine this was
//! written on. nothing removes them on its own, and that is a decision rather
//! than an omission — see [`bpd_engine::cache`] for why neither "what is still
//! needed" nor "what can be deleted" is answerable from inside one `bpd`
//!
//! there are **two** such directories and this covers both: the agents, and the
//! `sitecustomize` an `exec`'d child is entered through. they are reported as
//! two sections rather than one listing because they are two directories with
//! two current entries and two sizes — a single list would have to say which
//! root every line was about, which is the heading doing the same work twice
//!
//! so this is the whole of it: `bpd cache` says what is there, and
//! `bpd cache clear` takes it away. both exit non-zero when the answer is not
//! the whole answer — an entry that would not go, or something in a directory
//! that staging never wrote — because a cache command that printed "cleared"
//! over four of five entries would be worse than one that did nothing

use std::process::ExitCode;

use bpd_engine::cache::{Cache, Cleared, Current, Kind};

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
    /// leave the entries this `bpd` stages into — the agents it carries, and
    /// the hook a debugged child is entered through
    #[arg(long)]
    keep_current: bool,
}

/// both caches, in the order they are reported
///
/// the agents first because it is the one with the megabytes in it, and the one
/// somebody asking about a cache is almost always asking about
const BOTH: [Kind; 2] = [Kind::Agents, Kind::Children];

pub(crate) fn run(args: &Args) -> ExitCode {
    let mut ok = true;

    for (at, kind) in BOTH.into_iter().enumerate() {
        if at > 0 {
            println!();
        }
        // one root failing to open is not the other one's answer. a cache that
        // cannot be read is reported where it was reached, and the other is
        // still described — what would be untrue is a report that left one out
        // without saying so
        match bpd_engine::cache::open(kind) {
            Ok(cache) => {
                ok &= match &args.command {
                    None => report(&cache),
                    Some(Command::Clear(clear)) => self::clear(&cache, clear),
                };
            }
            Err(error) => {
                report_error(&error);
                ok = false;
            }
        }
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// what is in one of them, and what clearing it would cost
fn report(cache: &Cache) -> bool {
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
    let current = staged_by_this_bpd(cache.kind());
    let mut ok = true;
    match &current {
        Ok(staging) => {
            for one in staging {
                field("current", &one.digest);
                let held = match cache.entry(&one.digest) {
                    Some(entry) => format!("staged, {} — {}", size(entry.size()), one.costs),
                    None => "not staged yet — the next launch will put it there".to_owned(),
                };
                note(&format!("{} — {held}", one.what));
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

    ok
}

/// one entry this `bpd` would stage into a cache, said the way a person says it
struct Staging {
    digest: String,
    /// what it holds
    what: String,
    /// what removing it costs the next launch
    costs: &'static str,
}

/// every entry this `bpd` stages into one cache, or why it cannot say
///
/// the agents are a list, because a `bpd` carries one per interpreter tag and
/// each is what a launch on the interpreter it is for would stage — and it is a
/// question that can fail, since a `bpd` carrying no agent has none to name. the
/// child hook is one and cannot fail: it is compiled into the binary
fn staged_by_this_bpd(kind: Kind) -> Result<Vec<Staging>, bpd_engine::Error> {
    match kind {
        Kind::Agents => Ok(bpd_engine::cache::current()?
            .iter()
            .map(|build| Staging {
                digest: build.digest().to_owned(),
                what: carries(build),
                costs: "clearing it costs the next launch a cold load of the agent",
            })
            .collect()),
        Kind::Children => Ok(vec![Staging {
            digest: bpd_engine::cache::current_child_hook(),
            what: "the sitecustomize a debugged child is entered through".to_owned(),
            // said rather than borrowed from the agent's line: this is a few
            // hundred bytes of source, so what clearing it costs is a write and
            // a compile rather than a cold load of a shared object, and
            // claiming otherwise would be a reason to keep it that it has not
            // got
            costs: "clearing it costs the next launch with child debugging on a \
                    write of the hook, and that launch's first child a compile \
                    of it",
        }]),
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

/// take one cache's entries away, and say exactly which ones went
fn clear(cache: &Cache, args: &ClearArgs) -> bool {
    field("cache", &cache.root().display().to_string());

    let keep = if args.keep_current {
        match staged_by_this_bpd(cache.kind()) {
            Ok(staging) => staging,
            // refused rather than cleared: `--keep-current` names an entry to
            // keep, and clearing everything because the current one could not
            // be identified is the opposite of what was asked
            Err(error) => {
                report_error(&error);
                eprintln!(
                    "nothing has been removed from `{}`. `bpd cache clear` \
                     without `--keep-current` takes every entry, cold load and \
                     all",
                    cache.root().display()
                );
                return false;
            }
        }
    } else {
        Vec::new()
    };

    if !cache.present() {
        note("it is not there — nothing to remove");
        return true;
    }

    let digests: Vec<&str> = keep.iter().map(|one| one.digest.as_str()).collect();
    let cleared = match cache.clear(&digests) {
        Ok(cleared) => cleared,
        Err(error) => {
            report_error(&error);
            return false;
        }
    };

    field("removed", &entries(cleared.removed().len()));
    field("reclaimed", &size(cleared.reclaimed()));
    if args.keep_current {
        for entry in cleared.kept() {
            field("kept", entry.digest());
            note(&format!(
                "{} — {}",
                size(entry.size()),
                what_stages_it(&keep, entry.digest())
            ));
        }
        // something this `bpd` stages that was not in the cache to begin with.
        // saying so matters: the next launch stages it and pays the cost
        // anyway, and a silent "kept nothing" would look like it had been kept
        for one in &keep {
            if cache.entry(&one.digest).is_none() {
                note(&format!(
                    "{} was not in the cache, so nothing was kept for it",
                    one.what
                ));
            }
        }
    }

    if cleared.succeeded() {
        return true;
    }
    failed(&cleared);
    false
}

/// what a kept entry holds, out of the list that named it to be kept
fn what_stages_it(keep: &[Staging], digest: &str) -> String {
    let one = keep
        .iter()
        .find(|one| one.digest == digest)
        .unwrap_or_else(|| unreachable!("`{digest}` was kept because this list named it"));
    format!("{}, so no launch stages it again", one.what)
}

/// every entry that would not go, named with what stopped it
fn failed(cleared: &Cleared) {
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
