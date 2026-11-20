//! MeshChat's HTTP API, `/cgi-bin/meshchat`, so the stock MeshChat web client works
//! against this app unchanged.
//!
//! Shapes follow the Lua MeshChat (hickey/meshchat 2.x, kn6plv/meshchat 2.18): `action`
//! and its parameters come from the query string or a urlencoded body, answers are always
//! HTTP 200, and failures are `{"status":500,"response":"..."}`. The client polls
//! `messages_version_ui` every 15 s and fetches `messages` only when the version changes.
//! Node-to-node sync actions (`messages_raw` and friends) aren't served: sites sync
//! through their ridgewarden cores instead.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use ridgewarden_client::util::now_ms;

use crate::App;
use crate::chat;
use crate::files::{self, Fetch};
use crate::limit::{Peer, addr};
use crate::sites::State as SiteState;

/// Messages in a "download messages" file at most; the chat page pages through the rest.
const MAX_DOWNLOAD: usize = 20_000;

/// Users heard from within this long are listed; the client hides those older than 240 s.
const USERS_WINDOW_S: i64 = 600;

/// Decode `application/x-www-form-urlencoded` (or a query string) into pairs.
pub fn parse_form(s: &[u8]) -> Vec<(String, String)> {
    s.split(|b| *b == b'&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let mut it = kv.splitn(2, |b| *b == b'=');
            let k = decode(it.next().unwrap_or_default());
            let v = decode(it.next().unwrap_or_default());
            (k, v)
        })
        .collect()
}

fn decode(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < s.len() => {
                match std::str::from_utf8(&s[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct Part<'a> {
    pub name: String,
    pub filename: Option<String>,
    pub data: &'a [u8],
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// The parts of a `multipart/form-data` body. Small on purpose: uploads are capped at a
/// few MB, and only plain form fields and one file come in.
pub fn multipart<'a>(content_type: &str, body: &'a [u8]) -> Vec<Part<'a>> {
    let Some(boundary) = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| b.trim_matches('"'))
        // RFC 2046 caps a boundary at 70 characters; a longer one only makes the search slow
        .filter(|b| !b.is_empty() && b.len() <= 70)
    else {
        return vec![];
    };
    let delim = format!("--{boundary}");
    let mut out = vec![];
    let mut at = match find(body, delim.as_bytes(), 0) {
        Some(p) => p + delim.len(),
        None => return out,
    };
    while let Some(next) = find(body, format!("\r\n{delim}").as_bytes(), at) {
        let part = &body[at..next];
        at = next + 2 + delim.len();
        let Some(part) = part.strip_prefix(b"\r\n") else {
            break;
        };
        let Some(split) = find(part, b"\r\n\r\n", 0) else {
            continue;
        };
        let head = String::from_utf8_lossy(&part[..split]);
        let (mut name, mut filename) = (None, None);
        for line in head.lines() {
            if !line
                .to_ascii_lowercase()
                .starts_with("content-disposition:")
            {
                continue;
            }
            for p in line.split(';').map(str::trim) {
                if let Some(v) = p.strip_prefix("name=") {
                    name = Some(v.trim_matches('"').to_string());
                } else if let Some(v) = p.strip_prefix("filename=") {
                    filename = Some(v.trim_matches('"').to_string());
                }
            }
        }
        if let Some(name) = name {
            out.push(Part {
                name,
                filename,
                data: &part[split + 4..],
            });
        }
    }
    out
}

struct Params(Vec<(String, String)>);

impl Params {
    fn get(&self, k: &str) -> &str {
        self.0
            .iter()
            .find(|(n, _)| n == k)
            .map_or("", |(_, v)| v.as_str())
    }
}

fn ok() -> Response {
    Json(json!({"status": 200, "response": "OK"})).into_response()
}

fn refuse(why: impl Into<String>) -> Response {
    Json(json!({"status": 500, "response": why.into()})).into_response()
}

fn text(body: impl Into<String>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

/// jQuery marks its same-origin requests with this header, and another site's page
/// can't add it without a CORS preflight this app never answers. That keeps a stranger's
/// web page from posting or deleting through a visitor's browser, without changing the
/// stock client.
fn from_client(h: &HeaderMap) -> bool {
    h.get("x-requested-with")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
}

/// The host the client reached us on, which it uses to build download links.
fn host(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::HOST)?.to_str().ok()?;
    let ok = !v.is_empty()
        && v.len() <= 100
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b));
    ok.then(|| v.to_string())
}

/// Whether a request that changes something may go ahead; the refusal if not.
fn may_write(app: &App, h: &HeaderMap, peer: &Peer, cost: f64) -> Option<Response> {
    if !from_client(h) {
        return Some(refuse(
            "Refused: requests that change things must come from the MeshChat page",
        ));
    }
    if !app.limiter.take(addr(peer), cost) {
        return Some(refuse(
            "Too many posts from this address; wait a minute and try again",
        ));
    }
    None
}

