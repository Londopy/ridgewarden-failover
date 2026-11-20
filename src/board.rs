//! The status board: a short shared document of named entries ("Net control", "Shelter
//! at the school", "Highway 1") that anyone can update from any site, for the things a
//! served agency asks "what's the latest on...".
//!
//! It merges as a document, not a log. Each edit names the versions its writer saw
//! (`prev`), so an edit supersedes exactly those. Edits made on both sides of a split
//! supersede nothing of each other, so after the link returns an entry can have more
//! than one current version. The rule for who wins: the newest edit by its writer's
//! clock (then by event id) is shown, and the others stay listed as a conflict until
//! someone writes a new version having seen them all. Nothing is thrown away.

use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use ridgewarden_client::core::Projection;
use ridgewarden_client::event::Event;
use ridgewarden_client::util::rfc3339_to_ms;

use crate::chat::{CLOCK_SLACK_MS, clean_call, clean_channel};

pub const EDIT: &str = "failover.board";
pub const MAX_TEXT: usize = 500;
/// versions one edit may say it has seen; more than a handful only happens in a flood
const MAX_PREV: usize = 16;

pub struct Board;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS board (
    event_id    TEXT PRIMARY KEY,
    topic       TEXT NOT NULL,
    text        TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    author_key  TEXT NOT NULL,
    author_call TEXT NOT NULL,
    node        TEXT NOT NULL,
    call_sign   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS board_topic ON board(topic);
CREATE TABLE IF NOT EXISTS board_prev (
    event_id TEXT NOT NULL,
    prev_id  TEXT NOT NULL,
    PRIMARY KEY (event_id, prev_id)
);
CREATE INDEX IF NOT EXISTS board_prev_prev ON board_prev(prev_id);
"#;

/// Board text: up to 500 characters, line breaks allowed. Empty clears the entry.
pub fn clean_text(t: &str) -> Option<String> {
    let t = t.replace("\r\n", "\n").replace('\t', " ");
    let t = t.trim();
    let ok = t.chars().count() <= MAX_TEXT && !t.chars().any(|c| c.is_control() && c != '\n');
    ok.then(|| t.to_string())
}

/// Topic names follow channel names: short, and nothing that could carry markup.
pub fn clean_topic(t: &str) -> Option<String> {
    clean_channel(t).filter(|t| !t.is_empty())
}

fn is_event_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Projection for Board {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM board; DELETE FROM board_prev;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != EDIT {
            return Ok(());
        }
        let p = &e.payload;
        let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("");
        let (Some(topic), Some(text), Some(call)) = (
            clean_topic(s("topic")),
            clean_text(s("text")),
            clean_call(s("call_sign")),
        ) else {
            return Ok(());
        };
        let posted = rfc3339_to_ms(&e.wall_time).unwrap_or(ts);
        let node = ridgewarden_client::util::norm_node(s("node"));
        let node = if ridgewarden_client::util::valid_node(&node) {
            node
        } else {
            String::new()
        };
        tx.execute(
            "INSERT OR IGNORE INTO board (event_id, topic, text, ts, author_key, author_call, node, call_sign)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                e.id,
                topic,
                text,
                posted.min(ts + CLOCK_SLACK_MS),
                e.author.key,
                e.author.call,
                node,
                call
            ],
        )?;
        let prevs = p.get("prev").and_then(Value::as_array);
        for prev in prevs.into_iter().flatten().take(MAX_PREV) {
            if let Some(id) = prev.as_str().filter(|id| is_event_id(id) && *id != e.id) {
                tx.execute(
                    "INSERT OR IGNORE INTO board_prev (event_id, prev_id) VALUES (?1, ?2)",
                    params![e.id, id],
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub event_id: String,
    pub text: String,
    pub ts: i64,
    pub node: String,
    pub author_call: String,
    pub call_sign: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub topic: String,
    /// the version shown: the newest current one
    pub shown: Version,
    /// other current versions, written without seeing the shown one (across a split)
    pub conflicts: Vec<Version>,
}

impl Entry {
    /// Every current version's id, for the next edit's `prev`.
    pub fn heads(&self) -> Vec<String> {
        std::iter::once(&self.shown)
            .chain(&self.conflicts)
            .map(|v| v.event_id.clone())
            .collect()
    }
}

/// Every entry, by topic. An entry whose shown version is empty (cleared) and has no
/// conflict is left out.
pub fn entries(conn: &Connection) -> Result<Vec<Entry>> {
    let mut st = conn.prepare(
        "SELECT b.topic, b.event_id, b.text, b.ts, b.node, b.author_call, b.call_sign FROM board b
         WHERE NOT EXISTS (SELECT 1 FROM board_prev p JOIN board n ON n.event_id = p.event_id
                           WHERE p.prev_id = b.event_id AND n.topic = b.topic
                             AND n.event_id NOT IN (SELECT event_id FROM hidden))
           AND b.event_id NOT IN (SELECT event_id FROM hidden)
         ORDER BY b.topic, b.ts DESC, b.event_id DESC",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Version {
                event_id: r.get(1)?,
                text: r.get(2)?,
                ts: r.get(3)?,
                node: r.get(4)?,
                author_call: r.get(5)?,
                call_sign: r.get(6)?,
            },
        ))
    })?;
    let mut out: Vec<Entry> = vec![];
    for row in rows {
        let (topic, v) = row?;
        match out.last_mut() {
            Some(e) if e.topic == topic => e.conflicts.push(v),
            _ => out.push(Entry {
                topic,
                shown: v,
                conflicts: vec![],
            }),
        }
    }
    out.retain(|e| !e.shown.text.is_empty() || !e.conflicts.is_empty());
    Ok(out)
}

