//! The chat, files and sites pages, the directory, the MeshChat API and the stock client.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Component, Path as FsPath};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, now_ms, short_time};

use crate::App;
use crate::board;
use crate::chat::{self, Msg};
use crate::files;
use crate::limit::{Peer, addr};
use crate::meshchat::{self, multipart, parse_form};
use crate::sites::{self, Site, State as SiteState};

const PER_PAGE: usize = 100;

pub fn router(app: Arc<App>) -> Router {
    let limit = app.cfg.max_file_bytes() as usize + 64 * 1024;
    Router::new()
        .route("/", get(chat_page))
        .route("/post", post(post_message))
        .route("/files", get(files_page))
        .route("/files/upload", post(upload_post))
        .route("/files/delete", post(delete_post))
        .route("/file/{root}", get(file_get))
        .route("/sites", get(sites_page))
        .route("/board", get(board_page).post(board_post))
        .route("/api/services", get(services_json))
        .route("/go", get(go))
        .route("/go/meshchat", get(go_meshchat))
        .route("/api/sites", get(sites_json))
        .route("/cgi-bin/meshchat", get(meshchat::cgi).post(meshchat::cgi))
        .route(
            "/meshchat",
            get(|| async { Redirect::permanent("/meshchat/") }),
        )
        .route("/meshchat/", get(client_index))
        .route("/meshchat/{*path}", get(client_file))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(limit))
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Whether a request's Host is one this app should answer to: an address, a mesh name,
/// this app's own public name, or one listed in `allow_hosts`. Anything else is a web
/// page elsewhere pointing its own domain at this box (DNS rebinding).
fn host_ok(app: &App, host: &str) -> bool {
    let h = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    }
    .to_ascii_lowercase();
    let public = app
        .cfg
        .public_url
        .split_once("://")
        .and_then(|(_, r)| r.split(['/', ':']).next())
        .unwrap_or("");
    h.parse::<std::net::IpAddr>().is_ok()
        || h == "localhost"
        || h.ends_with(".local.mesh")
        || ridgewarden_client::util::valid_node(&h)
        || h.eq_ignore_ascii_case(public)
        || app
            .cfg
            .allow_hosts
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&h))
}

/// Refuse requests for other hosts, and tell browsers not to guess content types or run
/// scripts from anywhere else.
async fn guard(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host.is_empty() && !host_ok(&app, host) {
        return (
            StatusCode::MISDIRECTED_REQUEST,
            "This app answers only to mesh names and addresses.",
        )
            .into_response();
    }
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(
                "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'",
            ),
        );
    }
    r
}

struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

/// Percent-encode a query value.
pub fn enc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            let _ = write!(o, "%{b:02X}");
        }
    }
    o
}

fn page(app: &App, active: &str, title: &str, body: &str) -> Html<String> {
    let (_, here) = app.me();
    let who = if here.is_empty() {
        "core not reached yet".to_string()
    } else {
        format!("{} ({})", app.cfg.label(&here), app.cfg.zone)
    };
    let page = Page {
        app: "ridgewarden-failover",
        nav: &[
            ("/", "Chat"),
            ("/board", "Board"),
            ("/files", "Files"),
            ("/sites", "Sites"),
        ],
        active,
        who: &who,
        // a reload would wipe a half-typed message, so only the sites page refreshes
        refresh: active == "/sites",
    };
    // the shared CSS leaves links at the browser's blue, unreadable on the dark theme
    Html(page.render(
        title,
        &format!("<style>main a{{color:inherit}}</style>{body}"),
    ))
}

/// A site's label, or its call sign while its node isn't confirmed yet.
fn site_name(app: &App, s: &Site) -> String {
    if s.node.is_empty() {
        s.call.clone()
    } else {
        app.cfg.label(&s.node)
    }
}

