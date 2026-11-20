//! A bridge to MeshChat nodes, so a mesh where some sites still run MeshChat works as
//! one chat.
//!
//! It speaks MeshChat's own HTTP API, the same calls MeshChat's `meshchatsync` makes:
//! `messages_version` to see whether anything changed, `messages_raw` (checked against
//! its `Content-MD5`) to copy their messages in, and `send_message` to hand ours to them.
//! MeshChat nodes then pass messages among themselves as usual. It runs only on the
//! placement's home site, so each message crosses once; message ids, which both sides
//! keep, stop anything coming back around.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use ridgewarden_client::util::{norm_node, now_ms, valid_node};

use crate::App;
use crate::chat::{self, Msg};
use crate::md5;

/// Messages copied in per node per round, and per hour. Everything copied in is kept
/// for good at every site, so a node gone wrong mustn't be able to flood the log; the
/// first contact's 500 come in over ten rounds.
const MAX_IN: usize = 50;
const MAX_IN_HOUR: i64 = 600;
/// The most of a node's answer read; MeshChat's 500 messages fit easily.
const MAX_BODY: usize = 2 << 20;
/// Messages sent per node per round, so a long outage drains steadily.
const MAX_OUT: usize = 50;
/// Only messages this recent are sent on to MeshChat (see `push`).
const PUSH_WINDOW_MS: i64 = 3_600_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS bridge (
    peer    TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    -- our messages up to this sequence number have been sent to the node
    pushed  INTEGER NOT NULL,
    last_ok INTEGER,
    error   TEXT NOT NULL,
    -- messages copied in since in_since, for the hourly cap
    in_since INTEGER NOT NULL DEFAULT 0,
    in_count INTEGER NOT NULL DEFAULT 0
);";

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("ridgewarden-failover/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// One MeshChat database line: `id, epoch, message, call_sign, node, platform, channel`,
/// with newlines and quotes in the message escaped. Old nodes leave the channel off.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub id: String,
    pub epoch: i64,
    pub message: String,
    pub call_sign: String,
    pub node: String,
    pub channel: String,
}

pub fn parse_raw(body: &str) -> Vec<Line> {
    body.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 6 {
                return None;
            }
            let message = unescape_html(&f[2].replace("\\n", "\n").replace("\\\"", "\""));
            Some(Line {
                id: f[0].to_ascii_lowercase(),
                epoch: f[1].trim().parse().ok()?,
                message: chat::clean_message(&message)?,
                call_sign: chat::clean_call(f[3])?,
                node: f[4].to_string(),
                channel: chat::clean_channel(f.get(6).copied().unwrap_or(""))?,
            })
        })
        .filter(|l| chat::valid_msg_id(&l.id))
        .collect()
}

