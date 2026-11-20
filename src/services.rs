//! Every service this app runs, and how each one merges when a split mesh heals.
//!
//! A service declares its merge kind (an append-only log, a mergeable document, or
//! files), the plain-words rule for who wins on rejoin, and where it runs (everywhere,
//! or only on the placement's home site). Every event kind the app writes belongs to
//! exactly one service (a test holds that), so a new kind can't arrive without a merge
//! rule. The sites page lists these, and `/api/services` returns them with whatever the
//! last split left for people to look at.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::{board, chat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    /// a set of signed entries; merging is a union and nothing can conflict
    Log,
    /// named values edited in place; concurrent edits need a rule
    Document,
    /// blobs fetched by every site, with names and deletions as events
    Files,
}

impl Merge {
    pub fn label(self) -> &'static str {
        match self {
            Merge::Log => "append-only log",
            Merge::Document => "mergeable document",
            Merge::Files => "files",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runs {
    Everywhere,
    /// only on the site placement has chosen as home
    Home,
}

impl Runs {
    pub fn label(self) -> &'static str {
        match self {
            Runs::Everywhere => "every site",
            Runs::Home => "the home site only",
        }
    }
}

pub struct Service {
    pub name: &'static str,
    pub kinds: &'static [&'static str],
    pub merge: Merge,
    pub runs: Runs,
    /// who wins when both sides of a split changed the same thing
    pub rejoin: &'static str,
    /// what the last split left for people to sort out
    pub open: fn(&Connection) -> Result<Vec<String>>,
}

fn nothing_open(_: &Connection) -> Result<Vec<String>> {
    Ok(vec![])
}

fn board_open(conn: &Connection) -> Result<Vec<String>> {
    Ok(board::conflicts(conn)?
        .into_iter()
        .map(|e| {
            format!(
                "\"{}\" has {} versions written on different sides",
                e.topic,
                e.conflicts.len() + 1
            )
        })
        .collect())
}

fn files_open(conn: &Connection) -> Result<Vec<String>> {
    Ok(chat::name_clashes(conn)?
        .into_iter()
        .map(|(name, n)| format!("{n} different files are named \"{name}\""))
        .collect())
}

pub const SERVICES: &[Service] = &[
    Service {
        name: "Chat",
        kinds: &[chat::MSG],
        merge: Merge::Log,
        runs: Runs::Everywhere,
        rejoin: "Both sides' messages are kept; every site orders them by the poster's \
                 clock, then event id. Messages that crossed a split are marked late.",
        open: nothing_open,
    },
    Service {
        name: "Files",
        kinds: &[chat::FILE, chat::FILE_GONE, chat::HAVE],
        merge: Merge::Files,
        runs: Runs::Everywhere,
        rejoin: "Every upload is kept. When both sides shared a file under the same name, \
                 the MeshChat client shows the newer one and the files page lists both. \
                 Only the uploading site can delete, and a delete only removes what was \
                 shared before it.",
        open: files_open,
    },
    Service {
        name: "Status board",
        kinds: &[board::EDIT],
        merge: Merge::Document,
        runs: Runs::Everywhere,
        rejoin: "Each entry shows its newest version by the writer's clock; versions \
                 written on the other side of a split stay listed as a conflict until \
                 someone writes a version having seen them all.",
        open: board_open,
    },
    Service {
        name: "Moderation",
        kinds: &[chat::HIDE],
        merge: Merge::Log,
        runs: Runs::Everywhere,
        rejoin: "Hiding wins: anything hidden on either side stays hidden.",
        open: nothing_open,
    },
    Service {
        name: "Membership",
        kinds: &[chat::SITE],
        merge: Merge::Document,
        runs: Runs::Everywhere,
        rejoin: "Each site's newest heartbeat by its own clock describes it; liveness \
                 comes from when beats arrived here, never from the sender's clock.",
        open: nothing_open,
    },
    Service {
        name: "Drills",
        kinds: &[chat::DRILL],
        merge: Merge::Log,
        runs: Runs::Everywhere,
        rejoin: "Every site's drill results are kept.",
        open: nothing_open,
    },
    Service {
        name: "MeshChat bridge",
        kinds: &[],
        merge: Merge::Log,
        runs: Runs::Home,
        rejoin: "Runs on one site at a time, so each MeshChat message is copied in once \
                 and each message is sent out once. If both sides of a split each pick \
                 a home, a message copied in on both sides is shown once.",
        open: nothing_open,
    },
];

pub fn json(conn: &Connection) -> Result<Value> {
    let mut v = vec![];
    for s in SERVICES {
        v.push(json!({
            "name": s.name, "kinds": s.kinds, "merge": s.merge.label(), "runs": s.runs.label(),
            "rejoin": s.rejoin, "open": (s.open)(conn)?,
        }));
    }
    Ok(Value::Array(v))
}

/// Each service with what it has open, for the sites page.
pub fn rows(conn: &Connection) -> Result<Vec<(&'static Service, Vec<String>)>> {
    SERVICES.iter().map(|s| Ok((s, (s.open)(conn)?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_the_app_writes_has_exactly_one_merge_rule() {
        for kind in chat::WRITTEN {
            let owners = SERVICES.iter().filter(|s| s.kinds.contains(kind)).count();
            assert_eq!(owners, 1, "{kind} belongs to {owners} services");
        }
        for s in SERVICES {
            for k in s.kinds {
                assert!(
                    chat::WRITTEN.contains(k),
                    "{k} is declared but never written"
                );
            }
        }
    }
}