fn names(app: &App, list: &[&Site]) -> String {
    list.iter()
        .map(|s| esc(&site_name(app, s)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the person reading this needs to know about the split, if there is one.
fn split_notes(app: &App, all: &[Site], now: i64) -> String {
    let mut b = String::new();
    if !app.here_ok(now) {
        let since = app
            .core_last_ok()
            .map(|t| format!(" since {}", short_time(t)))
            .unwrap_or_default();
        let _ = write!(
            b,
            "<div class=banner>This site's ridgewarden core hasn't answered{since}. You can read \
             everything that reached this site, but posts can't be sent from here until it's back. \
             <a href=\"/go\">Go to a site that's up</a>.</div>"
        );
    }
    let side: Vec<&Site> = all.iter().filter(|s| s.state <= SiteState::Up).collect();
    let cut: Vec<&Site> = all
        .iter()
        .filter(|s| s.state == SiteState::CutOff)
        .collect();
    let silent: Vec<&Site> = all
        .iter()
        .filter(|s| s.state == SiteState::Silent)
        .collect();
    if !cut.is_empty() {
        let _ = write!(
            b,
            "<div class=banner><b>The mesh is split.</b> This side has {}. Cut off from here: {}. \
             Both sides keep working; messages posted on either side are kept and will merge when \
             the link returns. <a href=\"/sites\">Details</a></div>",
            if side.is_empty() {
                "no chat sites".to_string()
            } else {
                names(app, &side)
            },
            names(app, &cut)
        );
    }
    if !silent.is_empty() {
        let _ = write!(
            b,
            "<div class=banner>{} {} gone quiet though the mesh path is up, so its Pi or core has \
             probably stopped. Everything posted there before that is here, and anything posted \
             there since will arrive when it's back. <a href=\"/sites\">Details</a></div>",
            names(app, &silent),
            if silent.len() == 1 { "has" } else { "have" }
        );
    }
    b
}

fn cookie<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(name)?.strip_prefix('='))
}

fn site_of(app: &App, m: &Msg) -> String {
    if !m.bridged.is_empty() {
        let at = if m.node.is_empty() {
            &m.bridged
        } else {
            &m.node
        };
        return format!(
            "MeshChat on {}, bridged by {}",
            app.cfg.label(at),
            m.author_call
        );
    }
    if m.node.is_empty() {
        m.author_call.clone()
    } else {
        format!("{} ({})", app.cfg.label(&m.node), m.author_call)
    }
}

fn render_msg(app: &App, m: &Msg, b: &mut String) {
    let late = m
        .late_by(app.cfg.late_after_s as i64 * 1000)
        .map(|lag| {
            format!(
                " <span class=\"pill yellow\" title=\"posted {}\">arrived {} late: sent across a split</span>",
                esc(&short_time(m.posted_ms)),
                esc(&human_duration(lag))
            )
        })
        .unwrap_or_default();
    let channel = if m.channel.is_empty() {
        String::new()
    } else {
        format!(" <span class=\"mute small\">#{}</span>", esc(&m.channel))
    };
    let _ = write!(
        b,
        "<tr><td class=\"small mute\" style=\"white-space:nowrap\">{}</td><td><b>{}</b>{channel}<br>\
         <span class=\"mute small\">via {}, id {}</span>{late}</td><td>{}</td></tr>",
        esc(&short_time(m.ts)),
        esc(&m.call_sign),
        esc(&site_of(app, m)),
        &m.event_id[..m.event_id.len().min(12)],
        esc(&m.message).replace('\n', "<br>")
    );
}

fn post_form(app: &App, call: &str, channels: &[String], current: Option<&str>) -> String {
    let mut opts = String::new();
    for c in channels {
        let sel = if Some(c.as_str()) == current {
            " selected"
        } else {
            ""
        };
        let label = if c.is_empty() { "Everything" } else { c };
        let _ = write!(
            opts,
            "<option value=\"{}\"{sel}>{}</option>",
            esc(c),
            esc(label)
        );
    }
    if !channels.iter().any(|c| c.is_empty()) {
        opts.insert_str(0, "<option value=\"\">Everything</option>");
    }
    format!(
        "<form method=post action=\"/post\" class=noprint style=\"margin:8px 0 16px\">\
         <input type=hidden name=token value=\"{}\">\
         <p><label>Call sign <input name=call_sign value=\"{}\" maxlength=20 required size=10></label> \
         <label>Channel <select name=channel>{opts}</select></label> \
         <label>or new <input name=new_channel maxlength=40 size=10></label></p>\
         <p><textarea name=message maxlength={} rows=3 required style=\"width:100%\" \
         aria-label=Message></textarea></p><p><button type=submit>Send</button> \
         <span class=\"mute small\">Your call sign is whatever you type; it isn't checked. Each \
         message shows the site that signed and sent it.</span></p></form>",
        app.form_token,
        esc(call),
        chat::MAX_MESSAGE
    )
}

async fn chat_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let channel = q.get("channel").map(String::as_str);
    let pg: usize = q
        .get("page")
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
        .min(1_000_000);
    let all = app.sites(now)?;
    let (msgs, total, channels) = {
        let db = app.db();
        (
            chat::page(&db, channel, pg * PER_PAGE, PER_PAGE)?,
            chat::count(&db, channel)?,
            chat::channels(&db)?,
        )
    };
    let mut b = String::from("<h1>Chat</h1>");
    b.push_str(&split_notes(&app, &all, now));
    let call = cookie(&headers, "meshchat_call_sign").and_then(chat::clean_call);
    b.push_str(&post_form(
        &app,
        call.as_deref().unwrap_or(""),
        &channels,
        channel,
    ));
    b.push_str("<p class=small>Channel: ");
    let on = |c: Option<&str>| if c == channel { " class=on" } else { "" };
    let _ = write!(b, "<a href=\"/\"{}>all</a>", on(None));
    for c in channels.iter().filter(|c| !c.is_empty()) {
        let _ = write!(
            b,
            " | <a href=\"/?channel={}\"{}>#{}</a>",
            enc(c),
            on(Some(c)),
            esc(c)
        );
    }
    let _ = write!(
        b,
        " <span class=mute>{total} messages, newest first, every site's merged. \
         <a href=\"/{}\">Refresh</a></span></p>",
        channel
            .map(|c| format!("?channel={}", enc(c)))
            .unwrap_or_default()
    );
    if msgs.is_empty() {
        b.push_str(if total == 0 {
            "<p class=mute>No messages yet. Anything posted here or at another site will show up here.</p>"
        } else {
            "<p class=mute>No messages on this page.</p>"
        });
    } else {
        b.push_str("<div class=wrap><table><tr><th>Time</th><th>From</th><th>Message</th></tr>");
        for m in &msgs {
            render_msg(&app, m, &mut b);
        }
        b.push_str("</table></div>");
    }
    let base = channel
        .map(|c| format!("/?channel={}&page=", enc(c)))
        .unwrap_or_else(|| "/?page=".into());
    b.push_str("<p>");
    if pg > 0 {
        let _ = write!(b, "<a href=\"{base}{}\">Newer</a> ", pg - 1);
    }
    if ((pg + 1) * PER_PAGE) < total as usize {
        let _ = write!(b, "<a href=\"{base}{}\">Older</a>", pg + 1);
    }
    b.push_str("</p>");
    Ok(page(&app, "/", "Chat", &b))
}