pub async fn cgi(
    State(app): State<Arc<App>>,
    peer: Peer,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Response {
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut p = parse_form(q.unwrap_or_default().as_bytes());
    if ctype.starts_with("application/x-www-form-urlencoded") {
        p.extend(parse_form(&body));
    }
    let p = Params(p);
    let action = p.get("action");
    let cost = match action {
        "send_message" | "delete_file" => Some(1.0),
        "upload_file" => Some(5.0),
        _ => None,
    };
    if let Some(no) = cost.and_then(|c| may_write(&app, &headers, &peer, c)) {
        return no;
    }
    let r = match action {
        "config" => config(&app).await,
        "messages_version_ui" => version_ui(&app, &p, &peer),
        "messages" => messages(&app, &p, &peer),
        "send_message" => send(&app, &p).await,
        "users" => users(&app),
        "files" => file_list(&app, &headers),
        "file_download" => download(&app, &p).await,
        "upload_file" => upload(&app, &ctype, &body).await,
        "delete_file" => delete(&app, &p).await,
        "sync_status" => sync_status(&app),
        "action_log" => Ok(Json(json!([])).into_response()),
        "messages_download" => messages_download(&app),
        _ => Ok(text("error no action")),
    };
    r.unwrap_or_else(|e| {
        tracing::error!("meshchat {action}: {e:#}");
        refuse("This site hit an error; its log has the details")
    })
}

type R = anyhow::Result<Response>;

async fn config(app: &App) -> R {
    let node = app.whoami().await.map(|w| w.node).unwrap_or_default();
    Ok(Json(json!({
        // the client shows "Mesh Chat v<version>"
        "version": concat!(env!("CARGO_PKG_VERSION"), " (ridgewarden-failover)"),
        "protocol_verison": "1.02",
        "node": node,
        "zone": app.cfg.zone,
        "default_channel": "",
        "debug": 0,
        "platform": "pi",
    }))
    .into_response())
}

/// The client's poll doubles as its "I'm here" heartbeat. A login this site hasn't seen
/// before counts against the address's posting limit, so nobody can invent thousands.
fn note_user(app: &App, p: &Params, peer: &Peer) -> anyhow::Result<()> {
    let id = p.get("id");
    let id_ok = !id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_alphanumeric());
    let Some(call) = chat::clean_call(p.get("call_sign")).filter(|_| id_ok) else {
        return Ok(());
    };
    let known = chat::known_user(&app.db(), &call, id)?;
    if known || app.limiter.take(addr(peer), 1.0) {
        chat::heartbeat(&app.db(), &call, id, now_ms() / 1000)?;
    }
    Ok(())
}

fn version_ui(app: &App, p: &Params, peer: &Peer) -> R {
    note_user(app, p, peer)?;
    let v = chat::version(&app.db())?;
    Ok(Json(json!({"messages_version": v})).into_response())
}

fn messages(app: &App, p: &Params, peer: &Peer) -> R {
    note_user(app, p, peer)?;
    let since = p.get("since").parse().ok();
    let list: Vec<Value> = chat::newest(&app.db(), app.cfg.messages_per_fetch, since)?
        .iter()
        .map(chat::Msg::api_json)
        .collect();
    Ok(Json(list).into_response())
}

async fn send(app: &App, p: &Params) -> R {
    let Some(call) = chat::clean_call(p.get("call_sign")) else {
        return Ok(refuse("Call sign missing or not valid"));
    };
    let Some(text) = chat::clean_message(p.get("message")) else {
        return Ok(refuse(format!(
            "Message is empty or longer than {} characters",
            chat::MAX_MESSAGE
        )));
    };
    let Some(channel) = chat::clean_channel(p.get("channel")) else {
        return Ok(refuse("Channel name is too long or has odd characters"));
    };
    let id = match p.get("id") {
        id if chat::valid_msg_id(id) => id.to_string(),
        _ => format!("{:08x}", getrandom::u32()?),
    };
    match app.post(&call, &id, &channel, &text).await {
        Ok(()) => Ok(ok()),
        Err(e) => {
            tracing::warn!("posting: {e:#}");
            Ok(refuse(
                "Not sent: this site's ridgewarden core didn't take it. Open /go to reach a site that's up.",
            ))
        }
    }
}

fn users(app: &App) -> R {
    let (me, here) = app.me();
    let list: Vec<Value> = chat::users(&app.db(), &me, &here, now_ms() / 1000 - USERS_WINDOW_S)?
        .into_iter()
        .map(|u| {
            json!({"epoch": u.epoch, "id": u.id, "call_sign": u.call_sign,
                   "node": u.node, "platform": "pi"})
        })
        .collect();
    Ok(Json(list).into_response())
}

fn file_list(app: &App, h: &HeaderMap) -> R {
    let (me, here) = app.me();
    // every site has every file, so downloads come from wherever the client is now
    let node = host(h).unwrap_or(here);
    let db = app.db();
    let used = chat::used_bytes(&db, &me)?;
    let allowed = app.cfg.max_total_bytes() as i64;
    let list: Vec<Value> = chat::files_by_name(&db)?
        .into_iter()
        .map(|f| {
            json!({"file": f.name, "epoch": f.ts / 1000, "size": f.size, "node": node,
                   "platform": "pi", "local": i32::from(f.author_key == me)})
        })
        .collect();
    Ok(Json(json!({
        "stats": {
            "total": allowed,
            "used": used,
            "files": list.len(),
            "files_free": (allowed - used).max(0),
            "allowed": allowed,
        },
        "files": list,
    }))
    .into_response())
}

