//! The chat view: messages, shared files, and which sites run this app, built from the
//! core's event feed.
//!
//! Every message is its own signed event in the core's main log, so merging two sides of
//! a split is a set union keyed by event id, and every site orders the result the same way:
//! by timestamp, then event id. Nothing is ever trimmed.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use ridgewarden_client::core::Projection;
use ridgewarden_client::event::Event;
use ridgewarden_client::util::{esc, norm_node, rfc3339_to_ms, valid_node};

pub const MSG: &str = "failover.msg";
pub const FILE: &str = "failover.file";
pub const FILE_GONE: &str = "failover.file_gone";
/// telemetry heartbeat: this site's URL and who is logged in here
pub const SITE: &str = "failover.site";
/// an operator hiding a message or file from every site (`ridgewarden-failover hide`)
pub const HIDE: &str = "failover.hide";
/// a site saying it now holds a complete copy of a shared file
pub const HAVE: &str = "failover.have";
/// the result of a site-loss drill (`ridgewarden-failover drill`)
pub const DRILL: &str = "failover.drill";

/// Every event kind this app writes; each belongs to one service in `services`.
pub const WRITTEN: &[&str] = &[
    MSG,
    FILE,
    FILE_GONE,
    HAVE,
    HIDE,
    SITE,
    DRILL,
    crate::board::EDIT,
];

/// The MeshChat client's textarea limit.
pub const MAX_MESSAGE: usize = 1000;
const MAX_CALL: usize = 20;
const MAX_CHANNEL: usize = 40;
const MAX_FILE_NAME: usize = 100;
/// users one heartbeat may list; a real site has a handful
const MAX_USERS: usize = 50;
/// logins kept for this site, and the most any user list returns
pub const MAX_LOCAL_USERS: usize = 500;
/// a shared file bigger than this is a broken or hostile event, whatever the config says
const MAX_SANE_SIZE: u64 = 1 << 30;
/// how far ahead of the receiving core's clock a poster's clock may run before it's
/// no longer believed for ordering
pub const CLOCK_SLACK_MS: i64 = 10 * 60_000;

pub struct Chat;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS msgs (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id    TEXT NOT NULL UNIQUE,
    msg_id      TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    posted_ms   INTEGER NOT NULL,
    seen_ms     INTEGER,
    author_key  TEXT NOT NULL,
    author_call TEXT NOT NULL,
    node        TEXT NOT NULL,
    call_sign   TEXT NOT NULL,
    channel     TEXT NOT NULL,
    message     TEXT NOT NULL,
    -- the MeshChat node a bridged message came from; '' for messages posted here
    bridged     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS msgs_order ON msgs(ts, event_id);
CREATE INDEX IF NOT EXISTS msgs_msg_id ON msgs(msg_id);

CREATE TABLE IF NOT EXISTS files (
    event_id    TEXT PRIMARY KEY,
    root        TEXT NOT NULL,
    name        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    author_key  TEXT NOT NULL,
    node        TEXT NOT NULL,
    call_sign   TEXT NOT NULL,
    -- 0 not asked for yet, 1 asked the core to fetch it, 2 deleted and let go
    wanted      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS files_name ON files(name);

-- a file is gone when its uploader deleted it after sharing it; sharing it again brings it back
CREATE TABLE IF NOT EXISTS gone (
    root       TEXT NOT NULL,
    author_key TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (root, author_key)
);

CREATE TABLE IF NOT EXISTS hidden (
    event_id TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS haves (
    root     TEXT NOT NULL,
    site_key TEXT NOT NULL,
    PRIMARY KEY (root, site_key)
);

CREATE TABLE IF NOT EXISTS drills (
    event_id TEXT PRIMARY KEY,
    ts       INTEGER NOT NULL,
    node     TEXT NOT NULL,
    json     TEXT NOT NULL
);

-- ts orders a site's heartbeats: the sender's clock, capped by when this core got each one
CREATE TABLE IF NOT EXISTS sites (
    key     TEXT PRIMARY KEY,
    call    TEXT NOT NULL,
    node    TEXT NOT NULL,
    url     TEXT NOT NULL,
    power   TEXT NOT NULL,
    weight  INTEGER NOT NULL,
    ts      INTEGER NOT NULL
);

-- every heartbeat's run and monotonic counter, and when this site's core first saw it
CREATE TABLE IF NOT EXISTS beats (
    event_id   TEXT PRIMARY KEY,
    site_key   TEXT NOT NULL,
    run        TEXT NOT NULL,
    mono_ms    INTEGER NOT NULL,
    claimed_ms INTEGER NOT NULL,
    seen_ms    INTEGER
);
CREATE INDEX IF NOT EXISTS beats_site ON beats(site_key, run);

-- site_key '' is this site: people whose browsers poll this app
CREATE TABLE IF NOT EXISTS users (
    site_key  TEXT NOT NULL,
    call_sign TEXT NOT NULL,
    id        TEXT NOT NULL,
    epoch     INTEGER NOT NULL,
    PRIMARY KEY (site_key, call_sign, id)
);
"#;

fn s<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Channel and file names reach the stock client's HTML unescaped, so they get a
/// character set that can't carry markup.
fn safe_name(t: &str) -> bool {
    t.chars()
        .all(|c| c.is_alphanumeric() || " ._-()#+,".contains(c))
}

/// Call signs are typed by users and never checked against anything; this only keeps
/// them short and printable, upper-cased the way MeshChat shows them.
pub fn clean_call(c: &str) -> Option<String> {
    let c = c.trim().to_ascii_uppercase();
    let ok = !c.is_empty()
        && c.len() <= MAX_CALL
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b));
    ok.then_some(c)
}

pub fn clean_message(m: &str) -> Option<String> {
    let m = m.replace("\r\n", "\n").replace('\t', " ");
    let m = m.trim();
    let ok = !m.is_empty()
        && m.chars().count() <= MAX_MESSAGE
        && !m.chars().any(|c| c.is_control() && c != '\n');
    ok.then(|| m.to_string())
}

pub fn clean_channel(c: &str) -> Option<String> {
    let c = c.trim();
    (c.chars().count() <= MAX_CHANNEL && safe_name(c)).then(|| c.to_string())
}

/// MeshChat message ids are short hex strings the client makes up.
pub fn valid_msg_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A shared file's name as people see it: no path, nothing that could break a header.
pub fn clean_file_name(n: &str) -> Option<String> {
    let n = n.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let ok =
        !n.is_empty() && n.chars().count() <= MAX_FILE_NAME && !n.starts_with('.') && safe_name(n);
    ok.then(|| n.to_string())
}

pub fn valid_root(r: &str) -> bool {
    r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Site URLs from heartbeats end up in redirects and links.
pub fn valid_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://"))
        && u.len() > 8
        && u.len() <= 200
        && !u
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "\"'<>\\".contains(c))
}