fn message_page(app: &App, code: StatusCode, title: &str, text: &str, back: &str) -> Response {
    let b = format!(
        "<h1>{}</h1><p>{}</p><p><a href=\"{}\">Back</a></p>",
        esc(title),
        esc(text),
        esc(back)
    );
    (code, page(app, "", title, &b)).into_response()
}

fn form_field<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
    f.iter().find(|(n, _)| n == k).map_or("", |(_, v)| v)
}

fn too_many(app: &App, back: &str) -> Response {
    message_page(
        app,
        StatusCode::TOO_MANY_REQUESTS,
        "Slow down",
        "Too many posts from this address; wait a minute and try again.",
        back,
    )
}

async fn post_message(State(app): State<Arc<App>>, peer: Peer, body: Bytes) -> Response {
    let f = parse_form(&body);
    let back = "/";
    if form_field(&f, "token") != app.form_token {
        return message_page(
            &app,
            StatusCode::FORBIDDEN,
            "Not sent",
            "This form is out of date (the app restarted). Go back, reload and send again.",
            back,
        );
    }
    if !app.limiter.take(addr(&peer), 1.0) {
        return too_many(&app, back);
    }
    let Some(call) = chat::clean_call(form_field(&f, "call_sign")) else {
        return message_page(
            &app,
            StatusCode::BAD_REQUEST,
            "Not sent",
            "Call signs are up to 20 letters, digits and / - _ .",
            back,
        );
    };
    let new = form_field(&f, "new_channel").trim();
    let chosen = if new.is_empty() {
        form_field(&f, "channel")
    } else {
        new
    };
    let (Some(channel), Some(text)) = (
        chat::clean_channel(chosen),
        chat::clean_message(form_field(&f, "message")),
    ) else {
        return message_page(
            &app,
            StatusCode::BAD_REQUEST,
            "Not sent",
            "The message must be 1 to 1000 characters, and channel names up to 40 plain characters.",
            back,
        );
    };
    let id = match getrandom::u32() {
        Ok(r) => format!("{r:08x}"),
        Err(e) => return Fail(e.into()).into_response(),
    };
    if let Err(e) = app.post(&call, &id, &channel, &text).await {
        tracing::warn!("posting: {e:#}");
        return message_page(
            &app,
            StatusCode::SERVICE_UNAVAILABLE,
            "Not sent",
            "This site's ridgewarden core didn't take the message. If this site has lost its \
             core, use /go to reach one that's up.",
            back,
        );
    }
    let to = if channel.is_empty() {
        "/".to_string()
    } else {
        format!("/?channel={}", enc(&channel))
    };
    (
        [(
            header::SET_COOKIE,
            format!("meshchat_call_sign={call}; Path=/; SameSite=Lax"),
        )],
        Redirect::to(&to),
    )
        .into_response()
}