async fn download(app: &App, p: &Params) -> R {
    let f = match p.get("root") {
        "" => chat::file_named(&app.db(), p.get("file"))?,
        root => chat::file_by_root(&app.db(), root)?,
    };
    let Some(f) = f else {
        return Ok(text("no file"));
    };
    serve_file(app, &f).await
}

/// The file itself, or a plain explanation while it's still on its way here.
pub async fn serve_file(app: &App, f: &chat::SharedFile) -> R {
    match files::local_copy(app, f).await? {
        Fetch::Ready(path) => {
            let data = tokio::fs::read(&path).await?;
            let disp = format!("attachment; filename=\"{}\"", f.name);
            Ok((
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    ),
                    (header::CONTENT_DISPOSITION, HeaderValue::from_str(&disp)?),
                ],
                data,
            )
                .into_response())
        }
        Fetch::Pending(why) => Ok((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "60")],
            text(why),
        )
            .into_response()),
    }
}

async fn upload(app: &App, ctype: &str, body: &[u8]) -> R {
    let parts = multipart(ctype, body);
    let Some(file) = parts.iter().find(|p| p.name == "uploadfile") else {
        return Ok(refuse("No file in the upload"));
    };
    let name = file.filename.as_deref().unwrap_or("");
    match files::upload(app, name, file.data, "").await {
        Ok(()) => Ok(ok()),
        Err(e) => Ok(refuse(files::user_text(&e))),
    }
}

async fn delete(app: &App, p: &Params) -> R {
    let Some(f) = chat::file_named(&app.db(), p.get("file"))? else {
        return Ok(text("no file"));
    };
    match files::delete(app, &f).await {
        Ok(()) => Ok(ok()),
        Err(e) => Ok(refuse(files::user_text(&e))),
    }
}

/// When each other site was last heard from, newest first: MeshChat's "last sync" list.
fn sync_status(app: &App) -> R {
    let all = app.sites(now_ms())?;
    let mut list: Vec<(i64, String)> = all
        .into_iter()
        .filter(|s| s.state != SiteState::Here)
        .filter_map(|s| Some((s.last_heard? / 1000, app.cfg.label(&s.node))))
        .collect();
    list.sort_by(|a, b| b.cmp(a));
    let v: Vec<Value> = list
        .into_iter()
        .map(|(epoch, node)| json!({"node": node, "epoch": epoch}))
        .collect();
    Ok(Json(v).into_response())
}

/// Every message in MeshChat's own database format, oldest first, for the "download
/// messages" button.
fn messages_download(app: &App) -> R {
    let db = app.db();
    let mut all = chat::newest(&db, MAX_DOWNLOAD, None)?;
    all.reverse();
    let mut out = String::new();
    for m in all {
        let esc = crate::bridge::escape(&m.message);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\tpi\t{}\n",
            m.id,
            m.ts / 1000,
            esc,
            m.call_sign,
            m.node,
            m.channel
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=messages.txt",
            ),
        ],
        out,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_decoding_handles_plus_percent_and_junk() {
        let p = parse_form(
            b"action=send_message&message=hi+there%21%0Aok&call_sign=n0abc&bad=%zz&=x&flag",
        );
        assert_eq!(p[0], ("action".into(), "send_message".into()));
        assert_eq!(p[1].1, "hi there!\nok");
        assert_eq!(p[3].1, "%zz");
        assert_eq!(p[5], ("flag".into(), String::new()));
        assert_eq!(decode(b"100%"), "100%");
        assert_eq!(decode(b"%4"), "%4");
    }

    #[test]
    fn multipart_finds_the_file_and_its_bytes() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"token\"\r\n\r\nabc\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"uploadfile\"; filename=\"net plan.txt\"\r\n\
Content-Type: text/plain\r\n\r\nline1\r\n--not the end\r\n--XyZ--\r\n";
        let parts = multipart("multipart/form-data; boundary=XyZ", body);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "token");
        assert_eq!(parts[0].data, b"abc");
        assert_eq!(parts[1].filename.as_deref(), Some("net plan.txt"));
        assert_eq!(parts[1].data, b"line1\r\n--not the end");
        assert!(multipart("multipart/form-data", body).is_empty());
    }

    #[test]
    fn only_jquery_requests_may_change_things() {
        let mut h = HeaderMap::new();
        assert!(!from_client(&h));
        h.insert(
            "x-requested-with",
            HeaderValue::from_static("XMLHttpRequest"),
        );
        assert!(from_client(&h));
    }

    #[test]
    fn host_header_is_checked_before_it_goes_into_links() {
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("tst-hilltop-pi.local.mesh:8799"),
        );
        assert_eq!(host(&h).as_deref(), Some("tst-hilltop-pi.local.mesh:8799"));
        h.insert(header::HOST, HeaderValue::from_static("evil\"x"));
        assert_eq!(host(&h), None);
    }
}