/// Whether a site URL points into the mesh: a `*.local.mesh` name, a bare node name, a
/// 10.x (AREDN) or 44.x (AMPRNet) address, or a host listed in `extra`. The same rule
/// ridgewarden-health uses for what it will probe.
pub fn mesh_url(url: &str, extra: &[String]) -> bool {
    let Some(rest) = url.split_once("://").map(|(_, r)| r) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    let h = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if extra.iter().any(|e| e.eq_ignore_ascii_case(&h)) {
        return true;
    }
    // a bare number is an address to the OS, not a node name
    let numeric = h.bytes().all(|b| b.is_ascii_digit()) || h.starts_with("0x");
    if h.ends_with(".local.mesh") || (h != "localhost" && !numeric && valid_node(&h)) {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => matches!(ip.octets()[0], 10 | 44),
        _ => false,
    }
}

fn node_of(p: &Value) -> String {
    let n = norm_node(s(p, "node"));
    if valid_node(&n) { n } else { String::new() }
}

impl Projection for Chat {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM msgs; DELETE FROM files; DELETE FROM gone; DELETE FROM hidden; DELETE FROM haves; DELETE FROM drills; DELETE FROM sites; DELETE FROM beats;
             DELETE FROM users WHERE site_key <> '';",
        )?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        match e.kind.as_str() {
            MSG => apply_msg(tx, e, ts),
            FILE => apply_file(tx, e, ts),
            FILE_GONE => {
                let root = s(&e.payload, "root");
                if valid_root(root) {
                    tx.execute(
                        "INSERT INTO gone (root, author_key, ts) VALUES (?1, ?2, ?3)
                         ON CONFLICT(root, author_key) DO UPDATE SET ts = max(ts, excluded.ts)",
                        params![root.to_ascii_lowercase(), e.author.key, ts],
                    )?;
                }
                Ok(())
            }
            HIDE => {
                let id = s(&e.payload, "event_id");
                if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    tx.execute("INSERT OR IGNORE INTO hidden (event_id) VALUES (?1)", [id])?;
                }
                Ok(())
            }
            SITE => apply_site(tx, e, ts),
            HAVE => {
                let root = s(&e.payload, "root");
                if valid_root(root) {
                    tx.execute(
                        "INSERT OR IGNORE INTO haves (root, site_key) VALUES (?1, ?2)",
                        params![root.to_ascii_lowercase(), e.author.key],
                    )?;
                }
                Ok(())
            }
            DRILL => apply_drill(tx, e, ts),
            _ => Ok(()),
        }
    }
}

fn apply_msg(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let (Some(call), Some(text), Some(channel)) = (
        clean_call(s(p, "call_sign")),
        clean_message(s(p, "message")),
        clean_channel(s(p, "channel")),
    ) else {
        return Ok(());
    };
    let id = s(p, "id");
    let id = if valid_msg_id(id) {
        id.to_string()
    } else {
        e.id.chars().take(8).collect()
    };
    let bridge = p.get("bridge");
    let from = bridge.map(|b| norm_node(s(b, "from"))).unwrap_or_default();
    let from = if valid_node(&from) {
        from
    } else {
        String::new()
    };
    // a bridged message keeps the time its MeshChat node gave it
    let posted = match bridge.and_then(|b| b.get("epoch")).and_then(Value::as_i64) {
        Some(epoch) if !from.is_empty() => epoch.saturating_mul(1000),
        _ => rfc3339_to_ms(&e.wall_time).unwrap_or(ts),
    };
    if !from.is_empty() && !bridged_first(tx, &e.id, &id, &call)? {
        return Ok(());
    }
    if from.is_empty() {
        // the same message copied back in from MeshChat before the original got here
        tx.execute(
            "DELETE FROM msgs WHERE msg_id = ?1 AND call_sign = ?2 AND bridged <> ''",
            params![id, call],
        )?;
    }
    // `ts` is capped by the receiving core's clock, so it differs between sites; the
    // poster's own clock gives every site the same order
    let order = posted.min(ts + CLOCK_SLACK_MS);
    tx.execute(
        "INSERT OR IGNORE INTO msgs (event_id, msg_id, ts, posted_ms, author_key, author_call, node,
             call_sign, channel, message, bridged)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            e.id,
            id,
            order,
            posted,
            e.author.key,
            e.author.call,
            node_of(p),
            call,
            channel,
            text,
            from
        ],
    )?;
    Ok(())
}

/// Two sites may each copy the same MeshChat message in (two homes during a split). Keep
/// the copy with the smallest event id, so every site keeps the same one, and never one
/// that started on this mesh. A message is the same if its id and call sign match: the
/// text can come back changed, since MeshChat doesn't escape backslashes. Returns whether
/// this event should be stored.
fn bridged_first(tx: &Transaction, event: &str, id: &str, call: &str) -> Result<bool> {
    let other: Option<(String, String)> = tx
        .query_row(
            "SELECT event_id, bridged FROM msgs WHERE msg_id = ?1 AND call_sign = ?2
             ORDER BY bridged = '' DESC, event_id LIMIT 1",
            params![id, call],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match other {
        // it started on this mesh and went out to MeshChat; the original stays
        Some((_, b)) if b.is_empty() => Ok(false),
        Some((o, _)) if o.as_str() <= event => Ok(false),
        Some((o, _)) => {
            tx.execute("DELETE FROM msgs WHERE event_id = ?1", [o])?;
            Ok(true)
        }
        None => Ok(true),
    }
}

fn apply_drill(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let num = |k: &str| p.get(k).and_then(Value::as_i64);
    let (Some(lost), Some(sent)) = (num("lost"), num("sent")) else {
        return Ok(());
    };
    let target = norm_node(s(p, "target"));
    let ok = valid_node(&target) && (0..=100_000).contains(&sent) && (0..=sent).contains(&lost);
    if !ok {
        return Ok(());
    }
    let clean = json!({
        "target": target,
        "sites": num("sites").unwrap_or(0).clamp(0, 1000),
        "sent": sent,
        "lost": lost,
        "survive_ms": num("survive_ms").map(|v| v.max(0)),
        "detect_ms": num("detect_ms").map(|v| v.max(0)),
        "merge_ms": num("merge_ms").map(|v| v.max(0)),
        "error": p.get("error").and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|t| t.chars().filter(|c| !c.is_control()).take(200).collect::<String>()),
    });
    tx.execute(
        "INSERT OR IGNORE INTO drills (event_id, ts, node, json) VALUES (?1, ?2, ?3, ?4)",
        params![e.id, ts, node_of(p), clean.to_string()],
    )?;
    Ok(())
}

fn apply_file(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let root = s(p, "root");
    let size = p.get("size").and_then(Value::as_u64);
    let size = size.filter(|n| *n <= MAX_SANE_SIZE);
    let (Some(name), Some(size)) = (clean_file_name(s(p, "name")), size) else {
        return Ok(());
    };
    if !valid_root(root) {
        return Ok(());
    }
    let call = clean_call(s(p, "call_sign")).unwrap_or_default();
    tx.execute(
        "INSERT OR IGNORE INTO files (event_id, root, name, size, ts, author_key, node, call_sign)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            e.id,
            root.to_ascii_lowercase(),
            name,
            size as i64,
            ts,
            e.author.key,
            node_of(p),
            call
        ],
    )?;
    Ok(())
}