async fn files_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let all = app.sites(now)?;
    let (me, _) = app.me();
    let (list, used, copies) = {
        let db = app.db();
        (
            chat::files(&db)?,
            chat::used_bytes(&db, &me)?,
            chat::copies(&db)?,
        )
    };
    let site_count = all.len().max(1);
    let mut b = String::from("<h1>Files</h1>");
    b.push_str(&split_notes(&app, &all, now));
    let _ = write!(
        b,
        "<p class=\"mute small\">Every site keeps a copy of every file, so files stay available when the \
         site that shared them is down. {} of {} used; {} per file at most.</p>",
        size_text(used),
        size_text(app.cfg.max_total_bytes() as i64),
        size_text(app.cfg.max_file_bytes() as i64)
    );
    if app.cfg.files.enabled {
        let _ = write!(
            b,
            "<form method=post action=\"/files/upload\" enctype=\"multipart/form-data\" class=noprint>\
             <input type=hidden name=token value=\"{}\"><p><label>Call sign <input name=call_sign \
             maxlength=20 size=10></label> <input type=file name=uploadfile required aria-label=File> \
             <button type=submit>Share</button></p></form>",
            app.form_token
        );
    } else {
        b.push_str("<p class=mute>File sharing is turned off on this site.</p>");
    }
    if list.is_empty() {
        b.push_str("<p class=mute>No shared files yet.</p>");
        return Ok(page(&app, "/files", "Files", &b));
    }
    b.push_str(
        "<div class=wrap><table><tr><th>File</th><th>Size</th><th>Shared</th><th>From</th><th>Here</th>\
         <th>Copies</th><th></th></tr>",
    );
    for f in &list {
        let here = if app.cfg.cache_dir().join(&f.root).is_file() || f.author_key == me {
            pill(Color::Green, "copy here")
        } else if f.wanted {
            pill(Color::Yellow, "copying")
        } else {
            pill(Color::Unknown, "waiting")
        };
        let n = copies.get(&f.root).copied().unwrap_or(1);
        let copies = format!("at {n} of {site_count} sites");
        let del = if f.author_key == me {
            format!(
                "<form method=post action=\"/files/delete\"><input type=hidden name=token value=\"{}\">\
                 <input type=hidden name=root value=\"{}\"><button type=submit>Delete</button></form>",
                app.form_token, f.root
            )
        } else {
            String::new()
        };
        let by = if f.call_sign.is_empty() {
            String::new()
        } else {
            format!("{}, ", esc(&f.call_sign))
        };
        let _ = write!(
            b,
            "<tr><td><a href=\"/file/{}\">{}</a></td><td>{}</td><td>{}</td><td>{by}{}<br>\
             <span class=\"mute small\">id {}</span></td><td>{here}</td><td>{copies}</td><td>{del}</td></tr>",
            f.root,
            esc(&f.name),
            size_text(f.size),
            esc(&short_time(f.ts)),
            esc(&app.cfg.label(&f.node)),
            &f.event_id[..f.event_id.len().min(12)]
        );
    }
    b.push_str("</table></div>");
    Ok(page(&app, "/files", "Files", &b))
}

fn size_text(n: i64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    }
}

async fn upload_post(
    State(app): State<Arc<App>>,
    peer: Peer,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let parts = multipart(ctype, &body);
    let field = |k: &str| {
        parts
            .iter()
            .find(|p| p.name == k)
            .map(|p| String::from_utf8_lossy(p.data).into_owned())
            .unwrap_or_default()
    };
    if field("token") != app.form_token {
        return message_page(
            &app,
            StatusCode::FORBIDDEN,
            "Not shared",
            "This form is out of date (the app restarted). Go back, reload and try again.",
            "/files",
        );
    }
    if !app.limiter.take(addr(&peer), 5.0) {
        return too_many(&app, "/files");
    }
    let Some(file) = parts.iter().find(|p| p.name == "uploadfile") else {
        return message_page(
            &app,
            StatusCode::BAD_REQUEST,
            "Not shared",
            "No file was chosen.",
            "/files",
        );
    };
    let call = chat::clean_call(&field("call_sign")).unwrap_or_default();
    let name = file.filename.as_deref().unwrap_or("");
    match files::upload(&app, name, file.data, &call).await {
        Ok(()) => Redirect::to("/files").into_response(),
        Err(e) => message_page(
            &app,
            StatusCode::BAD_REQUEST,
            "Not shared",
            &files::user_text(&e),
            "/files",
        ),
    }
}