/// The current versions of one topic, for an edit's `prev`; empty for a new topic.
pub fn heads(conn: &Connection, topic: &str) -> Result<Vec<String>> {
    Ok(entries(conn)?
        .into_iter()
        .find(|e| e.topic == topic)
        .map(|e| e.heads())
        .unwrap_or_default())
}

/// Topics with more than one current version: what the last split left to sort out.
pub fn conflicts(conn: &Connection) -> Result<Vec<Entry>> {
    Ok(entries(conn)?
        .into_iter()
        .filter(|e| !e.conflicts.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::{T0, ev};
    use ridgewarden_client::core::init_db;
    use serde_json::json;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_db(&c, &[&crate::chat::Chat, &Board]).unwrap();
        c
    }

    fn edit(
        c: &mut Connection,
        site: &str,
        topic: &str,
        text: &str,
        prev: &[&str],
        ts: i64,
    ) -> String {
        let e = ev(
            site,
            EDIT,
            json!({"topic": topic, "text": text, "call_sign": "N0ED", "prev": prev, "node": format!("tst-{site}")}),
            ts,
        );
        let tx = c.transaction().unwrap();
        Board.apply(&tx, &e, ts).unwrap();
        tx.commit().unwrap();
        e.id
    }

    #[test]
    fn an_edit_replaces_the_version_it_saw() {
        let mut c = db();
        let v1 = edit(&mut c, "a", "Shelter", "open, 20 beds", &[], T0);
        edit(&mut c, "b", "Shelter", "open, 12 beds", &[&v1], T0 + 60_000);
        let e = entries(&c).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].shown.text, "open, 12 beds");
        assert!(e[0].conflicts.is_empty());
    }

    #[test]
    fn edits_from_both_sides_of_a_split_are_both_kept_newest_shown() {
        let mut here = db();
        let mut there = db();
        let v1 = edit(&mut here, "a", "Road", "open", &[], T0);
        edit(&mut there, "a", "Road", "open", &[], T0);
        // during the split each side edits the version it had
        let a = edit(
            &mut here,
            "a",
            "Road",
            "closed at the bridge",
            &[&v1],
            T0 + 60_000,
        );
        let b = edit(
            &mut there,
            "b",
            "Road",
            "one lane open",
            &[&v1],
            T0 + 120_000,
        );
        // the link returns: each side gets the other's edit
        edit(
            &mut here,
            "b",
            "Road",
            "one lane open",
            &[&v1],
            T0 + 120_000,
        );
        edit(
            &mut there,
            "a",
            "Road",
            "closed at the bridge",
            &[&v1],
            T0 + 60_000,
        );
        for c in [&here, &there] {
            let e = &entries(c).unwrap()[0];
            assert_eq!(e.shown.text, "one lane open");
            assert_eq!(e.conflicts.len(), 1);
            assert_eq!(e.conflicts[0].text, "closed at the bridge");
        }
        assert_eq!(conflicts(&here).unwrap().len(), 1);
        // someone who saw both writes the settled version
        edit(
            &mut here,
            "a",
            "Road",
            "one lane open, bridge closed",
            &[&a, &b],
            T0 + 180_000,
        );
        let e = &entries(&here).unwrap()[0];
        assert_eq!(e.shown.text, "one lane open, bridge closed");
        assert!(e.conflicts.is_empty());
    }

    #[test]
    fn an_edit_can_only_replace_versions_of_its_own_topic() {
        let mut c = db();
        let shelter = edit(&mut c, "a", "Shelter", "open", &[], T0);
        edit(&mut c, "b", "Weather", "rain", &[&shelter], T0 + 1);
        let topics: Vec<String> = entries(&c).unwrap().into_iter().map(|e| e.topic).collect();
        assert_eq!(topics, ["Shelter", "Weather"]);
    }

    #[test]
    fn clearing_an_entry_removes_it_and_bad_edits_are_ignored() {
        let mut c = db();
        let v1 = edit(&mut c, "a", "Weather", "wind 40 kt", &[], T0);
        edit(&mut c, "a", "<b>bad</b>", "x", &[], T0);
        edit(
            &mut c,
            "a",
            "Weather",
            &"x".repeat(MAX_TEXT + 1),
            &[&v1],
            T0,
        );
        assert_eq!(entries(&c).unwrap().len(), 1);
        edit(&mut c, "a", "Weather", "", &[&v1], T0 + 1);
        assert!(entries(&c).unwrap().is_empty());
    }
}