/// Power sources an operator can name for a site, best first.
pub const POWER: &[&str] = &["grid", "generator", "solar", "battery", "unknown"];

fn apply_beat(tx: &Transaction, e: &Event, claimed: i64) -> Result<()> {
    let p = &e.payload;
    let run = s(p, "run");
    let mono = p.get("mono_ms").and_then(Value::as_i64);
    let run_ok =
        !run.is_empty() && run.len() <= 32 && run.bytes().all(|b| b.is_ascii_alphanumeric());
    // a counter past a few decades is broken or hostile, and would overflow the sums later
    if let (true, Some(mono)) = (run_ok, mono.filter(|m| (0..1 << 40).contains(m))) {
        tx.execute(
            "INSERT OR IGNORE INTO beats (event_id, site_key, run, mono_ms, claimed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![e.id, e.author.key, run, mono, claimed],
        )?;
    }
    Ok(())
}

fn apply_site(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let claimed = rfc3339_to_ms(&e.wall_time).unwrap_or(ts);
    apply_beat(tx, e, claimed)?;
    let last: Option<i64> = tx
        .query_row(
            "SELECT ts FROM sites WHERE key = ?1",
            [&e.author.key],
            |r| r.get(0),
        )
        .optional()?;
    // heartbeats held up by a split arrive late and out of order; only the newest counts.
    // `ts` is capped by when this core got it, so one beat dated years ahead can't freeze
    // the site's details.
    if last.is_some_and(|l| l > ts) {
        return Ok(());
    }
    let url = s(p, "url");
    let url = if valid_url(url) { url } else { "" };
    let power = s(p, "power");
    let power = if POWER.contains(&power) {
        power
    } else {
        "unknown"
    };
    let weight = p
        .get("weight")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, 100);
    tx.execute(
        "INSERT INTO sites (key, call, node, url, power, weight, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(key) DO UPDATE SET call = excluded.call, node = excluded.node,
             url = excluded.url, power = excluded.power, weight = excluded.weight, ts = excluded.ts",
        params![e.author.key, e.author.call, node_of(p), url, power, weight, ts],
    )?;
    tx.execute("DELETE FROM users WHERE site_key = ?1", [&e.author.key])?;
    let users = p.get("users").and_then(Value::as_array);
    for u in users.into_iter().flatten().take(MAX_USERS) {
        let id = s(u, "id");
        let epoch = u.get("epoch").and_then(Value::as_i64).unwrap_or(0);
        let Some(call) = clean_call(s(u, "call_sign")) else {
            continue;
        };
        if id.is_empty() || id.len() > 16 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO users (site_key, call_sign, id, epoch) VALUES (?1, ?2, ?3, ?4)",
            params![e.author.key, call, id, epoch.min(ts / 1000)],
        )?;
    }
    Ok(())
}

/// This box's clock stepped by `step` ms: move the heartbeat arrival times recorded on
/// the old clock, so each run's counter stays pinned to the right time.
pub fn shift_seen(conn: &Connection, step: i64) -> Result<()> {
    conn.execute(
        "UPDATE beats SET seen_ms = seen_ms + ?1 WHERE seen_ms IS NOT NULL",
        [step],
    )?;
    Ok(())
}

/// Record when the core first saw each message and heartbeat; `apply` only gets the
/// earlier of that and the author's clock.
pub fn note_seen(conn: &Connection, items: &[ridgewarden_client::core::FeedItem]) -> Result<()> {
    for it in items.iter().filter(|i| i.trusted) {
        let Some(seen) = it.seen_ms else { continue };
        let sql = match it.event.kind.as_str() {
            MSG => "UPDATE msgs SET seen_ms = ?2 WHERE event_id = ?1 AND seen_ms IS NULL",
            SITE => "UPDATE beats SET seen_ms = ?2 WHERE event_id = ?1 AND seen_ms IS NULL",
            _ => continue,
        };
        conn.execute(sql, params![it.event.id, seen])?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub seq: i64,
    pub event_id: String,
    /// the id the MeshChat client knows it by
    pub id: String,
    pub ts: i64,
    pub posted_ms: i64,
    pub seen_ms: Option<i64>,
    pub author_key: String,
    pub author_call: String,
    pub node: String,
    pub call_sign: String,
    pub channel: String,
    pub message: String,
    /// the MeshChat node it was bridged from, or ""
    pub bridged: String,
}

impl Msg {
    /// How long after posting it reached this site, if that's longer than `late_ms`.
    pub fn late_by(&self, late_ms: i64) -> Option<i64> {
        let lag = self.seen_ms? - self.posted_ms;
        (lag > late_ms).then_some(lag)
    }

    /// The stock client puts the message into its page as HTML, so it goes out escaped.
    pub fn api_json(&self) -> Value {
        json!({
            "id": self.id,
            "epoch": self.ts / 1000,
            "message": esc(&self.message),
            "call_sign": self.call_sign,
            "node": self.node,
            "platform": "pi",
            "channel": self.channel,
        })
    }
}

// two messages with the same client id (a collision, or a client that reuses ids): the
// older keeps it and the other is known by its event id, so the client shows both
const MSG_COLS: &str = "m.seq, m.event_id,
    CASE WHEN EXISTS (SELECT 1 FROM msgs o WHERE o.msg_id = m.msg_id
                      AND (o.ts, o.event_id) < (m.ts, m.event_id))
         THEN substr(m.event_id, 1, 8) ELSE m.msg_id END,
    m.ts, m.posted_ms, m.seen_ms, m.author_key, m.author_call, m.node, m.call_sign, m.channel, m.message, m.bridged";

fn msg_row(r: &rusqlite::Row) -> rusqlite::Result<Msg> {
    Ok(Msg {
        seq: r.get(0)?,
        event_id: r.get(1)?,
        id: r.get(2)?,
        ts: r.get(3)?,
        posted_ms: r.get(4)?,
        seen_ms: r.get(5)?,
        author_key: r.get(6)?,
        author_call: r.get(7)?,
        node: r.get(8)?,
        call_sign: r.get(9)?,
        channel: r.get(10)?,
        message: r.get(11)?,
        bridged: r.get(12)?,
    })
}

/// Goes up whenever a message is added or hidden; the MeshChat client fetches only
/// when it changes.
pub fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT (SELECT coalesce(max(seq), 0) FROM msgs) + (SELECT count(*) FROM hidden)",
        [],
        |r| r.get(0),
    )?)
}

const SHOWN: &str = "m.event_id NOT IN (SELECT event_id FROM hidden)";

/// Newest first, optionally only those added after version `since`.
pub fn newest(conn: &Connection, limit: usize, since: Option<i64>) -> Result<Vec<Msg>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM msgs m WHERE {SHOWN} AND m.seq > ?1 ORDER BY m.ts DESC, m.event_id DESC LIMIT ?2"
    );
    let v = conn
        .prepare(&sql)?
        .query_map(params![since.unwrap_or(0), limit as i64], msg_row)?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// One page of the chat, newest first; `channel` None is every channel.
