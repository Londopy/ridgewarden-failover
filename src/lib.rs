//! ridgewarden-failover: MeshChat-compatible chat that keeps working when the site hosting
//! it goes down, and merges both sides when a split mesh heals.
//!
//! Runs next to a ridgewarden core at every site. Each message is a signed event in the
//! core's main log, which the cores copy to every site they can reach; each site's app
//! serves its own merged view, so there's no server to lose. Shared files go through the
//! core's blob store the same way, and a status board merges as a document. How each of
//! these merges after a split is declared in `services`.

pub mod board;
pub mod bridge;
pub mod chat;
pub mod config;
pub mod drill;
pub mod files;
pub mod limit;
pub mod md5;
pub mod meshchat;
pub mod placement;
pub mod services;
pub mod sites;
pub mod web;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use ridgewarden_client::core::{FEED_PAGE, FeedItem, Projection, Whoami, apply_page};
use ridgewarden_client::event::Event;
use ridgewarden_client::mesh::Mesh;
use ridgewarden_client::util::{now_ms, rfc3339_to_ms};
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::board::Board;
use crate::chat::Chat;
use crate::config::Config;
use crate::limit::Limiter;

pub const PROJECTIONS: &[&dyn Projection] = &[&Mesh, &Chat, &Board];

/// How long a sites view is reused.
const VIEW_TTL: Duration = Duration::from_secs(3);
/// A jump of this box's wall clock bigger than this, against its monotonic clock, is a
/// step rather than drift.
const CLOCK_STEP_MS: i64 = 30_000;

/// Every site and the mesh, as judged at one moment.
pub struct View {
    pub sites: Vec<sites::Site>,
    pub mesh: sites::MeshView,
}

/// How often the app asks its core for new events. It's a loopback request that
/// usually returns nothing, and it's the floor on how fast a message shows up.
const FOLLOW_EVERY: Duration = Duration::from_secs(3);