async fn delete_post(State(app): State<Arc<App>>, peer: Peer, body: Bytes) -> Response {
    let f = parse_form(&body);
    if form_field(&f, "token") != app.form_token {
        return message_page(
            &app,
            StatusCode::FORBIDDEN,
            "Not deleted",
            "This form is out of date (the app restarted). Go back, reload and try again.",
            "/files",
        );
    }
    if !app.limiter.take(addr(&peer), 1.0) {
        return too_many(&app, "/files");
    }
    let found = chat::file_by_root(&app.db(), form_field(&f, "root"));
    let file = match found {
        Ok(Some(file)) => file,
        Ok(None) => {
            return message_page(
                &app,
                StatusCode::NOT_FOUND,
                "Not deleted",
                "No such file.",
                "/files",
            );
        }
        Err(e) => return Fail(e).into_response(),
    };
    match files::delete(&app, &file).await {
        Ok(()) => Redirect::to("/files").into_response(),
        Err(e) => message_page(
            &app,
            StatusCode::BAD_REQUEST,
            "Not deleted",
            &files::user_text(&e),
            "/files",
        ),
    }
}

async fn file_get(State(app): State<Arc<App>>, Path(root): Path<String>) -> Result<Response, Fail> {
    let found = chat::valid_root(&root)
        .then(|| chat::file_by_root(&app.db(), &root.to_ascii_lowercase()))
        .transpose()?
        .flatten();
    match found {
        Some(f) => Ok(meshchat::serve_file(&app, &f).await?),
        None => Ok(message_page(
            &app,
            StatusCode::NOT_FOUND,
            "No such file",
            "Nobody has shared a file with that id, or it was deleted.",
            "/files",
        )),
    }
}

fn state_color(s: SiteState) -> Color {
    match s {
        SiteState::Here | SiteState::Up => Color::Green,
        SiteState::Silent => Color::Yellow,
        SiteState::CutOff => Color::Red,
    }
}

async fn sites_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let all = app.sites(now)?;
    let mut b = String::from("<h1>Sites</h1>");
    b.push_str(&split_notes(&app, &all, now));
    b.push_str(
        "<p class=\"mute small\">Every site running this app serves the whole chat. A site is up while \
         its chat app's heartbeats keep arriving (judged by when they arrived here, not by its clock); a quiet site is judged by the mesh's own link \
         data, so a slow hop doesn't make it look dead. Bookmark <a href=\"/go\">/go</a> on any site: it \
         sends you to this site while it works, otherwise to the nearest site that's up.</p>",
    );
    if all.is_empty() {
        b.push_str("<p class=mute>No sites yet. This site appears once its core answers.</p>");
        return Ok(page(&app, "/sites", "Sites", &b));
    }
    b.push_str(
        "<div class=wrap><table><tr><th>State</th><th>Site</th><th>Last heard</th><th>Mesh path</th>\
         <th>Why</th><th>Address</th></tr>",
    );
    for s in &all {
        let path = match (s.hops, s.weakest) {
            (Some(0), _) => "here".to_string(),
            (Some(h), Some(q)) => format!("{h} hops, weakest link {q:.0}%"),
            (Some(h), None) => format!("{h} hops"),
            (None, _) => "no path known".into(),
        };
        let url = if s.url.is_empty() {
            "<span class=mute>not set</span>".to_string()
        } else {
            format!("<a href=\"{0}/\">{0}</a>", esc(&s.url))
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}<br><span class=\"mute small\">{}</span></td><td>{}</td><td>{}</td>\
             <td>{}</td><td>{url}</td></tr>",
            pill(state_color(s.state), s.state.label()),
            esc(&site_name(&app, s)),
            esc(&s.call),
            s.last_heard
                .map(|t| esc(&short_time(t)))
                .unwrap_or_else(|| "never".into()),
            esc(&path),
            esc(&why_with_clock(s))
        );
    }
    b.push_str("</table></div>");
    home_section(&app, &all, now, &mut b)?;
    services_section(&app, &mut b)?;
    drills_section(&app, &mut b)?;
    bridge_section(&app, &mut b)?;
    Ok(page(&app, "/sites", "Sites", &b))
}

fn label_key(app: &App, all: &[Site], key: &str) -> String {
    all.iter()
        .find(|s| s.key == key)
        .map(|s| site_name(app, s))
        .unwrap_or_else(|| format!("a site no longer heard from ({})", &key[..key.len().min(8)]))
}

fn home_section(app: &App, all: &[Site], now: i64, b: &mut String) -> Result<(), Fail> {
    let home = app.place(now)?;
    b.push_str("<h2>Home site</h2><p class=\"mute small\">Jobs that must run in one place (the MeshChat \
                bridge, and /go when <code>directory = \"home\"</code>) run on the home site. It is \
                the up site that reaches the most others, then has the steadiest power, then the \
                highest operator weight. If home goes down the next best takes over at once; a better \
                site takes it back only after being up without a break for the failback time.</p>");
    match home {
        Some(h) => {
            let _ = write!(
                b,
                "<p>{} since {}.</p>",
                esc(&label_key(app, all, &h.key)),
                esc(&short_time(h.since))
            );
        }
        None => b.push_str(
            "<p class=mute>No site can be home yet: none is up with a weight above 0.</p>",
        ),
    }
    let log = crate::placement::log(&app.db(), 5)?;
    if !log.is_empty() {
        b.push_str("<ul class=\"plain small\">");
        for (ts, key, why) in log {
            let _ = write!(
                b,
                "<li>{}: {} ({})</li>",
                esc(&short_time(ts)),
                esc(&label_key(app, all, &key)),
                esc(&why)
            );
        }
        b.push_str("</ul>");
    }
    Ok(())
}