pub fn page(
    conn: &Connection,
    channel: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<Vec<Msg>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM msgs m WHERE {SHOWN} AND (?1 IS NULL OR m.channel = ?1)
         ORDER BY m.ts DESC, m.event_id DESC LIMIT ?2 OFFSET ?3"
    );
    let v = conn
        .prepare(&sql)?
        .query_map(params![channel, limit as i64, offset as i64], msg_row)?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

pub fn count(conn: &Connection, channel: Option<&str>) -> Result<i64> {
    let sql =
        format!("SELECT count(*) FROM msgs m WHERE {SHOWN} AND (?1 IS NULL OR m.channel = ?1)");
    Ok(conn.query_row(&sql, [channel], |r| r.get(0))?)
}

/// Every channel anyone has posted in, the default ("") first.
pub fn channels(conn: &Connection) -> Result<Vec<String>> {
    let v = conn
        .prepare(&format!(
            "SELECT DISTINCT m.channel FROM msgs m WHERE {SHOWN} ORDER BY m.channel"
        ))?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Whether this exact message is already in the view: a client retrying a send whose
/// answer got lost shouldn't post it twice.
pub fn already_posted(conn: &Connection, id: &str, call: &str, text: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM msgs WHERE msg_id = ?1 AND call_sign = ?2 AND message = ?3",
            params![id, call, text],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A MeshChat client polling this site: remember who is logged in here.
/// Whether this call sign and client id have logged in here before.
pub fn known_user(conn: &Connection, call: &str, id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM users WHERE site_key = '' AND call_sign = ?1 AND id = ?2",
            params![call, id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub fn heartbeat(conn: &Connection, call: &str, id: &str, epoch: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO users (site_key, call_sign, id, epoch) VALUES ('', ?1, ?2, ?3)
         ON CONFLICT(site_key, call_sign, id) DO UPDATE SET epoch = max(epoch, excluded.epoch)",
        params![call, id, epoch],
    )?;
    // logins cost nothing to invent, so only the most recent few hundred are kept
    conn.execute(
        "DELETE FROM users WHERE site_key = '' AND rowid NOT IN
             (SELECT rowid FROM users WHERE site_key = '' ORDER BY epoch DESC LIMIT ?1)",
        [MAX_LOCAL_USERS as i64],
    )?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub call_sign: String,
    pub id: String,
    pub epoch: i64,
    /// the node of the site they're connected to; "" for one that hasn't said
    pub node: String,
}

/// Users heard from since `since_s`, here and at other sites, newest first.
/// `me` is this site's key, whose own heartbeats come back through the feed.
pub fn users(conn: &Connection, me: &str, here: &str, since_s: i64) -> Result<Vec<User>> {
    let v = conn
        .prepare(
            "SELECT u.call_sign, u.id, u.epoch,
                    CASE WHEN u.site_key = '' THEN ?2 ELSE coalesce(s.node, '') END
             FROM users u LEFT JOIN sites s ON s.key = u.site_key
             WHERE u.epoch >= ?3 AND u.site_key <> ?1
             ORDER BY u.epoch DESC, u.call_sign LIMIT ?4",
        )?
        .query_map(params![me, here, since_s, MAX_LOCAL_USERS as i64], |r| {
            Ok(User {
                call_sign: r.get(0)?,
                id: r.get(1)?,
                epoch: r.get(2)?,
                node: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// People logged in here since `since_s`, for this site's heartbeat.
pub fn local_users(conn: &Connection, since_s: i64) -> Result<Vec<Value>> {
    let v = conn
        .prepare(
            "SELECT call_sign, id, epoch FROM users WHERE site_key = '' AND epoch >= ?1
             ORDER BY epoch DESC LIMIT ?2",
        )?
        .query_map(params![since_s, MAX_USERS as i64], |r| {
            Ok(
                json!({"call_sign": r.get::<_, String>(0)?, "id": r.get::<_, String>(1)?,
                      "epoch": r.get::<_, i64>(2)?}),
            )
        })?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

#[derive(Clone, Debug, PartialEq)]
pub struct SharedFile {
    pub event_id: String,
    pub root: String,
    pub name: String,
    pub size: i64,
    pub ts: i64,
    pub author_key: String,
    pub node: String,
    pub call_sign: String,
    pub wanted: bool,
}

const FILE_COLS: &str =
    "f.event_id, f.root, f.name, f.size, f.ts, f.author_key, f.node, f.call_sign, f.wanted";
const LIVE: &str = "NOT EXISTS (SELECT 1 FROM gone g WHERE g.root = f.root
        AND g.author_key = f.author_key AND g.ts >= f.ts)
    AND f.event_id NOT IN (SELECT event_id FROM hidden)";

fn file_row(r: &rusqlite::Row) -> rusqlite::Result<SharedFile> {
    Ok(SharedFile {
        event_id: r.get(0)?,
        root: r.get(1)?,
        name: r.get(2)?,
        size: r.get(3)?,
        ts: r.get(4)?,
        author_key: r.get(5)?,
        node: r.get(6)?,
        call_sign: r.get(7)?,
        wanted: r.get(8)?,
    })
}

fn query_files(conn: &Connection, filter: &str, arg: &str) -> Result<Vec<SharedFile>> {
    let sql = format!(
        "SELECT {FILE_COLS} FROM files f WHERE {LIVE} AND {filter} ORDER BY f.ts DESC, f.event_id DESC"
    );
    let v = conn
        .prepare(&sql)?
        .query_map([arg], file_row)?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Every file not deleted, newest first, every version of a name.
pub fn files(conn: &Connection) -> Result<Vec<SharedFile>> {
    query_files(conn, "?1 = ?1", "")
}

/// One file per name, the newest upload winning, as the MeshChat client expects.
pub fn files_by_name(conn: &Connection) -> Result<Vec<SharedFile>> {
    let mut seen = std::collections::BTreeSet::new();
    Ok(files(conn)?
        .into_iter()
        .filter(|f| seen.insert(f.name.clone()))
        .collect())
}

pub fn file_named(conn: &Connection, name: &str) -> Result<Option<SharedFile>> {
    Ok(query_files(conn, "f.name = ?1", name)?.into_iter().next())
}

pub fn file_by_root(conn: &Connection, root: &str) -> Result<Option<SharedFile>> {
    Ok(query_files(conn, "f.root = ?1", root)?.into_iter().next())
}

/// Bytes of live files this site really holds: its own uploads, and files whose copy it
/// checked against the shared size before saying it had them. Sizes other stations only
/// claim don't count.
pub fn used_bytes(conn: &Connection, me: &str) -> Result<i64> {
    let sql = format!(
        "SELECT coalesce(sum(size), 0) FROM (SELECT DISTINCT f.root, f.size FROM files f WHERE {LIVE}
             AND (f.author_key = ?1 OR f.root IN (SELECT root FROM haves WHERE site_key = ?1)))"
    );
    Ok(conn.query_row(&sql, [me], |r| r.get(0))?)
}

/// Files other sites shared that this site hasn't asked its core for yet: (root, size).
pub fn unwanted(conn: &Connection, me: &str, max_size: i64) -> Result<Vec<(String, i64)>> {
    let sql = format!(
        "SELECT f.root, max(f.size) FROM files f WHERE {LIVE} AND f.wanted = 0 AND f.author_key <> ?1
         AND f.size <= ?2 GROUP BY f.root ORDER BY min(f.ts)"
    );
    let v = conn
        .prepare(&sql)?
        .query_map(params![me, max_size], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Bytes of shared files this site keeps or has asked its core to fetch.
pub fn held_bytes(conn: &Connection, me: &str) -> Result<i64> {
    let sql = format!(
        "SELECT coalesce(sum(size), 0) FROM (SELECT DISTINCT f.root, f.size FROM files f
         WHERE {LIVE} AND (f.wanted = 1 OR f.author_key = ?1))"
    );
    Ok(conn.query_row(&sql, [me], |r| r.get(0))?)
}

pub fn mark_wanted(conn: &Connection, root: &str) -> Result<()> {
    conn.execute("UPDATE files SET wanted = 1 WHERE root = ?1", [root])?;
    Ok(())
}

/// A deleted or hidden file's blob has been let go.
pub fn mark_released(conn: &Connection, root: &str) -> Result<()> {
    conn.execute("UPDATE files SET wanted = 2 WHERE root = ?1", [root])?;
    Ok(())
}

/// Roots of deleted or hidden files not yet released, so the core can drop them and
/// their cached copies can go.
pub fn deleted_roots(conn: &Connection) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT d.root FROM files d WHERE d.wanted <> 2
         AND NOT EXISTS (SELECT 1 FROM files f WHERE f.root = d.root AND {LIVE})"
    );
    let v = conn
        .prepare(&sql)?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Messages posted on this mesh (not bridged in) added after `after`, oldest first, for
/// sending on to MeshChat nodes.
pub fn to_bridge(conn: &Connection, after: i64, limit: usize) -> Result<Vec<Msg>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM msgs m WHERE {SHOWN} AND m.seq > ?1 AND m.bridged = ''
         ORDER BY m.seq LIMIT ?2"
    );
    let v = conn
        .prepare(&sql)?
        .query_map(params![after, limit as i64], msg_row)?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Whether a message with this client id and call sign is here, bridged or not, shown
/// or hidden.
pub fn has_message(conn: &Connection, id: &str, call: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM msgs WHERE msg_id = ?1 AND call_sign = ?2",
            params![id, call],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The highest sequence number of messages that reached this site before `cutoff`.
pub fn seq_before(conn: &Connection, cutoff: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT coalesce(max(seq), 0) FROM msgs WHERE coalesce(seen_ms, ts) < ?1",
        [cutoff],
        |r| r.get(0),
    )?)
}

/// The highest message sequence number in the view (not counting hides).
pub fn last_seq(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT coalesce(max(seq), 0) FROM msgs", [], |r| r.get(0))?)
}

/// Sites that hold a complete copy of each file, by root: the uploader plus every site
/// that has said so.
pub fn copies(conn: &Connection) -> Result<std::collections::BTreeMap<String, i64>> {
    let v = conn
        .prepare(
            "SELECT root, count(DISTINCT k) FROM (SELECT root, site_key AS k FROM haves
                 UNION SELECT root, author_key FROM files) GROUP BY root",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Whether this site has already said it holds `root`.
pub fn have(conn: &Connection, root: &str, me: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM haves WHERE root = ?1 AND site_key = ?2
             UNION SELECT 1 FROM files WHERE root = ?1 AND author_key = ?2",
            params![root, me],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Names that more than one different live file goes by: (name, how many).
pub fn name_clashes(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let sql = format!(
        "SELECT f.name, count(DISTINCT f.root) FROM files f WHERE {LIVE}
         GROUP BY f.name HAVING count(DISTINCT f.root) > 1 ORDER BY f.name"
    );
    let v = conn
        .prepare(&sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Drill {
    pub ts: i64,
    /// the site the drill was run from
    pub node: String,
    pub result: Value,
}

/// Drill results from every site, newest first.
pub fn drills(conn: &Connection) -> Result<Vec<Drill>> {
    let v = conn
        .prepare("SELECT ts, node, json FROM drills ORDER BY ts DESC")?
        .query_map([], |r| {
            let j: String = r.get(2)?;
            Ok(Drill {
                ts: r.get(0)?,
                node: r.get(1)?,
                result: serde_json::from_str(&j).unwrap_or(Value::Null),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Event ids of messages, files and board versions starting with `prefix` (8 or more hex characters).
pub fn find_event(conn: &Connection, prefix: &str) -> Result<Vec<String>> {
    let p = prefix.trim().to_ascii_lowercase();
    if p.len() < 8 || p.len() > 64 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(vec![]);
    }
    let v = conn
        .prepare(
            "SELECT event_id FROM msgs WHERE substr(event_id, 1, length(?1)) = ?1
             UNION SELECT event_id FROM files WHERE substr(event_id, 1, length(?1)) = ?1
             UNION SELECT event_id FROM board WHERE substr(event_id, 1, length(?1)) = ?1",
        )?
        .query_map([&p], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Drop what nobody needs any more: old logins and raw link data. Messages and files stay.
pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute("DELETE FROM users WHERE epoch < ?1", [now / 1000 - 3600])?;
    // a day of heartbeats is plenty to pin each run's counter to this site's clock
    conn.execute("DELETE FROM beats WHERE seen_ms < ?1", [now - 86_400_000])?;
    let old = now - 2 * 86_400_000;
    conn.execute("DELETE FROM link_samples WHERE ts < ?1", [old])?;
    conn.execute("DELETE FROM polls WHERE ts < ?1", [old])?;
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ridgewarden_client::core::init_db;
    use ridgewarden_client::event::Author;
    use ridgewarden_client::mesh::Mesh;
    use ridgewarden_client::util::ms_to_rfc3339;

    pub const T0: i64 = 1_800_000_000_000;

    /// An event as the feed delivers it. Projections never check signatures (the core
    /// does), so tests don't need real ones.
    pub fn ev(site: &str, kind: &str, payload: Value, ts: i64) -> Event {
        let id = fake_id(site, kind, &payload, ts);
        Event {
            id,
            author: Author {
                call: format!("N0{}", site.to_ascii_uppercase()),
                key: format!("{site:0>64}"),
            },
            seq: 0,
            prev: None,
            lamport: 0,
            wall_time: ms_to_rfc3339(ts),
            mono_time: 0,
            kind: kind.into(),
            payload,
            sig: String::new(),
            log: None,
        }
    }

    fn fake_id(site: &str, kind: &str, p: &Value, ts: i64) -> String {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in format!("{site}{kind}{p}{ts}").bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        format!("{h:016x}{:048x}", 0)
    }

    pub fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_db(&c, &[&Mesh, &Chat, &crate::board::Board]).unwrap();
        c
    }

    pub fn apply(c: &mut Connection, e: &Event, ts: i64) {
        let tx = c.transaction().unwrap();
        Chat.apply(&tx, e, ts).unwrap();
        tx.commit().unwrap();
    }

    pub fn msg(site: &str, id: &str, call: &str, text: &str, ts: i64) -> Event {
        ev(
            site,
            MSG,
            json!({"id": id, "call_sign": call, "message": text, "channel": "", "node": format!("tst-{site}")}),
            ts,
        )
    }

    fn texts(c: &Connection) -> Vec<String> {
        newest(c, 1000, None)
            .unwrap()
            .into_iter()
            .map(|m| m.message)
            .collect()
    }

    #[test]
    fn both_sides_of_a_split_merge_in_the_same_order_whatever_arrives_first() {
        let a: Vec<Event> = (0..5)
            .map(|i| {
                msg(
                    "a",
                    &format!("a{i}"),
                    "N0AAA",
                    &format!("from a {i}"),
                    T0 + i * 60_000 + 1000,
                )
            })
            .collect();
        let b: Vec<Event> = (0..5)
            .map(|i| {
                msg(
                    "b",
                    &format!("b{i}"),
                    "N0BBB",
                    &format!("from b {i}"),
                    T0 + i * 60_000,
                )
            })
            .collect();
        let mut here = db();
        let mut there = db();
        for e in a.iter().chain(&b) {
            apply(&mut here, e, rfc3339_to_ms(&e.wall_time).unwrap());
        }
        // the other site gets them in the opposite order, and its core's clock is 3 s
        // slow, so a's messages look 3 s older there than a's own clock says
        for e in b.iter().rev() {
            apply(&mut there, e, rfc3339_to_ms(&e.wall_time).unwrap());
        }
        for e in a.iter().rev() {
            apply(&mut there, e, rfc3339_to_ms(&e.wall_time).unwrap() - 3000);
        }
        // and a duplicate delivery changes nothing
        apply(&mut there, &a[2], T0 + 120_000);
        assert_eq!(texts(&here).len(), 10);
        assert_eq!(texts(&here), texts(&there));
        assert_eq!(texts(&here)[0], "from a 4");
        assert_eq!(texts(&here)[9], "from b 0");
    }

    #[test]
    fn same_timestamp_is_ordered_by_event_id() {
        let mut c = db();
        let x = msg("a", "1", "N0AAA", "x", T0);
        let y = msg("b", "2", "N0BBB", "y", T0);
        apply(&mut c, &y, T0);
        apply(&mut c, &x, T0);
        let got = newest(&c, 10, None).unwrap();
        assert!(got[0].event_id > got[1].event_id);
    }

    #[test]
    fn hostile_or_broken_messages_are_ignored() {
        let mut c = db();
        let long = "x".repeat(MAX_MESSAGE + 1);
        for p in [
            json!({"id": "1", "call_sign": "N0AAA", "message": ""}),
            json!({"id": "2", "call_sign": "N0AAA", "message": long}),
            json!({"id": "3", "call_sign": "<script>", "message": "hi"}),
            json!({"id": "4", "message": "no call"}),
            json!({"id": "5", "call_sign": "N0AAA", "message": "bell\u{7}"}),
            json!({"id": "6", "call_sign": "N0AAA", "message": "hi", "channel": "a\nb"}),
            json!("not an object"),
        ] {
            apply(&mut c, &ev("a", MSG, p, T0), T0);
        }
        assert_eq!(count(&c, None).unwrap(), 0);
        apply(
            &mut c,
            &ev(
                "a",
                MSG,
                json!({"id": "zz", "call_sign": "n0aaa", "message": " fine\r\nline two ", "node": "../etc"}),
                T0,
            ),
            T0,
        );
        let m = &newest(&c, 1, None).unwrap()[0];
        assert_eq!(m.call_sign, "N0AAA");
        assert_eq!(m.message, "fine\nline two");
        assert_eq!(m.node, "");
        // a bad client id is replaced with one from the event
        assert_eq!(m.id, m.event_id[..8]);
    }

    #[test]
    fn version_moves_on_every_new_message_and_since_returns_only_new_ones() {
        let mut c = db();
        assert_eq!(version(&c).unwrap(), 0);
        apply(&mut c, &msg("a", "1", "N0AAA", "one", T0), T0);
        let v1 = version(&c).unwrap();
        apply(&mut c, &msg("a", "1", "N0AAA", "one", T0), T0);
        assert_eq!(version(&c).unwrap(), v1);
        apply(
            &mut c,
            &msg("b", "2", "N0BBB", "two", T0 - 60_000),
            T0 - 60_000,
        );
        let v2 = version(&c).unwrap();
        assert!(v2 > v1);
        let new: Vec<String> = newest(&c, 10, Some(v1))
            .unwrap()
            .into_iter()
            .map(|m| m.message)
            .collect();
        assert_eq!(new, ["two"]);
    }

    #[test]
    fn colliding_client_ids_stay_distinct() {
        let mut c = db();
        apply(&mut c, &msg("a", "abcd1234", "N0AAA", "first", T0), T0);
        apply(
            &mut c,
            &msg("b", "abcd1234", "N0BBB", "second", T0 + 1),
            T0 + 1,
        );
        let got = newest(&c, 10, None).unwrap();
        assert_eq!(got[1].id, "abcd1234");
        assert_ne!(got[0].id, "abcd1234");
    }

    #[test]
    fn retry_detection_matches_id_call_and_text() {
        let mut c = db();
        apply(&mut c, &msg("a", "abcd", "N0AAA", "hello", T0), T0);
        assert!(already_posted(&c, "abcd", "N0AAA", "hello").unwrap());
        assert!(!already_posted(&c, "abcd", "N0AAA", "hello again").unwrap());
    }

    #[test]
    fn only_the_uploading_site_can_delete_a_file() {
        let mut c = db();
        let root = "ab".repeat(32);
        apply(
            &mut c,
            &ev(
                "a",
                FILE,
                json!({"root": root, "name": "map.pdf", "size": 10}),
                T0,
            ),
            T0,
        );
        apply(
            &mut c,
            &ev("b", FILE_GONE, json!({"root": root}), T0 + 1),
            T0 + 1,
        );
        assert_eq!(files(&c).unwrap().len(), 1);
        apply(
            &mut c,
            &ev("a", FILE_GONE, json!({"root": root}), T0 + 2),
            T0 + 2,
        );
        assert!(files(&c).unwrap().is_empty());
        assert_eq!(deleted_roots(&c).unwrap(), [root]);
    }

    #[test]
    fn newest_upload_wins_a_name_but_older_versions_are_kept() {
        let mut c = db();
        let (r1, r2) = ("11".repeat(32), "22".repeat(32));
        apply(
            &mut c,
            &ev(
                "a",
                FILE,
                json!({"root": r1, "name": "net.txt", "size": 5}),
                T0,
            ),
            T0,
        );
        apply(
            &mut c,
            &ev(
                "b",
                FILE,
                json!({"root": r2, "name": "net.txt", "size": 7}),
                T0 + 9,
            ),
            T0 + 9,
        );
        assert_eq!(files(&c).unwrap().len(), 2);
        let named = files_by_name(&c).unwrap();
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].root, r2);
        assert_eq!(file_named(&c, "net.txt").unwrap().unwrap().root, r2);
        // only what this site really holds counts: its own upload, then the other once it
        // has checked its copy
        let me = format!("{:0>64}", "a");
        assert_eq!(used_bytes(&c, &me).unwrap(), 5);
        apply(
            &mut c,
            &ev("a", HAVE, json!({"root": r2}), T0 + 10),
            T0 + 10,
        );
        assert_eq!(used_bytes(&c, &me).unwrap(), 12);
    }

    #[test]
    fn a_poster_clock_far_ahead_is_not_believed() {
        let mut c = db();
        apply(
            &mut c,
            &msg("a", "1", "N0AAA", "from the future", T0 + 86_400_000),
            T0,
        );
        apply(
            &mut c,
            &msg("b", "2", "N0BBB", "now", T0 + 60 * 60_000),
            T0 + 60 * 60_000,
        );
        assert_eq!(texts(&c), ["now", "from the future"]);
    }

    #[test]
    fn messages_that_crossed_a_split_are_marked_late() {
        let mut c = db();
        let on_time = msg("a", "1", "N0AAA", "on time", T0);
        let late = msg("b", "2", "N0BBB", "held up", T0);
        apply(&mut c, &on_time, T0);
        apply(&mut c, &late, T0);
        let item = |e: &Event, seen, trusted| ridgewarden_client::core::FeedItem {
            seq: 0,
            event: e.clone(),
            trusted,
            seen_ms: Some(seen),
        };
        note_seen(
            &c,
            &[
                item(&on_time, T0 + 20_000, true),
                item(&late, T0 + 900_000, true),
            ],
        )
        .unwrap();
        // the first arrival counts; a later copy doesn't move it
        note_seen(&c, &[item(&on_time, T0 + 5_000_000, true)]).unwrap();
        let got = newest(&c, 10, None).unwrap();
        let by = |t: &str| {
            got.iter()
                .find(|m| m.message == t)
                .unwrap()
                .late_by(600_000)
        };
        assert_eq!(by("on time"), None);
        assert_eq!(by("held up"), Some(900_000));
    }

    #[test]
    fn untrusted_arrivals_dont_set_arrival_times() {
        let mut c = db();
        let e = msg("a", "1", "N0AAA", "x", T0);
        apply(&mut c, &e, T0);
        let item = ridgewarden_client::core::FeedItem {
            seq: 0,
            event: e,
            trusted: false,
            seen_ms: Some(T0 + 9_000_000),
        };
        note_seen(&c, &[item]).unwrap();
        assert_eq!(newest(&c, 1, None).unwrap()[0].seen_ms, None);
    }

    #[test]
    fn the_stock_client_gets_messages_escaped() {
        let mut c = db();
        apply(
            &mut c,
            &msg("a", "1", "N0AAA", "<img src=x onerror=alert(1)> & co", T0),
            T0,
        );
        let j = newest(&c, 1, None).unwrap()[0].api_json();
        assert_eq!(j["message"], "&lt;img src=x onerror=alert(1)&gt; &amp; co");
    }

    #[test]
    fn hidden_messages_and_files_disappear_and_bump_the_version() {
        let mut c = db();
        let m = msg("a", "1", "N0AAA", "spam", T0);
        apply(&mut c, &m, T0);
        let root = "cd".repeat(32);
        let f = ev(
            "a",
            FILE,
            json!({"root": root, "name": "junk.bin", "size": 3}),
            T0,
        );
        apply(&mut c, &f, T0);
        let v = version(&c).unwrap();
        assert_eq!(
            find_event(&c, &m.id[..10]).unwrap(),
            std::slice::from_ref(&m.id)
        );
        assert!(find_event(&c, "abc").unwrap().is_empty());
        apply(
            &mut c,
            &ev("b", HIDE, json!({"event_id": m.id}), T0 + 1),
            T0 + 1,
        );
        apply(
            &mut c,
            &ev("b", HIDE, json!({"event_id": f.id}), T0 + 2),
            T0 + 2,
        );
        assert!(version(&c).unwrap() > v);
        assert_eq!(count(&c, None).unwrap(), 0);
        assert!(channels(&c).unwrap().is_empty());
        assert!(files(&c).unwrap().is_empty());
        assert_eq!(deleted_roots(&c).unwrap(), [root]);
    }

    #[test]
    fn a_file_shared_again_after_deleting_comes_back() {
        let mut c = db();
        let root = "ef".repeat(32);
        let share = |ts| {
            ev(
                "a",
                FILE,
                json!({"root": root, "name": "plan.txt", "size": 4}),
                ts,
            )
        };
        apply(&mut c, &share(T0), T0);
        apply(
            &mut c,
            &ev("a", FILE_GONE, json!({"root": root}), T0 + 10),
            T0 + 10,
        );
        assert!(files(&c).unwrap().is_empty());
        apply(&mut c, &share(T0 + 20), T0 + 20);
        assert_eq!(files(&c).unwrap().len(), 1);
        assert!(deleted_roots(&c).unwrap().is_empty());
    }

    #[test]
    fn absurd_file_sizes_are_ignored() {
        let mut c = db();
        for (i, size) in [u64::MAX, i64::MAX as u64, MAX_SANE_SIZE + 1]
            .iter()
            .enumerate()
        {
            let root = format!("{i:0>64}");
            apply(
                &mut c,
                &ev(
                    "a",
                    FILE,
                    json!({"root": root, "name": "big.iso", "size": size}),
                    T0,
                ),
                T0,
            );
        }
        assert!(files(&c).unwrap().is_empty());
        assert_eq!(used_bytes(&c, &format!("{:0>64}", "a")).unwrap(), 0);
    }

    #[test]
    fn fetching_stops_at_the_total_and_skips_own_and_oversized_files() {
        let mut c = db();
        let me = format!("{:0>64}", "a");
        for (i, (site, size)) in [("a", 10), ("b", 20), ("b", 5000)].iter().enumerate() {
            let root = format!("{i:0>64}");
            apply(
                &mut c,
                &ev(
                    site,
                    FILE,
                    json!({"root": root, "name": format!("f{i}"), "size": size}),
                    T0 + i as i64,
                ),
                T0,
            );
        }
        let todo = unwanted(&c, &me, 1000).unwrap();
        assert_eq!(todo, [(format!("{:0>64}", 1), 20)]);
        assert_eq!(held_bytes(&c, &me).unwrap(), 10);
        mark_wanted(&c, &todo[0].0).unwrap();
        assert_eq!(held_bytes(&c, &me).unwrap(), 30);
    }

    #[test]
    fn only_mesh_hosts_are_accepted_for_site_addresses() {
        let none: &[String] = &[];
        for ok in [
            "http://tst-hilltop-pi.local.mesh:8799",
            "http://tst-hilltop-pi:8799/",
            "http://10.1.2.3:8799",
            "http://44.0.0.9",
        ] {
            assert!(mesh_url(ok, none), "{ok}");
        }
        for bad in [
            "http://example.com",
            "http://8.8.8.8:8799",
            "http://localhost:8799",
            "http://2130706433/",
            "http://10.1.2.3@evil.example/",
            "http://[::1]:8799",
        ] {
            assert!(!mesh_url(bad, none), "{bad}");
        }
        assert!(mesh_url(
            "http://127.0.0.1:9163",
            &["127.0.0.1".to_string()]
        ));
    }

    #[test]
    fn site_urls_must_be_plain_http() {
        assert!(valid_url("http://tst-hilltop-pi.local.mesh:8799"));
        assert!(!valid_url("javascript:alert(1)"));
        assert!(!valid_url("http://x\" onmouseover=1"));
        assert!(!valid_url("http://a b"));
        assert!(!valid_url("http://"));
    }

    #[test]
    fn channel_and_file_names_cant_carry_markup() {
        assert_eq!(
            clean_channel(" net (Sat) #2 ").as_deref(),
            Some("net (Sat) #2")
        );
        assert_eq!(clean_channel("<b>"), None);
        assert_eq!(clean_file_name("x<svg onload=1>.txt"), None);
        assert_eq!(
            clean_file_name("ICS-213 form (v2).pdf").as_deref(),
            Some("ICS-213 form (v2).pdf")
        );
    }

    #[test]
    fn file_names_lose_paths_and_header_breakers() {
        assert_eq!(
            clean_file_name("C:\\Users\\x\\plan.pdf").as_deref(),
            Some("plan.pdf")
        );
        assert_eq!(
            clean_file_name("../../etc/passwd").as_deref(),
            Some("passwd")
        );
        assert_eq!(clean_file_name(".hidden"), None);
        assert_eq!(clean_file_name("a\"b"), None);
        assert_eq!(clean_file_name(""), None);
    }

    #[test]
    fn late_heartbeats_dont_overwrite_newer_ones() {
        let mut c = db();
        let beat = |url: &str, users: Value, ts| {
            ev(
                "b",
                SITE,
                json!({"node": "tst-b", "url": url, "users": users}),
                ts,
            )
        };
        apply(
            &mut c,
            &beat(
                "http://new",
                json!([{"call_sign": "N0NEW", "id": "x1", "epoch": T0 / 1000}]),
                T0,
            ),
            T0,
        );
        apply(
            &mut c,
            &beat(
                "http://old",
                json!([{"call_sign": "N0OLD", "id": "x2", "epoch": 1}]),
                T0 - 60_000,
            ),
            T0 - 60_000,
        );
        let url: String = c
            .query_row("SELECT url FROM sites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(url, "http://new");
        let u = users(&c, "me", "tst-a", T0 / 1000 - 600).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].call_sign, "N0NEW");
        assert_eq!(u[0].node, "tst-b");
    }

    #[test]
    fn local_logins_show_with_this_sites_node_and_own_echo_is_skipped() {
        let mut c = db();
        heartbeat(&c, "N0LOC", "abc12", T0 / 1000).unwrap();
        let me = format!("{:0>64}", "a");
        apply(
            &mut c,
            &ev(
                "a",
                SITE,
                json!({"node": "tst-a", "users": [{"call_sign": "N0LOC", "id": "abc12", "epoch": T0 / 1000}]}),
                T0,
            ),
            T0,
        );
        let u = users(&c, &me, "tst-a", T0 / 1000 - 600).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].node, "tst-a");
    }

    #[test]
    fn a_native_message_replaces_a_bridged_copy_that_got_here_first() {
        let mut c = db();
        let bridged = ev(
            "b",
            MSG,
            json!({"id": "1a2b3c4d", "call_sign": "N0AAA", "message": "hello\\there",
                   "bridge": {"from": "tst-mc", "epoch": T0 / 1000}}),
            T0 + 5000,
        );
        apply(&mut c, &bridged, T0 + 5000);
        // the original says "hello\there" with a real backslash, which MeshChat mangled
        apply(
            &mut c,
            &msg("a", "1a2b3c4d", "N0AAA", "hello\\there", T0),
            T0,
        );
        let got = newest(&c, 10, None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].bridged, "");
        // and a bridged copy arriving after the original is dropped
        let late = ev(
            "c",
            MSG,
            json!({"id": "1a2b3c4d", "call_sign": "N0AAA", "message": "hello\there",
                   "bridge": {"from": "tst-mc", "epoch": T0 / 1000}}),
            T0 + 9000,
        );
        apply(&mut c, &late, T0 + 9000);
        assert_eq!(newest(&c, 10, None).unwrap().len(), 1);
    }

    #[test]
    fn invented_logins_are_capped() {
        let c = db();
        for i in 0..(MAX_LOCAL_USERS + 100) {
            heartbeat(&c, "N0X", &format!("id{i}"), T0 / 1000 + i as i64).unwrap();
        }
        let n: i64 = c
            .query_row("SELECT count(*) FROM users WHERE site_key = ''", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, MAX_LOCAL_USERS as i64);
        // the newest stay
        assert!(known_user(&c, "N0X", &format!("id{}", MAX_LOCAL_USERS + 99)).unwrap());
        assert!(!known_user(&c, "N0X", "id0").unwrap());
        assert_eq!(users(&c, "me", "tst-a", 0).unwrap().len(), MAX_LOCAL_USERS);
    }

    #[test]
    fn a_heartbeat_dated_years_ahead_doesnt_freeze_a_site() {
        let mut c = db();
        let beat = |url: &str, sent: i64| ev("b", SITE, json!({"node": "tst-b", "url": url}), sent);
        // claims ten years from now, but this core got it at T0
        apply(
            &mut c,
            &beat("http://tst-old.local.mesh", T0 + 10 * 365 * 86_400_000),
            T0,
        );
        apply(
            &mut c,
            &beat("http://tst-new.local.mesh", T0 + 60_000),
            T0 + 60_000,
        );
        let url: String = c
            .query_row("SELECT url FROM sites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(url, "http://tst-new.local.mesh");
    }

    #[test]
    fn a_clock_step_moves_recorded_arrivals() {
        let mut c = db();
        let e = ev(
            "b",
            SITE,
            json!({"node": "tst-b", "run": "r", "mono_ms": 5}),
            T0,
        );
        apply(&mut c, &e, T0);
        let item = ridgewarden_client::core::FeedItem {
            seq: 0,
            event: e,
            trusted: true,
            seen_ms: Some(T0),
        };
        note_seen(&c, &[item]).unwrap();
        shift_seen(&c, 3_600_000).unwrap();
        let seen: i64 = c
            .query_row("SELECT seen_ms FROM beats", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, T0 + 3_600_000);
    }
}