/// What this site sends is HTML-escaped, because the MeshChat client shows messages as
/// HTML; undo that for what comes back.
fn unescape_html(m: &str) -> String {
    m.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// MeshChat's own escaping, for its database format.
pub fn escape(m: &str) -> String {
    m.replace('\n', "\\n")
        .replace('"', "\\\"")
        .replace('\t', " ")
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeerState {
    pub peer: String,
    pub last_ok: Option<i64>,
    pub error: String,
}

pub fn states(conn: &Connection) -> Result<Vec<PeerState>> {
    let v = conn
        .prepare("SELECT peer, last_ok, error FROM bridge ORDER BY peer")?
        .query_map([], |r| {
            Ok(PeerState {
                peer: r.get(0)?,
                last_ok: r.get(1)?,
                error: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

fn cgi(peer: &str) -> String {
    format!("{}/cgi-bin/meshchat", peer.trim_end_matches('/'))
}

async fn get_text(http: &reqwest::Client, url: &str) -> Result<(String, Option<String>)> {
    let r = http.get(url).send().await?.error_for_status()?;
    let md5 = r
        .headers()
        .get("content-md5")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    let mut r = r;
    let mut body = vec![];
    while let Some(chunk) = r.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY {
            bail!("{url} sent more than {MAX_BODY} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    if let Some(want) = &md5
        && md5::hex(&body) != *want
    {
        bail!("{url} sent a body that doesn't match its Content-MD5");
    }
    Ok((String::from_utf8_lossy(&body).into_owned(), md5))
}

/// The node's own name, which its messages are filed under here.
async fn node_name(http: &reqwest::Client, peer: &str) -> String {
    let name = async {
        let v: serde_json::Value = http
            .get(format!("{}?action=config", cgi(peer)))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        Some(norm_node(v.get("node")?.as_str()?))
    };
    name.await
        .filter(|n| valid_node(n))
        .unwrap_or_else(|| "meshchat".into())
}

/// How many more messages may come in from `peer` this hour.
fn import_budget(app: &App, peer: &str, now: i64) -> Result<i64> {
    let (since, count): (i64, i64) = app.db().query_row(
        "SELECT in_since, in_count FROM bridge WHERE peer = ?1",
        [peer],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if now - since >= 3_600_000 {
        app.db().execute(
            "UPDATE bridge SET in_since = ?2, in_count = 0 WHERE peer = ?1",
            params![peer, now],
        )?;
        return Ok(MAX_IN_HOUR);
    }
    Ok(MAX_IN_HOUR - count)
}

/// Copy in what the node has that this mesh doesn't. A line with the id and call sign
/// of a message already here is that message, even if its text came back changed.
/// Returns how many came in.
async fn pull(app: &App, http: &reqwest::Client, peer: &str, lines: &[Line]) -> Result<usize> {
    let mut new = vec![];
    for l in lines {
        if !chat::has_message(&app.db(), &l.id, &l.call_sign)? {
            new.push(l.clone());
        }
    }
    if new.is_empty() {
        return Ok(0);
    }
    let budget = import_budget(app, peer, now_ms())?.clamp(0, MAX_IN as i64) as usize;
    if budget < new.len() {
        tracing::info!(
            "MeshChat {peer}: {} new messages, copying {budget} this round",
            new.len()
        );
    }
    new.sort_by_key(|l| l.epoch);
    let from = node_name(http, peer).await;
    let mut n = 0;
    for l in new.into_iter().take(budget) {
        let e = app
            .core
            .append(
                chat::MSG,
                json!({"id": l.id, "call_sign": l.call_sign, "message": l.message,
                       "channel": l.channel, "node": norm_node(&l.node),
                       "bridge": {"from": from, "epoch": l.epoch}}),
            )
            .await?;
        app.apply_own(&e)?;
        app.db().execute(
            "UPDATE bridge SET in_count = in_count + 1 WHERE peer = ?1",
            [peer],
        )?;
        n += 1;
    }
    Ok(n)
}

async fn send(http: &reqwest::Client, peer: &str, m: &Msg) -> Result<()> {
    use crate::web::enc;
    let body = format!(
        "action=send_message&message={}&call_sign={}&epoch={}&id={}&channel={}",
        // the MeshChat client shows messages as HTML, and nothing upstream escapes them
        enc(&ridgewarden_client::util::esc(&m.message)),
        enc(&m.call_sign),
        m.ts / 1000,
        enc(&m.id),
        enc(&m.channel)
    );
    let r = http
        .post(cgi(peer))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?
        .error_for_status()?;
    let text = r.text().await?;
    // MeshChat answers 200 even when it refuses; its JSON says which
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && v.get("status").and_then(|s| s.as_i64()) == Some(500)
    {
        bail!("refused: {}", v["response"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// Hand the node what reached this site since last time and isn't there already.
/// Only what arrived here in the last hour is sent: when home moves to a site that was
/// home long ago, its old cursor mustn't replay history the node has since trimmed.
/// Returns how many went.
async fn push(app: &App, http: &reqwest::Client, peer: &str, there: &[Line]) -> Result<usize> {
    let after: i64 =
        app.db()
            .query_row("SELECT pushed FROM bridge WHERE peer = ?1", [peer], |r| {
                r.get(0)
            })?;
    let last = chat::last_seq(&app.db())?;
    let todo = chat::to_bridge(&app.db(), after, MAX_OUT)?;
    let complete = todo.len() < MAX_OUT;
    let since = now_ms() - PUSH_WINDOW_MS;
    let mut n = 0;
    let mut done = after;
    for m in todo {
        let present = there
            .iter()
            .any(|l| l.id == m.id && l.call_sign == m.call_sign);
        if !present && m.seen_ms.unwrap_or(m.ts) >= since {
            send(http, peer, &m)
                .await
                .with_context(|| format!("sending a message to {peer}"))?;
            n += 1;
        }
        done = m.seq;
        app.db().execute(
            "UPDATE bridge SET pushed = ?2 WHERE peer = ?1",
            params![peer, done],
        )?;
    }
    // everything else up to `last` is bridged in or hidden; skip past it so an idle round
    // doesn't fetch the node's whole database again
    if complete && last > done {
        app.db().execute(
            "UPDATE bridge SET pushed = ?2 WHERE peer = ?1",
            params![peer, last],
        )?;
    }
    Ok(n)
}

async fn exchange(app: &App, http: &reqwest::Client, peer: &str) -> Result<(usize, usize)> {
    let known: Option<(String, i64)> = app
        .db()
        .query_row(
            "SELECT version, pushed FROM bridge WHERE peer = ?1",
            [peer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let last = chat::last_seq(&app.db())?;
    if known.is_none() {
        // start with the last hour, so what was posted while home was moving here still
        // goes out; anything the node already has is skipped by id
        let start = chat::seq_before(&app.db(), now_ms() - PUSH_WINDOW_MS)?;
        app.db().execute(
            "INSERT INTO bridge (peer, version, pushed, error) VALUES (?1, '', ?2, '')",
            params![peer, start],
        )?;
    }
    let (version, _) = get_text(http, &format!("{}?action=messages_version", cgi(peer))).await?;
    let version = version.trim().to_string();
    let changed = known.as_ref().is_none_or(|(v, _)| *v != version);
    let to_send = known.as_ref().is_some_and(|(_, pushed)| *pushed < last);
    if !changed && !to_send {
        return Ok((0, 0));
    }
    let (body, _) = get_text(http, &format!("{}?action=messages_raw", cgi(peer))).await?;
    let lines = parse_raw(&body);
    let pulled = if changed {
        pull(app, http, peer, &lines).await?
    } else {
        0
    };
    app.db().execute(
        "UPDATE bridge SET version = ?2 WHERE peer = ?1",
        params![peer, version],
    )?;
    let pushed = push(app, http, peer, &lines).await?;
    Ok((pulled, pushed))
}

/// Exchange messages with every configured MeshChat node, if this site is home.
pub async fn round(app: &App, http: &reqwest::Client) -> Result<()> {
    if app.cfg.meshchat.peers.is_empty() || !app.is_home(now_ms())? {
        return Ok(());
    }
    for peer in &app.cfg.meshchat.peers {
        let r = exchange(app, http, peer).await;
        let now = now_ms();
        match &r {
            Ok((pulled, pushed)) => {
                if pulled + pushed > 0 {
                    tracing::info!("MeshChat {peer}: {pulled} in, {pushed} out");
                }
                app.db().execute(
                    "UPDATE bridge SET last_ok = ?2, error = '' WHERE peer = ?1",
                    params![peer, now],
                )?;
            }
            Err(e) => {
                tracing::warn!("MeshChat {peer}: {e:#}");
                app.db().execute(
                    "UPDATE bridge SET error = ?2 WHERE peer = ?1",
                    params![peer, format!("{e:#}").chars().take(200).collect::<String>()],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_lines_parse_both_formats_and_unescape() {
        let body = "1a2b3c4d\t1700000000\tline one\\nsays \\\"hi\\\"\tn0abc\tnode1\tnode\tnet\n\
                    deadbeef\t1700000001\told node, no channel\tN0OLD\tnode2\tpi\n\
                    zzzz\t1\tbad id\tN0X\tn\tpi\t\n\
                    00000001\tnot-a-number\tbad epoch\tN0X\tn\tpi\t\n";
        let l = parse_raw(body);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].message, "line one\nsays \"hi\"");
        assert_eq!(l[0].call_sign, "N0ABC");
        assert_eq!(l[0].channel, "net");
        assert_eq!(l[1].channel, "");
        assert_eq!(escape(&l[0].message), "line one\\nsays \\\"hi\\\"");
    }
}