/// The core's identity, kept so a restart with the core down still knows which site
/// this is.
const WHOAMI_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS whoami (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    call TEXT NOT NULL,
    key  TEXT NOT NULL,
    node TEXT NOT NULL
)";

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// put in this app's own forms, so other web pages can't post through a visitor
    pub form_token: String,
    pub limiter: Limiter,
    /// for talking to MeshChat nodes
    pub http: reqwest::Client,
    who: Mutex<Option<Whoami>>,
    /// when the core last answered a feed request (ms), 0 if never
    core_ok: AtomicI64,
    /// the last sites view and when it was made
    view: Mutex<Option<(Instant, Arc<View>)>>,
    /// this box's wall clock and monotonic clock at the last feed round, to catch the
    /// wall clock stepping (NTP arriving on a Pi with no clock battery)
    clocks: Mutex<Option<(i64, Instant)>>,
    /// this run's id and start, which heartbeats carry so other sites can judge this
    /// one without trusting its wall clock
    run: String,
    started: Instant,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let token = cfg
            .core
            .token_file
            .exists()
            .then_some(cfg.core.token_file.as_path());
        if token.is_none() {
            tracing::warn!(
                "core token {} not found; reading works, posting won't",
                cfg.core.token_file.display()
            );
        }
        let core = CoreClient::new(&cfg.core.url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(WHOAMI_SCHEMA)?;
        placement::init(&db)?;
        bridge::init(&db)?;
        let who = db
            .query_row("SELECT call, key, node FROM whoami WHERE id = 1", [], |r| {
                Ok(Whoami {
                    call: r.get(0)?,
                    key: r.get(1)?,
                    node: r.get(2)?,
                })
            })
            .optional()?;
        Ok(Self {
            limiter: Limiter::new(cfg.posts_per_minute),
            cfg,
            core,
            db: Mutex::new(db),
            form_token: format!("{:016x}{:016x}", getrandom::u64()?, getrandom::u64()?),
            who: Mutex::new(who),
            core_ok: AtomicI64::new(0),
            view: Mutex::new(None),
            clocks: Mutex::new(None),
            http: bridge::client()?,
            run: format!("{:016x}", getrandom::u64()?),
            started: Instant::now(),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The core's key and node, cached once the core knows its node.
    pub async fn whoami(&self) -> Result<Whoami> {
        if let Some(w) = self.cached_who()
            && !w.node.is_empty()
        {
            return Ok(w);
        }
        let w = self.core.whoami().await?;
        self.db().execute(
            "INSERT INTO whoami (id, call, key, node) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET call = excluded.call, key = excluded.key, node = excluded.node",
            params![w.call, w.key, w.node],
        )?;
        *self.who.lock().unwrap_or_else(|e| e.into_inner()) = Some(w.clone());
        Ok(w)
    }

    fn cached_who(&self) -> Option<Whoami> {
        self.who.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// This site's key and node as last known, without asking the core; empty if never.
    pub fn me(&self) -> (String, String) {
        self.cached_who()
            .map(|w| (w.key, ridgewarden_client::util::norm_node(&w.node)))
            .unwrap_or_default()
    }

    /// Whether this site can take posts: its core answered recently.
    pub fn here_ok(&self, now: i64) -> bool {
        now - self.core_ok.load(Ordering::Relaxed) < 15_000
    }

    pub fn core_last_ok(&self) -> Option<i64> {
        Some(self.core_ok.load(Ordering::Relaxed)).filter(|t| *t > 0)
    }

    /// The time to judge other sites at: now, or with this site's own core down, the
    /// last time it could hear anything, since silence since then says nothing about them.
    fn as_of(&self, now: i64) -> i64 {
        if self.here_ok(now) {
            now
        } else {
            self.core_last_ok().unwrap_or(now)
        }
    }

    /// Every site judged from here, and the mesh it was judged on, reused for a few
    /// seconds since every page and the placement need it. A site's address is dropped
    /// unless it's a mesh host (or listed in `allow_hosts`), so a station can't point the
    /// directory elsewhere.
    pub fn view(&self, now: i64) -> Result<Arc<View>> {
        let mut cached = self.view.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = cached.as_ref()
            && at.elapsed() < VIEW_TTL
        {
            return Ok(v.clone());
        }
        let (me, here) = self.me();
        let rules = sites::Rules::from_cfg(&self.cfg);
        let db = self.db();
        let as_of = self.as_of(now);
        let mut all = sites::sites(&db, as_of, &me, &here, rules)?;
        for s in &mut all {
            if !s.url.is_empty() && !chat::mesh_url(&s.url, &self.cfg.allow_hosts) {
                s.url.clear();
            }
        }
        let v = Arc::new(View {
            sites: all,
            mesh: sites::MeshView::load(&db, as_of)?,
        });
        *cached = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// Drop the cached view: something just changed it.
    fn stale(&self) {
        *self.view.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn sites(&self, now: i64) -> Result<Vec<sites::Site>> {
        Ok(self.view(now)?.sites.clone())
    }

    /// Move the placement's home if the rules say so, and return it.
    pub fn place(&self, now: i64) -> Result<Option<placement::Home>> {
        let view = self.view(now)?;
        let all = &view.sites;
        let mesh = &view.mesh;
        let db = self.db();
        let failback = self.cfg.placement.failback_s as i64 * 1000;
        // a site that just started may not have heard the others yet, unless every site
        // it knows of is already up
        let settled = self.started.elapsed().as_secs() >= self.cfg.peers.grace_s
            || all
                .iter()
                .all(|s| matches!(s.state, sites::State::Here | sites::State::Up));
        placement::tick(&db, mesh, all, self.here_ok(now), now, failback, settled)
    }

    /// Whether this site is the home of single-site jobs right now.
    pub fn is_home(&self, now: i64) -> Result<bool> {
        let (me, _) = self.me();
        Ok(self.place(now)?.is_some_and(|h| h.key == me))
    }

    /// Put an event this site's core just signed into the view now, rather than on the
    /// next feed round, so the poster sees it straight away.
    pub fn apply_own(&self, e: &Event) -> Result<()> {
        let now = now_ms();
        let ts = rfc3339_to_ms(&e.wall_time).unwrap_or(now).min(now);
        let mut db = self.db();
        let tx = db.transaction()?;
        for p in PROJECTIONS {
            p.apply(&tx, e, ts)?;
        }
        tx.commit()?;
        let item = FeedItem {
            seq: 0,
            event: e.clone(),
            trusted: true,
            seen_ms: Some(now),
        };
        chat::note_seen(&db, &[item])?;
        drop(db);
        self.stale();
        Ok(())
    }

    /// Have the core sign and log a chat message, then show it.
    pub async fn post(&self, call: &str, id: &str, channel: &str, text: &str) -> Result<()> {
        if chat::already_posted(&self.db(), id, call, text)? {
            return Ok(());
        }
        let node = self.whoami().await.map(|w| w.node).unwrap_or_default();
        let e = self
            .core
            .append(
                chat::MSG,
                json!({"id": id, "call_sign": call, "channel": channel, "message": text, "node": node}),
            )
            .await?;
        self.apply_own(&e)
    }

    /// Hide a message or shared file at every site, by its event id or the first 8 or
    /// more characters of it: the operator's tool for traffic that shouldn't stay on
    /// the air. Returns the full event id.
    pub async fn hide(&self, prefix: &str) -> Result<String> {
        let found = chat::find_event(&self.db(), prefix)?;
        let [id] = found.as_slice() else {
            anyhow::bail!(
                "{} messages or files match {prefix:?}; give 8 or more characters of one event id",
                found.len()
            );
        };
        let e = self
            .core
            .append(chat::HIDE, json!({"event_id": id}))
            .await?;
        self.apply_own(&e)?;
        Ok(id.clone())
    }

    /// Tell the other sites this one is alive, where it is, how it would rank as home,
    /// and who is logged in.
    pub async fn beat(&self) -> Result<()> {
        let w = self.whoami().await?;
        let since = now_ms() / 1000 - 2 * self.cfg.peers.beat_s as i64;
        let users = chat::local_users(&self.db(), since)?;
        let e = self
            .core
            .append_telemetry(
                chat::SITE,
                json!({"node": w.node, "url": self.cfg.public_url, "zone": self.cfg.zone,
                       "users": users, "run": self.run,
                       "mono_ms": self.started.elapsed().as_millis() as u64,
                       "power": self.cfg.placement.power, "weight": self.cfg.placement.weight}),
            )
            .await?;
        self.apply_own(&e)
    }

    /// Post a board entry: `topic` set to `text`, replacing the versions in `prev`.
    pub async fn edit_board(
        &self,
        call: &str,
        topic: &str,
        text: &str,
        prev: &[String],
    ) -> Result<()> {
        let node = self.whoami().await.map(|w| w.node).unwrap_or_default();
        let e = self
            .core
            .append(
                board::EDIT,
                json!({"topic": topic, "text": text, "call_sign": call, "prev": prev, "node": node}),
            )
            .await?;
        self.apply_own(&e)
    }
}

fn cursor(conn: &Connection) -> Result<(Option<String>, i64)> {
    Ok(conn
        .query_row(
            "SELECT instance, seq FROM feed_cursor WHERE id = 1",
            [],
            |r| Ok((Some(r.get(0)?), r.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, -1)))
}

/// Pull everything new from the core into the view. The client crate's `follow` does
/// the same, but this one also keeps when the core first saw each message, which is
/// how a message that crossed a split is told from one that didn't.
/// If this box's wall clock stepped since the last round, move the arrival times
/// recorded on the old clock, so heartbeat counters pinned to them line up with the new
/// one. Returns the step in ms.
fn catch_clock_step(app: &App) -> Result<i64> {
    let (wall, mono) = (now_ms(), Instant::now());
    let last = app
        .clocks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace((wall, mono));
    let Some((w, m)) = last else { return Ok(0) };
    let step = (wall - w) - mono.duration_since(m).as_millis() as i64;
    if step.abs() < CLOCK_STEP_MS {
        return Ok(0);
    }
    tracing::warn!(
        "this box's clock jumped {} ms; moving recorded arrival times to match",
        step
    );
    chat::shift_seen(&app.db(), step)?;
    app.stale();
    Ok(step)
}

pub async fn follow(app: &App) -> Result<usize> {
    catch_clock_step(app)?;
    let mut total = 0;
    loop {
        let (instance, after) = cursor(&app.db())?;
        let mut feed = app.core.feed(after, FEED_PAGE).await?;
        if instance.as_deref().is_some_and(|i| i != feed.instance) {
            feed = app.core.feed(-1, FEED_PAGE).await?;
        }
        let full = feed.events.len() >= FEED_PAGE;
        let mut db = app.db();
        let n = apply_page(&mut db, &feed, PROJECTIONS)?;
        chat::note_seen(&db, &feed.events)?;
        drop(db);
        app.core_ok.store(now_ms(), Ordering::Relaxed);
        if n > 0 {
            app.stale();
        }
        total += n;
        if !full || n == 0 {
            return Ok(total);
        }
    }
}

/// Follow the core, send heartbeats, fetch shared files and tidy up, forever.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        let mut failing = false;
        loop {
            match follow(&a).await {
                Ok(_) => failing = false,
                // the core being down is the case this app exists for; say so once
                Err(e) if !failing => {
                    tracing::warn!("following the core: {e:#}");
                    failing = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(FOLLOW_EVERY).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = a.beat().await {
                tracing::debug!("heartbeat not sent: {e:#}");
            }
            if let Err(e) = a.place(now_ms()) {
                tracing::warn!("placement: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(a.cfg.peers.beat_s)).await;
        }
    });
    let a = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            if let Err(e) = bridge::round(&a, &a.http).await {
                tracing::warn!("MeshChat bridge: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(a.cfg.meshchat.interval_s)).await;
        }
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Err(e) = files::fetch_round(&app).await {
                tracing::warn!("fetching shared files: {e:#}");
            }
            if let Err(e) = chat::prune(&app.db(), now_ms()) {
                tracing::warn!("pruning old data: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}
