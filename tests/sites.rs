//! Two or three sites, each an app in front of a stand-in for its ridgewarden core,
//! driven over HTTP the way the stock MeshChat client drives them. The stand-in core does what the
//! real one's local API does (sign-and-log, feed, blobs); "sync" between them is the set
//! union the real cores converge to, done by hand so the test controls when links are up.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use ridgewarden_client::core::{Feed, FeedItem};
use ridgewarden_client::event::{Author, Event};
use ridgewarden_client::util::{ms_to_rfc3339, now_ms};
use ridgewarden_failover::config::Config;
use ridgewarden_failover::{App, follow, web};

struct Core {
    author: Author,
    node: String,
    data_dir: PathBuf,
    events: Vec<FeedItem>,
    blobs: HashMap<String, Vec<u8>>,
    next: u64,
}

type Shared = Arc<Mutex<Core>>;

async fn feed(State(c): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Json<Feed> {
    let c = c.lock().unwrap();
    let after: i64 = q["after"].parse().unwrap();
    let limit: usize = q["limit"].parse().unwrap();
    Json(Feed {
        instance: "test".into(),
        events: c
            .events
            .iter()
            .filter(|i| i.seq > after)
            .take(limit)
            .cloned()
            .collect(),
    })
}

async fn whoami(State(c): State<Shared>) -> Json<Value> {
    let c = c.lock().unwrap();
    Json(json!({"call": c.author.call, "key": c.author.key, "node": c.node}))
}

fn add(c: &mut Core, e: Event) {
    let seq = c.events.len() as i64 + 1;
    c.events.push(FeedItem {
        seq,
        event: e,
        trusted: true,
        seen_ms: Some(now_ms()),
    });
}

async fn append(State(c): State<Shared>, Json(req): Json<Value>) -> Json<Event> {
    let mut c = c.lock().unwrap();
    c.next += 1;
    let e = Event {
        id: format!("{:0>48}{:016x}", &c.author.key[56..], c.next),
        author: c.author.clone(),
        seq: c.next,
        prev: None,
        lamport: c.next,
        wall_time: ms_to_rfc3339(now_ms()),
        mono_time: 0,
        kind: req["kind"].as_str().unwrap().into(),
        payload: req["payload"].clone(),
        sig: String::new(),
        log: req["telemetry"]
            .as_bool()
            .unwrap()
            .then(|| "telemetry".into()),
    };
    add(&mut c, e.clone());
    Json(e)
}

async fn blob_add(State(c): State<Shared>, Json(req): Json<Value>) -> Json<Value> {
    let mut c = c.lock().unwrap();
    let path = c
        .data_dir
        .join("imports")
        .join(req["file"].as_str().unwrap());
    let data = std::fs::read(path).unwrap();
    let root = format!(
        "{:064x}",
        data.iter().map(|b| *b as u64).sum::<u64>() + data.len() as u64
    );
    MANIFESTS
        .lock()
        .unwrap()
        .insert(root.clone(), data.len() as u64);
    c.blobs.insert(root.clone(), data);
    Json(json!({"root": root}))
}

/// Blob manifests every stand-in core has heard of, as the real cores' sync spreads them.
static MANIFESTS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

async fn blob_status(
    State(c): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    let root = &q["root"];
    let Some(size) = MANIFESTS.lock().unwrap().get(root).copied() else {
        return Err(axum::http::StatusCode::NOT_FOUND);
    };
    let complete = c.lock().unwrap().blobs.contains_key(root);
    Ok(Json(
        json!({"root": root, "name": "x", "size": size, "complete": complete,
                   "wanted": false, "priority": 3, "author": "", "trusted": true}),
    ))
}

async fn blob_want(Json(req): Json<Value>) -> Json<Value> {
    Json(json!({"wanted": req["root"]}))
}

async fn blob_unwant(Json(_): Json<Value>) -> Json<Value> {
    Json(json!({"freed": 0}))
}

async fn blob_export(
    State(c): State<Shared>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let c = c.lock().unwrap();
    let Some(data) = c.blobs.get(req["root"].as_str().unwrap()) else {
        return Err((
            axum::http::StatusCode::CONFLICT,
            "blob is not complete".into(),
        ));
    };
    let dir = c.data_dir.join("exports");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(req["name"].as_str().unwrap());
    std::fs::write(&path, data).unwrap();
    Ok(Json(json!({"exported": path})))
}

async fn serve(r: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    format!("http://{addr}")
}

struct Site {
    core: Shared,
    app: Arc<App>,
    url: String,
    _dir: tempfile::TempDir,
}

async fn site(name: &str) -> Site {
    site_with(name, |_| {}).await
}

async fn site_with(name: &str, tweak: impl FnOnce(&mut Config)) -> Site {
    let dir = tempfile::tempdir().unwrap();
    let core_dir = dir.path().join("core");
    std::fs::create_dir_all(&core_dir).unwrap();
    let core = Arc::new(Mutex::new(Core {
        author: Author {
            call: format!("N0{}", name.to_ascii_uppercase()),
            key: format!("{:a>64}", name),
        },
        node: format!("tst-{name}"),
        data_dir: core_dir.clone(),
        events: vec![],
        blobs: HashMap::new(),
        next: 0,
    }));
    let core_url = serve(
        Router::new()
            .route("/v1/feed", get(feed))
            .route("/v1/whoami", get(whoami))
            .route("/local/v1/append", post(append))
            .route("/local/v1/blob/add", post(blob_add))
            .route("/local/v1/blob/want", post(blob_want))
            .route("/local/v1/blob/unwant", post(blob_unwant))
            .route("/v1/blobs", get(blob_status))
            .route("/local/v1/blob/export", post(blob_export))
            .with_state(core.clone()),
    )
    .await;
    let token = dir.path().join("local.token");
    std::fs::write(&token, "secret").unwrap();
    let mut cfg: Config = toml::from_str(ridgewarden_failover::config::EXAMPLE).unwrap();
    cfg.data_dir = dir.path().join("app");
    cfg.core.url = core_url;
    cfg.core.token_file = token;
    cfg.core.data_dir = core_dir;
    let client = dir.path().join("client");
    std::fs::create_dir_all(&client).unwrap();
    std::fs::write(client.join("index.html"), "<title>Mesh Chat</title>").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "not for the web").unwrap();
    std::fs::write(client.join(".env"), "not for the web").unwrap();
    cfg.client_dir = Some(client);
    tweak(&mut cfg);
    let app = Arc::new(App::new(cfg).unwrap());
    app.whoami().await.unwrap();
    let url = serve(web::router(app.clone())).await;
    Site {
        core,
        app,
        url,
        _dir: dir,
    }
}