fn services_section(app: &App, b: &mut String) -> Result<(), Fail> {
    b.push_str(
        "<h2>Services and how they merge</h2><div class=wrap><table><tr><th>Service</th><th>Kind</th>\
         <th>Runs on</th><th>After a split</th><th>Needs a look</th></tr>",
    );
    for (s, open) in crate::services::rows(&app.db())? {
        let open = if open.is_empty() {
            "<span class=mute>nothing</span>".to_string()
        } else {
            open.iter().map(|o| esc(o)).collect::<Vec<_>>().join("<br>")
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=small>{}</td><td class=small>{open}</td></tr>",
            esc(s.name),
            esc(s.merge.label()),
            esc(s.runs.label()),
            esc(s.rejoin)
        );
    }
    b.push_str("</table></div>");
    Ok(())
}

fn drills_section(app: &App, b: &mut String) -> Result<(), Fail> {
    let drills = chat::drills(&app.db())?;
    b.push_str("<h2>Drills</h2>");
    if drills.is_empty() {
        b.push_str(
            "<p class=mute>No drills yet. <code>ridgewarden-failover drill --kill &lt;node&gt;</code> \
             takes one site down on purpose and checks nothing is lost; the result shows here at \
             every site.</p>",
        );
        return Ok(());
    }
    let ms = |v: &serde_json::Value| v.as_i64();
    let passed = drills
        .iter()
        .filter(|d| {
            d.result["error"].is_null()
                && ms(&d.result["lost"]) == Some(0)
                && ms(&d.result["survive_ms"]).is_some_and(|t| t <= crate::drill::SURVIVE_LIMIT_MS)
        })
        .count();
    let lost: i64 = drills.iter().filter_map(|d| ms(&d.result["lost"])).sum();
    let _ = write!(
        b,
        "<p>{} drill{}, {passed} passed, {lost} message{} lost in all. The spec's goal is 20 drills \
         with none lost, and every survivor taking posts within 60 s.</p>",
        drills.len(),
        if drills.len() == 1 { "" } else { "s" },
        if lost == 1 { "" } else { "s" }
    );
    b.push_str(
        "<div class=wrap><table><tr><th>When</th><th>Took down</th><th>Survivors took posts</th>\
         <th>Merged</th><th>Lost</th><th>Run from</th><th>Stopped early</th></tr>",
    );
    let secs = |v: &serde_json::Value| {
        v.as_i64().map_or("not reached".to_string(), |t| {
            format!("{:.0} s", t as f64 / 1000.0)
        })
    };
    for d in drills.iter().take(10) {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} of {}</td><td>{}</td><td>{}</td></tr>",
            esc(&short_time(d.ts)),
            esc(&app.cfg.label(d.result["target"].as_str().unwrap_or(""))),
            secs(&d.result["survive_ms"]),
            secs(&d.result["merge_ms"]),
            d.result["lost"],
            d.result["sent"],
            esc(&app.cfg.label(&d.node)),
            esc(d.result["error"].as_str().unwrap_or(""))
        );
    }
    b.push_str("</table></div>");
    Ok(())
}

fn bridge_section(app: &App, b: &mut String) -> Result<(), Fail> {
    if app.cfg.meshchat.peers.is_empty() {
        return Ok(());
    }
    b.push_str(
        "<h2>MeshChat nodes</h2><p class=\"mute small\">Only the home site exchanges messages with \
         these; this table shows this site's own attempts.</p><div class=wrap><table><tr><th>Node</th>\
         <th>Last exchange</th><th>Problem</th></tr>",
    );
    let states = crate::bridge::states(&app.db())?;
    for peer in &app.cfg.meshchat.peers {
        let st = states.iter().find(|s| &s.peer == peer);
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(peer),
            st.and_then(|s| s.last_ok)
                .map_or("never".to_string(), |t| esc(&short_time(t))),
            esc(st.map_or("", |s| s.error.as_str()))
        );
    }
    b.push_str("</table></div>");
    Ok(())
}

async fn services_json(State(app): State<Arc<App>>) -> Result<Json<serde_json::Value>, Fail> {
    Ok(Json(crate::services::json(&app.db())?))
}