/// What the cores do when a link is up: each ends up holding every event.
fn sync(a: &Site, b: &Site) {
    for (from, to) in [(&a.core, &b.core), (&b.core, &a.core)] {
        let theirs: Vec<Event> = from
            .lock()
            .unwrap()
            .events
            .iter()
            .map(|i| i.event.clone())
            .collect();
        let mut to = to.lock().unwrap();
        for e in theirs {
            if !to.events.iter().any(|i| i.event.id == e.id) {
                add(&mut to, e);
            }
        }
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn cgi(s: &Site, query: &str) -> Value {
    http()
        .get(format!("{}/cgi-bin/meshchat?{query}", s.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// `send_message` as the stock client's `$.post` sends it.
async fn send(s: &Site, call: &str, id: &str, text: &str) -> Value {
    let body = format!(
        "action=send_message&message={}&call_sign={call}&epoch={}&id={id}&channel=",
        text.replace(' ', "+"),
        now_ms() / 1000
    );
    http()
        .post(format!("{}/cgi-bin/meshchat", s.url))
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .header("x-requested-with", "XMLHttpRequest")
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn texts(s: &Site) -> Vec<String> {
    follow(&s.app).await.unwrap();
    let v = cgi(s, "action=messages&call_sign=N0X&id=abc12&epoch=1").await;
    v.as_array()
        .unwrap()
        .iter()
        .map(|m| m["message"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn both_sides_of_a_split_keep_chatting_and_merge_with_nothing_lost() {
    let a = site("a").await;
    let b = site("b").await;
    assert_eq!(
        send(&a, "N0AAA", "000000a1", "before from a").await["status"],
        200
    );
    assert_eq!(
        send(&b, "N0BBB", "000000b1", "before from b").await["status"],
        200
    );
    sync(&a, &b);
    assert_eq!(texts(&a).await, texts(&b).await);
    assert_eq!(texts(&a).await.len(), 2);

    // the link drops: both sides keep taking posts
    for i in 0..3 {
        send(&a, "N0AAA", &format!("00000a2{i}"), &format!("split a {i}")).await;
        send(&b, "N0BBB", &format!("00000b2{i}"), &format!("split b {i}")).await;
    }
    assert_eq!(texts(&a).await.len(), 5);
    assert!(!texts(&a).await.contains(&"split b 0".to_string()));

    // the link returns
    sync(&a, &b);
    let merged = texts(&a).await;
    assert_eq!(merged.len(), 8);
    assert_eq!(merged, texts(&b).await);
    for want in ["before from a", "before from b", "split a 2", "split b 2"] {
        assert!(merged.contains(&want.to_string()), "{want} lost");
    }
}

#[tokio::test]
async fn stock_client_calls_get_the_shapes_it_expects() {
    let a = site("a").await;
    let cfg = cgi(&a, "action=config").await;
    assert_eq!(cfg["node"], "tst-a");
    assert_eq!(cfg["default_channel"], "");

    let v0 = cgi(
        &a,
        "action=messages_version_ui&call_sign=n0abc&id=Ab3xZ&epoch=1",
    )
    .await;
    assert_eq!(v0["messages_version"], 0);
    let users = cgi(&a, "action=users&call_sign=N0ABC&id=Ab3xZ").await;
    assert_eq!(users[0]["call_sign"], "N0ABC");
    assert_eq!(users[0]["node"], "tst-a");

    assert_eq!(
        send(&a, "N0ABC", "1a2b3c4d", "hello mesh").await["status"],
        200
    );
    let v1 = cgi(
        &a,
        "action=messages_version_ui&call_sign=N0ABC&id=Ab3xZ&epoch=1",
    )
    .await;
    assert_ne!(v1["messages_version"], v0["messages_version"]);
    let msgs = cgi(&a, "action=messages&call_sign=N0ABC&id=Ab3xZ&epoch=1").await;
    let m = &msgs[0];
    assert_eq!(m["id"], "1a2b3c4d");
    assert_eq!(m["call_sign"], "N0ABC");
    assert_eq!(m["message"], "hello mesh");
    assert_eq!(m["channel"], "");
    assert_eq!(m["node"], "tst-a");
    assert!(m["epoch"].is_i64());

    // a retried send isn't posted twice
    send(&a, "N0ABC", "1a2b3c4d", "hello mesh").await;
    follow(&a.app).await.unwrap();
    assert_eq!(texts(&a).await.len(), 1);

    let bad = send(&a, "N0ABC", "1a2b3c4e", "").await;
    assert_eq!(bad["status"], 500);

    // a form on someone else's web page can't post through a visitor's browser
    let forged: Value = http()
        .post(format!("{}/cgi-bin/meshchat", a.url))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("action=send_message&message=spam&call_sign=N0SPAM&channel=")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(forged["status"], 500);
    assert_eq!(texts(&a).await.len(), 1);
}

async fn upload(s: &Site, name: &str, data: &[u8]) -> Value {
    let mut body = format!(
        "--b0undary\r\nContent-Disposition: form-data; name=\"uploadfile\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n--b0undary--\r\n");
    http()
        .post(format!("{}/cgi-bin/meshchat?action=upload_file", s.url))
        .header("content-type", "multipart/form-data; boundary=b0undary")
        .header("x-requested-with", "XMLHttpRequest")
        .body(body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn shared_files_reach_the_other_site_and_only_the_uploader_can_delete() {
    let a = site("a").await;
    let b = site("b").await;
    let data = b"net control script\nline two\n".to_vec();
    assert_eq!(upload(&a, "script.txt", &data).await["status"], 200);
    let files = cgi(&a, "action=files").await;
    assert_eq!(files["files"][0]["file"], "script.txt");
    assert_eq!(files["files"][0]["local"], 1);
    assert_eq!(files["stats"]["files"], 1);

    sync(&a, &b);
    follow(&b.app).await.unwrap();
    let files = cgi(&b, "action=files").await;
    assert_eq!(files["files"][0]["local"], 0);
    let get = || {
        http()
            .get(format!(
                "{}/cgi-bin/meshchat?action=file_download&file=script.txt",
                b.url
            ))
            .send()
    };
    // b's core hasn't fetched the blob yet
    assert_eq!(get().await.unwrap().status(), 503);
    let blobs = a.core.lock().unwrap().blobs.clone();
    b.core.lock().unwrap().blobs = blobs;
    let r = get().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-disposition"],
        "attachment; filename=\"script.txt\""
    );
    assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());
    // b now says it holds a copy, and a hears about it
    sync(&a, &b);
    follow(&a.app).await.unwrap();
    let copies = ridgewarden_failover::chat::copies(&a.app.db()).unwrap();
    assert_eq!(copies.values().copied().collect::<Vec<_>>(), [2]);

    let refused: Value = http()
        .get(format!(
            "{}/cgi-bin/meshchat?action=delete_file&file=script.txt",
            b.url
        ))
        .header("x-requested-with", "XMLHttpRequest")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(refused["status"], 500);
    let done: Value = http()
        .get(format!(
            "{}/cgi-bin/meshchat?action=delete_file&file=script.txt",
            a.url
        ))
        .header("x-requested-with", "XMLHttpRequest")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["status"], 200);
    sync(&a, &b);
    follow(&b.app).await.unwrap();
    assert_eq!(
        cgi(&b, "action=files").await["files"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn pages_and_directory_answer() {
    let a = site("a").await;
    follow(&a.app).await.unwrap();
    send(&a, "N0ABC", "1a2b3c4d", "<b>not bold</b>").await;
    let page = http()
        .get(&a.url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("&lt;b&gt;not bold&lt;/b&gt;"));
    assert!(page.contains("via tst-a (N0A)"));
    for p in ["/files", "/sites", "/api/sites"] {
        let r = http().get(format!("{}{p}", a.url)).send().await.unwrap();
        assert_eq!(r.status(), 200, "{p}");
    }
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = no_redirect
        .get(format!("{}/go", a.url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/");
}

#[tokio::test]
async fn stock_client_files_are_served_and_nothing_else() {
    let a = site("a").await;
    let r = http()
        .get(format!("{}/meshchat/", a.url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    assert!(r.text().await.unwrap().contains("Mesh Chat"));
    for bad in [
        "/meshchat/.env",
        "/meshchat/..%2Fsecret.txt",
        "/meshchat/%2E%2E/secret.txt",
    ] {
        let r = http().get(format!("{}{bad}", a.url)).send().await.unwrap();
        assert_eq!(r.status(), 404, "{bad}");
    }
}

#[tokio::test]
async fn the_apps_own_forms_need_their_token() {
    let a = site("a").await;
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let form = |path: &str, body: &str| {
        no_redirect
            .post(format!("{}{path}", a.url))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body.to_string())
            .send()
    };
    let r = form("/post", "token=nope&call_sign=N0X&channel=&message=forged")
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let ok = format!(
        "token={}&call_sign=N0X&channel=&message=real",
        a.app.form_token
    );
    assert_eq!(form("/post", &ok).await.unwrap().status(), 303);
    assert_eq!(texts(&a).await, ["real"]);

    upload(&a, "keep.txt", b"keep").await;
    let root = cgi(&a, "action=files").await;
    assert_eq!(root["files"].as_array().unwrap().len(), 1);
    let gone = ridgewarden_failover::chat::files(&a.app.db()).unwrap()[0]
        .root
        .clone();
    let r = form("/files/delete", &format!("token=nope&root={gone}"))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = http()
        .post(format!("{}/files/upload", a.url))
        .header("content-type", "multipart/form-data; boundary=zz")
        .body(
            "--zz\r\nContent-Disposition: form-data; name=\"token\"\r\n\r\nnope\r\n--zz\r\n\
             Content-Disposition: form-data; name=\"uploadfile\"; filename=\"x.txt\"\r\n\r\nx\r\n--zz--\r\n",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(
        cgi(&a, "action=files").await["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn one_address_cant_flood_the_chat() {
    let a = site_with("a", |c| c.posts_per_minute = 12).await;
    let mut refused = 0;
    for i in 0..15 {
        if send(&a, "N0FLD", &format!("{i:08x}"), &format!("flood {i}")).await["status"] == 500 {
            refused += 1;
        }
    }
    assert_eq!(refused, 3, "12 a minute are allowed");
    assert_eq!(texts(&a).await.len(), 12);
}

#[tokio::test]
async fn three_sites_relayed_through_the_middle_lose_nothing() {
    let (a, b, c) = (site("a").await, site("b").await, site("c").await);
    // a and c only reach each other through b
    let relay = |a: &Site, b: &Site, c: &Site| {
        sync(a, b);
        sync(b, c);
        sync(a, b);
    };
    send(&a, "N0AAA", "0000000a", "a before").await;
    send(&c, "N0CCC", "0000000c", "c before").await;
    relay(&a, &b, &c);
    assert_eq!(texts(&a).await.len(), 2);
    // c is cut off; a and b keep going, and so does c on its own
    send(&a, "N0AAA", "0000001a", "a during").await;
    send(&b, "N0BBB", "0000001b", "b during").await;
    send(&c, "N0CCC", "0000001c", "c alone").await;
    sync(&a, &b);
    assert_eq!(texts(&a).await.len(), 4);
    assert_eq!(texts(&c).await.len(), 3);
    relay(&a, &b, &c);
    let all = texts(&a).await;
    assert_eq!(all.len(), 5);
    assert_eq!(all, texts(&b).await);
    assert_eq!(all, texts(&c).await);
}

#[tokio::test]
async fn board_edits_on_both_sides_of_a_split_show_as_a_conflict_until_settled() {
    let a = site("a").await;
    let b = site("b").await;
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let edit = |s: &Site, text: &str, prev: &str| {
        let body = format!(
            "token={}&call_sign=N0ED&topic=Shelter&text={}&prev={prev}",
            s.app.form_token,
            text.replace(' ', "+")
        );
        no_redirect
            .post(format!("{}/board", s.url))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
    };
    assert_eq!(edit(&a, "open 20 beds", "").await.unwrap().status(), 303);
    sync(&a, &b);
    follow(&b.app).await.unwrap();
    let heads = ridgewarden_failover::board::heads(&b.app.db(), "Shelter")
        .unwrap()
        .join(",");
    // split: both sides update what they last saw
    edit(&a, "open 15 beds", &heads).await.unwrap();
    edit(&b, "full", &heads).await.unwrap();
    sync(&a, &b);
    for s in [&a, &b] {
        follow(&s.app).await.unwrap();
        let page = http()
            .get(format!("{}/board", s.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            page.contains("Also written on the other side of a split"),
            "{}",
            s.url
        );
        let open = http()
            .get(format!("{}/api/services", s.url))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        let board = open
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "Status board")
            .unwrap();
        assert_eq!(board["open"].as_array().unwrap().len(), 1);
    }
    // a new entry under the same topic settles it, having seen both
    edit(&a, "full, overflow at the library", "").await.unwrap();
    let e = ridgewarden_failover::board::entries(&a.app.db()).unwrap();
    assert_eq!(e[0].shown.text, "full, overflow at the library");
    assert!(e[0].conflicts.is_empty());
}

/// A stand-in for a MeshChat node: its database lines, served the way MeshChat serves them.
type Node = Arc<Mutex<Vec<String>>>;

async fn mc_cgi(
    State(db): State<Node>,
    Query(q): Query<HashMap<String, String>>,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut p = q.clone();
    p.extend(ridgewarden_failover::meshchat::parse_form(body.as_bytes()));
    let mut db = db.lock().unwrap();
    match p.get("action").map(String::as_str) {
        Some("config") => Json(json!({"node": "TST-MC", "zone": "MeshChat"})).into_response(),
        Some("messages_version") => format!("{}\n", db.len()).into_response(),
        Some("messages_raw") => {
            let body: String = db.iter().map(|l| format!("{l}\n")).collect();
            let md5 = ridgewarden_failover::md5::hex(body.as_bytes());
            ([("Content-MD5", md5)], body).into_response()
        }
        Some("send_message") => {
            let msg = p["message"].replace('\n', "\\n").replace('"', "\\\"");
            db.push(format!(
                "{}\t{}\t{msg}\t{}\tTST-MC\tnode\t{}",
                p["id"], p["epoch"], p["call_sign"], p["channel"]
            ));
            Json(json!({"status": 200, "response": "OK"})).into_response()
        }
        _ => "error no action".into_response(),
    }
}

#[tokio::test]
async fn meshchat_nodes_and_this_mesh_see_each_others_messages_once() {
    let node: Node = Arc::new(Mutex::new(vec![
        "1a2b3c4d\t1700000000\tfrom meshchat\\nline two\tN0MC\tTST-MC\tnode\t".into(),
    ]));
    let node_url = serve(
        Router::new()
            .route("/cgi-bin/meshchat", get(mc_cgi).post(mc_cgi))
            .with_state(node.clone()),
    )
    .await;
    let a = site_with("a", |c| c.meshchat.peers = vec![node_url.clone()]).await;
    follow(&a.app).await.unwrap();
    a.app.beat().await.unwrap();
    send(&a, "N0AAA", "000000a0", "posted before the bridge started").await;
    let round = || ridgewarden_failover::bridge::round(&a.app, &a.app.http);
    round().await.unwrap();
    let here = texts(&a).await;
    assert!(
        here.contains(&"from meshchat\nline two".to_string()),
        "{here:?}"
    );
    // the bridge starts with the last hour, so what was posted while it was getting going goes out
    assert_eq!(node.lock().unwrap().len(), 2);

    send(&a, "N0AAA", "000000a1", "hello <b>meshchat</b>").await;
    send(&a, "N0AAA", "000000a2", "path C:\\new").await;
    round().await.unwrap();
    assert_eq!(node.lock().unwrap().len(), 4);
    // the MeshChat client shows messages as HTML, so they go out escaped
    assert!(node.lock().unwrap()[2].contains("hello &lt;b&gt;meshchat&lt;/b&gt;"));
    node.lock()
        .unwrap()
        .push("5e6f7a8b\t1700000100\tsecond from meshchat\tN0MC\tTST-MC\tnode\t".into());
    round().await.unwrap();
    round().await.unwrap();
    // nothing echoes back and forth, even the message whose backslash MeshChat mangled
    assert_eq!(node.lock().unwrap().len(), 5);
    // this site was home long ago and comes back to it with an old cursor: what the
    // node already has isn't sent again
    a.app
        .db()
        .execute(
            "UPDATE bridge SET pushed = (SELECT seq FROM msgs WHERE message LIKE 'posted before%')",
            [],
        )
        .unwrap();
    round().await.unwrap();
    assert_eq!(node.lock().unwrap().len(), 5);
    let here = texts(&a).await;
    assert_eq!(here.len(), 5, "{here:?}");
    let page = http()
        .get(&a.url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("MeshChat on tst-mc"),
        "bridged messages say where they came from"
    );
}

#[tokio::test]
async fn invented_logins_count_against_the_posting_limit() {
    let a = site_with("a", |c| c.posts_per_minute = 12).await;
    for i in 0..30 {
        cgi(
            &a,
            &format!("action=messages_version_ui&call_sign=N0X{i}&id=abc{i}&epoch=1"),
        )
        .await;
    }
    // a known login polling again costs nothing
    for _ in 0..10 {
        cgi(
            &a,
            "action=messages_version_ui&call_sign=N0X0&id=abc0&epoch=1",
        )
        .await;
    }
    let users = cgi(&a, "action=users").await;
    assert_eq!(users.as_array().unwrap().len(), 12);
}

#[tokio::test]
async fn a_file_shared_with_a_false_size_isnt_fetched_and_doesnt_stop_the_others() {
    let a = site("a").await;
    let b = site("b").await;
    upload(&a, "good.txt", b"good file").await;
    // a station says a 4-byte blob it published is 40 bytes
    let bad = format!("{:064x}", 0xbad);
    MANIFESTS.lock().unwrap().insert(bad.clone(), 4);
    {
        let mut core = a.core.lock().unwrap();
        core.blobs.insert(bad.clone(), b"liar".to_vec());
        core.next += 1;
        let n = core.next;
        let author = core.author.clone();
        add(
            &mut core,
            Event {
                id: format!("{:0>48}{n:016x}", "bad"),
                author,
                seq: n,
                prev: None,
                lamport: n,
                wall_time: ms_to_rfc3339(now_ms()),
                mono_time: 0,
                kind: "failover.file".into(),
                payload: json!({"root": bad, "name": "big.iso", "size": 40}),
                sig: String::new(),
                log: None,
            },
        );
    }
    sync(&a, &b);
    follow(&b.app).await.unwrap();
    b.app.beat().await.unwrap();
    ridgewarden_failover::files::fetch_round(&b.app)
        .await
        .unwrap();
    let blobs = a.core.lock().unwrap().blobs.clone();
    b.core.lock().unwrap().blobs = blobs;
    ridgewarden_failover::files::fetch_round(&b.app)
        .await
        .unwrap();
    let files = ridgewarden_failover::chat::files(&b.app.db()).unwrap();
    let wanted = |name: &str| files.iter().find(|f| f.name == name).unwrap().wanted;
    assert!(wanted("good.txt"));
    assert!(!wanted("big.iso"));
    let copies = ridgewarden_failover::chat::copies(&b.app.db()).unwrap();
    let good = files.iter().find(|f| f.name == "good.txt").unwrap();
    assert_eq!(copies[&good.root], 2, "b fetched the good one and said so");
}

#[tokio::test]
async fn requests_for_other_host_names_are_refused() {
    let a = site("a").await;
    let port = a.url.rsplit(':').next().unwrap();
    let ask = |host: &str| {
        http()
            .get(format!("{}/sites", a.url))
            .header("host", format!("{host}:{port}"))
            .send()
    };
    assert_eq!(ask("attacker.example.com").await.unwrap().status(), 421);
    for ok in [
        "tst-hilltop-pi.local.mesh",
        "tst-hilltop-pi",
        "10.1.2.3",
        "localhost",
    ] {
        let r = ask(ok).await.unwrap();
        assert_eq!(r.status(), 200, "{ok}");
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        assert!(r.headers().contains_key("content-security-policy"));
    }
}