fn board_form(app: &App, call: &str, topic: Option<&str>, text: &str, prev: &[String]) -> String {
    let topic_field = match topic {
        Some(t) => format!("<input type=hidden name=topic value=\"{}\">", esc(t)),
        None => {
            "<p><label>Entry <input name=topic maxlength=40 required size=20></label></p>".into()
        }
    };
    format!(
        "<form method=post action=\"/board\" class=noprint><input type=hidden name=token value=\"{}\">\
         <input type=hidden name=prev value=\"{}\">{topic_field}\
         <p><textarea name=text maxlength={} rows=2 style=\"width:100%\" aria-label=Text>{}</textarea></p>\
         <p><label>Call sign <input name=call_sign value=\"{}\" maxlength=20 required size=10></label> \
         <button type=submit>{}</button></p></form>",
        app.form_token,
        prev.join(","),
        board::MAX_TEXT,
        esc(text),
        esc(call),
        if topic.is_some() { "Update" } else { "Add" }
    )
}

fn version_line(app: &App, v: &board::Version) -> String {
    let at = if v.node.is_empty() {
        v.author_call.clone()
    } else {
        app.cfg.label(&v.node)
    };
    format!(
        "{} at {}, {}, id {}",
        esc(&v.call_sign),
        esc(&at),
        esc(&short_time(v.ts)),
        &v.event_id[..12]
    )
}

async fn board_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let all = app.sites(now)?;
    let entries = board::entries(&app.db())?;
    let call = cookie(&headers, "meshchat_call_sign")
        .and_then(chat::clean_call)
        .unwrap_or_default();
    let mut b = String::from("<h1>Status board</h1>");
    b.push_str(&split_notes(&app, &all, now));
    b.push_str(
        "<p class=\"mute small\">The latest word on each topic, kept at every site. If two people \
         update the same entry on different sides of a split, the newer one shows and the other is \
         listed under it until someone writes an update that takes both into account. Clear an \
         entry by saving it empty.</p>",
    );
    if entries.is_empty() {
        b.push_str("<p class=mute>Nothing on the board yet.</p>");
    }
    for e in &entries {
        let _ = write!(
            b,
            "<h2>{}</h2><p>{}</p><p class=\"mute small\">{}</p>",
            esc(&e.topic),
            if e.shown.text.is_empty() {
                "<span class=mute>(cleared)</span>".to_string()
            } else {
                esc(&e.shown.text).replace('\n', "<br>")
            },
            version_line(&app, &e.shown)
        );
        if !e.conflicts.is_empty() {
            b.push_str(
                "<div class=banner>Also written on the other side of a split:<ul class=plain>",
            );
            for v in &e.conflicts {
                let _ = write!(
                    b,
                    "<li>{} <span class=\"mute small\">{}</span></li>",
                    esc(&v.text).replace('\n', "<br>"),
                    version_line(&app, v)
                );
            }
            b.push_str("</ul>Update the entry to settle it.</div>");
        }
        b.push_str(&board_form(
            &app,
            &call,
            Some(&e.topic),
            &e.shown.text,
            &e.heads(),
        ));
    }
    b.push_str("<h2>New entry</h2>");
    b.push_str(&board_form(&app, &call, None, "", &[]));
    Ok(page(&app, "/board", "Status board", &b))
}

async fn board_post(State(app): State<Arc<App>>, peer: Peer, body: Bytes) -> Response {
    let f = parse_form(&body);
    let back = "/board";
    if form_field(&f, "token") != app.form_token {
        return message_page(
            &app,
            StatusCode::FORBIDDEN,
            "Not saved",
            "This form is out of date (the app restarted). Go back, reload and try again.",
            back,
        );
    }
    if !app.limiter.take(addr(&peer), 1.0) {
        return too_many(&app, back);
    }
    let (Some(call), Some(topic), Some(text)) = (
        chat::clean_call(form_field(&f, "call_sign")),
        board::clean_topic(form_field(&f, "topic")),
        board::clean_text(form_field(&f, "text")),
    ) else {
        return message_page(
            &app,
            StatusCode::BAD_REQUEST,
            "Not saved",
            "Entries need a call sign, a topic of up to 40 letters, digits and spaces, and up to \
             500 characters of text.",
            back,
        );
    };
    let mut prev: Vec<String> = form_field(&f, "prev")
        .split(',')
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .take(16)
        .map(String::from)
        .collect();
    // a "new" entry whose topic already exists updates it
    if prev.is_empty() {
        match board::heads(&app.db(), &topic) {
            Ok(h) => prev = h,
            Err(e) => return Fail(e).into_response(),
        }
    }
    if let Err(e) = app.edit_board(&call, &topic, &text, &prev).await {
        tracing::warn!("board edit: {e:#}");
        return message_page(
            &app,
            StatusCode::SERVICE_UNAVAILABLE,
            "Not saved",
            "This site's ridgewarden core didn't take the update. If this site has lost its core, \
             use /go to reach one that's up.",
            back,
        );
    }
    (
        [(
            header::SET_COOKIE,
            format!("meshchat_call_sign={call}; Path=/; SameSite=Lax"),
        )],
        Redirect::to(back),
    )
        .into_response()
}

fn why_with_clock(s: &Site) -> String {
    match s.clock_err {
        Some(err) if err.abs() > sites::CLOCK_WARN_MS => format!(
            "{}; its clock is about {} {}",
            s.why,
            human_duration(err.abs()),
            if err > 0 { "ahead" } else { "behind" }
        ),
        _ => s.why.clone(),
    }
}

async fn sites_json(State(app): State<Arc<App>>) -> Result<Json<serde_json::Value>, Fail> {
    let now = now_ms();
    let all = app.sites(now)?;
    let v: Vec<_> = all
        .iter()
        .map(|s| {
            json!({"key": s.key, "call": s.call, "node": s.node, "url": s.url,
                   "state": s.state.label(), "last_heard": s.last_heard, "hops": s.hops,
                   "weakest": s.weakest, "why": s.why, "clock_err_ms": s.clock_err,
                   "power": s.power, "weight": s.weight})
        })
        .collect();
    let home = app.place(now)?.map(|h| h.key);
    let drills: Vec<_> = chat::drills(&app.db())?
        .into_iter()
        .map(|d| json!({"ts": d.ts, "node": d.node, "result": d.result}))
        .collect();
    Ok(Json(
        json!({"generated": now, "here_ok": app.here_ok(now), "home": home,
                   "sites": v, "drills": drills}),
    ))
}

/// The directory: this site while its core works (or the home site, with
/// `directory = "home"`), else the nearest site that's up.
async fn directory(app: &App, path: &str) -> Result<Response, Fail> {
    let now = now_ms();
    let all = app.sites(now)?;
    if app.cfg.directory == "home"
        && let Some(h) = app.place(now)?
        && let Some(s) = all
            .iter()
            .find(|s| s.key == h.key && s.state == SiteState::Up && !s.url.is_empty())
    {
        return Ok(Redirect::to(&format!("{}{path}", s.url.trim_end_matches('/'))).into_response());
    }
    if app.here_ok(now) {
        return Ok(Redirect::to(path).into_response());
    }
    if let Some(s) = sites::nearest_up(&all) {
        return Ok(Redirect::to(&format!("{}{path}", s.url.trim_end_matches('/'))).into_response());
    }
    let mut b = String::from(
        "<h1>No chat site reachable</h1><p>This site's core isn't answering and no other site has been \
         heard from recently. Try the addresses below, or come back in a few minutes.</p><ul class=plain>",
    );
    for s in all.iter().filter(|s| !s.url.is_empty()) {
        let _ = write!(
            b,
            "<li><a href=\"{0}{1}\">{2}</a> {3}</li>",
            esc(s.url.trim_end_matches('/')),
            esc(path),
            esc(&site_name(app, s)),
            pill(state_color(s.state), s.state.label())
        );
    }
    b.push_str("</ul>");
    Ok((
        StatusCode::SERVICE_UNAVAILABLE,
        page(app, "", "Directory", &b),
    )
        .into_response())
}

async fn go(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    directory(&app, "/").await
}

async fn go_meshchat(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    directory(&app, "/meshchat/").await
}

fn content_type(path: &FsPath) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "mp3" => "audio/mpeg",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A file from the stock MeshChat client folder; without one, people get this app's page.
async fn serve_client(app: &App, rel: &str) -> Response {
    let Some(dir) = &app.cfg.client_dir else {
        return Redirect::to("/").into_response();
    };
    let rel = FsPath::new(rel);
    let plain =
        |c: Component| matches!(c, Component::Normal(n) if !n.to_string_lossy().starts_with('.'));
    if !rel.components().all(plain) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = dir.join(rel);
    match tokio::fs::read(&path).await {
        // the client builds its pages from what other stations sent; scripts only from here
        Ok(data) => (
            [
                (header::CONTENT_TYPE, content_type(&path)),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:",
                ),
            ],
            data,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn client_index(State(app): State<Arc<App>>) -> Response {
    serve_client(&app, "index.html").await
}

async fn client_file(State(app): State<Arc<App>>, Path(path): Path<String>) -> Response {
    serve_client(&app, &path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(enc("a b&c/#"), "a%20b%26c%2F%23");
    }

    #[test]
    fn call_sign_cookie_is_found_among_others() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "meshchat_id=Ab3xZ; meshchat_call_sign=N0ABC"
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie(&h, "meshchat_call_sign"), Some("N0ABC"));
        assert_eq!(cookie(&h, "nope"), None);
    }
}
