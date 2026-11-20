//! Which sites run this app, which of them this site can reach, and where the directory
//! should send people.
//!
//! A site counts as up while its app's heartbeats keep arriving. "When was it last
//! alive" never trusts the other site's wall clock: each heartbeat carries the sender's
//! run id and a monotonic counter, and this site's core stamps when it first saw each
//! one. Within a run, the earliest-arriving beat pins the sender's counter to this site's
//! clock, so a backlog of old beats delivered after a split can't make a dead site look
//! alive, and a site whose clock is hours out is still judged right. What's assumed:
//! this box's own clock runs steadily, and a site that restarted on the far side of a
//! split and then died has only its backlog to go on.
//!
//! RF links drop packets and the core syncs every couple of minutes, so a site gets several
//! missed beats of grace, twice that when the mesh path to it runs over a weak link. When a
//! site goes quiet, the mesh's own link data says why: no path at all means the mesh has
//! split; a path that's up means the site's Pi, core or app has stopped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::Result;
use rusqlite::{Connection, params};

use ridgewarden_client::graph::{Edge, Graph, edge};
use ridgewarden_client::util::human_duration;

/// Link data older than this says nothing about the mesh now. Crawled nodes are polled
/// every 15 minutes by default, so allow two rounds.
const LINK_WINDOW_MS: i64 = 30 * 60_000;

/// A clock this far out is worth telling the operator about.
pub const CLOCK_WARN_MS: i64 = 120_000;

#[derive(Clone, Copy, Debug)]
pub struct Rules {
    pub grace_ms: i64,
    pub weak_quality: f64,
}

impl Rules {
    pub fn from_cfg(c: &crate::config::Config) -> Self {
        Rules {
            grace_ms: c.peers.grace_s as i64 * 1000,
            weak_quality: c.peers.weak_quality,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Here,
    Up,
    /// not heard from, but the mesh path to it is up: its Pi, core or app has stopped
    Silent,
    /// not heard from, and no mesh path from here: the other side of a split
    CutOff,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Here => "THIS SITE",
            State::Up => "UP",
            State::Silent => "SILENT",
            State::CutOff => "CUT OFF",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Site {
    pub key: String,
    pub call: String,
    /// the node its own core polls on its LAN; "" until one of those polls arrives
    pub node: String,
    pub url: String,
    /// what the site's operator says powers it ("grid", "battery", ...)
    pub power: String,
    /// the operator's preference for this site as the home of single-site jobs, 0-100
    pub weight: i64,
    pub state: State,
    /// when its app was last alive, on this site's clock
    pub last_heard: Option<i64>,
    /// how far its wall clock is from this site's, roughly
    pub clock_err: Option<i64>,
    /// hops over the mesh from this site's node, when there's a path
    pub hops: Option<usize>,
    /// the weakest link quality on that path, when the nodes report it
    pub weakest: Option<f64>,
    pub why: String,
}

/// The mesh as the freshest polls describe it.
#[derive(Default)]
pub struct MeshView {
    pub graph: Graph,
    quality: BTreeMap<Edge, f64>,
}

impl MeshView {
    pub fn load(conn: &Connection, now: i64) -> Result<Self> {
        // each node's newest poll, if it worked; a node whose newest poll failed is gone
        let mut st = conn.prepare(
            "SELECT p.node, l.neighbor, l.quality, l.nquality
             FROM polls p LEFT JOIN link_samples l ON l.event_id = p.event_id
             WHERE p.ok = 1 AND p.ts >= ?1
               AND p.ts = (SELECT max(q.ts) FROM polls q WHERE q.node = p.node)",
        )?;
        let rows = st.query_map(params![now - LINK_WINDOW_MS], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
            ))
        })?;
        let mut v = MeshView::default();
        for row in rows {
            let (node, nb, q, nq) = row?;
            v.graph.add_node(&node);
            let Some(nb) = nb else { continue };
            v.graph.add_edge(&node, &nb);
            let worst = match (q, nq) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if let Some(w) = worst {
                let e = v.quality.entry(edge(&node, &nb)).or_insert(w);
                *e = e.min(w);
            }
        }
        Ok(v)
    }

    /// Fewest-hops path between two nodes: (hops, weakest link quality on it).
    pub fn route(&self, from: &str, to: &str) -> Option<(usize, Option<f64>)> {
        if from == to {
            return self.graph.adj.contains_key(from).then_some((0, None));
        }
        let mut prev: BTreeMap<&str, &str> = BTreeMap::new();
        let mut seen = BTreeSet::from([from]);
        let mut todo = VecDeque::from([from]);
        while let Some(u) = todo.pop_front() {
            for v in self.graph.adj.get(u).into_iter().flatten() {
                if !seen.insert(v.as_str()) {
                    continue;
                }
                prev.insert(v, u);
                if v == to {
                    return Some(self.walk_back(&prev, to));
                }
                todo.push_back(v);
            }
        }
        None
    }

    fn walk_back(&self, prev: &BTreeMap<&str, &str>, to: &str) -> (usize, Option<f64>) {
        let (mut hops, mut weakest, mut at) = (0, None::<f64>, to);
        while let Some(p) = prev.get(at) {
            hops += 1;
            if let Some(q) = self.quality.get(&edge(p, at)) {
                weakest = Some(weakest.map_or(*q, |w| w.min(*q)));
            }
            at = p;
        }
        (hops, weakest)
    }
}

/// When each site's app was last alive on this site's clock, and how far its own wall
/// clock is out, from its heartbeats' run counters (see the module notes).
fn liveness(conn: &Connection) -> Result<BTreeMap<String, (i64, i64)>> {
    let mut st = conn.prepare(
        "SELECT b.site_key, min(b.seen_ms - b.mono_ms), max(b.mono_ms),
                (SELECT n.claimed_ms - n.mono_ms FROM beats n
                 WHERE n.site_key = b.site_key AND n.run = b.run ORDER BY n.mono_ms DESC LIMIT 1)
         FROM beats b WHERE b.seen_ms IS NOT NULL GROUP BY b.site_key, b.run",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut out: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for row in rows {
        let (key, anchor, last, wall0) = row?;
        let alive = anchor + last;
        if out.get(&key).is_none_or(|(a, _)| alive > *a) {
            out.insert(key, (alive, wall0 - anchor));
        }
    }
    Ok(out)
}

/// Every site that has run this app, judged from here. `me` is this site's core key and
/// `here` its node.
pub fn sites(conn: &Connection, now: i64, me: &str, here: &str, rules: Rules) -> Result<Vec<Site>> {
    let mesh = MeshView::load(conn, now)?;
    let alive = liveness(conn)?;
    // a site's node is the one its own core polls on its LAN, not what its heartbeat
    // says, so one station can't pose as another site's node
    let mut st = conn.prepare(
        "SELECT s.key, s.call,
                (SELECT p.node FROM polls p WHERE p.author_key = s.key AND p.via = 'lan'
                 ORDER BY p.ts DESC LIMIT 1),
                s.url, s.power, s.weight,
                (SELECT max(p.ts) FROM polls p WHERE p.author_key = s.key)
         FROM sites s",
    )?;
    let rows = st.query_map([], |r| {
        let key: String = r.get(0)?;
        let node: String = r.get::<_, Option<String>>(2)?.unwrap_or_default();
        let core_ts: Option<i64> = r.get(6)?;
        let route = (!node.is_empty() && !here.is_empty())
            .then(|| mesh.route(here, &node))
            .flatten();
        let (heard, clock_err) = alive.get(&key).copied().unzip();
        let quiet = heard.map_or(i64::MAX, |h| now - h);
        let core_recent = core_ts.is_some_and(|t| now - t <= rules.grace_ms);
        let (state, why) = verdict(key == me, quiet, route, core_recent, rules);
        Ok(Site {
            key,
            call: r.get(1)?,
            node,
            url: r.get(3)?,
            power: r.get(4)?,
            weight: r.get(5)?,
            state,
            last_heard: heard,
            clock_err,
            hops: route.map(|r| r.0),
            weakest: route.and_then(|r| r.1),
            why,
        })
    })?;
    let mut out: Vec<Site> = rows.collect::<Result<_, _>>()?;
    out.sort_by(|a, b| {
        let rank = |s: &Site| (s.state, s.hops.unwrap_or(usize::MAX), s.node.clone());
        rank(a).cmp(&rank(b))
    });
    Ok(out)
}

fn verdict(
    is_me: bool,
    quiet: i64,
    route: Option<(usize, Option<f64>)>,
    core_recent: bool,
    rules: Rules,
) -> (State, String) {
    let weakest = route.and_then(|r| r.1);
    let weak = weakest.is_some_and(|q| q < rules.weak_quality);
    let grace = if weak {
        2 * rules.grace_ms
    } else {
        rules.grace_ms
    };
    let for_how_long = if quiet == i64::MAX {
        "never heard from".to_string()
    } else {
        format!("nothing for {}", human_duration(quiet))
    };
    if is_me {
        (State::Here, "this site".into())
    } else if quiet <= grace && weak {
        let why = format!(
            "heard {} ago; the path is weak ({:.0}%), so it gets extra time before it's called down",
            human_duration(quiet),
            weakest.unwrap_or(0.0)
        );
        (State::Up, why)
    } else if quiet <= grace {
        (State::Up, format!("heard {} ago", human_duration(quiet)))
    } else if core_recent {
        let why = format!(
            "{for_how_long} from its chat app, though its ridgewarden core is still sending: the app there has stopped"
        );
        (State::Silent, why)
    } else if route.is_some() {
        let why = format!(
            "{for_how_long}, though the mesh path to it is up: its Pi or ridgewarden core has probably stopped"
        );
        (State::Silent, why)
    } else {
        let why = format!(
            "{for_how_long}, and no mesh path from here: it's on the other side of a split, or off the air"
        );
        (State::CutOff, why)
    }
}

/// The nearest other site that's up and has said where it can be reached.
pub fn nearest_up(sites: &[Site]) -> Option<&Site> {
    sites
        .iter()
        .filter(|s| s.state == State::Up && !s.url.is_empty())
        .min_by_key(|s| (s.hops.unwrap_or(usize::MAX), -s.last_heard.unwrap_or(0)))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::chat::tests::{T0, apply, db, ev};
    use crate::chat::{Chat, SITE, note_seen};
    use ridgewarden_client::core::{FeedItem, Projection};
    use ridgewarden_client::mesh::Mesh;
    use serde_json::json;

    pub const RULES: Rules = Rules {
        grace_ms: 360_000,
        weak_quality: 70.0,
    };
    pub const MIN: i64 = 60_000;

    pub fn key(site: &str) -> String {
        format!("{site:0>64}")
    }

    /// A heartbeat sent at `sent` by the sender's clock, `mono` ms into its run, first
    /// seen here at `seen`.
    pub fn beat_at(c: &mut Connection, site: &str, sent: i64, mono: i64, seen: i64) {
        beat_cfg(c, site, sent, mono, seen, "grid", 50);
    }

    /// The same, from a site whose operator set its power and placement weight.
    pub fn beat_cfg(
        c: &mut Connection,
        site: &str,
        sent: i64,
        mono: i64,
        seen: i64,
        power: &str,
        weight: i64,
    ) {
        let e = ev(
            site,
            SITE,
            json!({"node": format!("tst-{site}"), "url": format!("http://tst-{site}.local.mesh:8799"),
                   "run": "r1", "mono_ms": mono, "power": power, "weight": weight}),
            sent,
        );
        apply(c, &e, sent.min(seen));
        // its core polls its own node too, which is what ties the site to that node
        let lan = ev(
            site,
            "node.poll",
            json!({"node": format!("tst-{site}"), "ok": true, "via": "lan", "links": []}),
            sent,
        );
        let tx = c.transaction().unwrap();
        Mesh.apply(&tx, &lan, sent.min(seen)).unwrap();
        tx.commit().unwrap();
        let item = FeedItem {
            seq: 0,
            event: e,
            trusted: true,
            seen_ms: Some(seen),
        };
        note_seen(c, &[item]).unwrap();
    }

    /// A heartbeat from a site with an accurate clock, seen as soon as it was sent.
    pub fn beat(c: &mut Connection, site: &str, ts: i64) {
        beat_at(c, site, ts, ts - T0 + 1000, ts);
    }

    /// A poll by site `by` of `node`, with links to `nbs` at the given quality.
    pub fn poll(c: &mut Connection, by: &str, node: &str, nbs: &[(&str, f64)], ts: i64) {
        let links: Vec<_> = nbs
            .iter()
            .map(|(n, q)| json!({"neighbor": n, "quality": q, "nquality": q}))
            .collect();
        let e = ev(
            by,
            "node.poll",
            json!({"node": node, "ok": true, "links": links}),
            ts,
        );
        let tx = c.transaction().unwrap();
        Mesh.apply(&tx, &e, ts).unwrap();
        Chat.apply(&tx, &e, ts).unwrap();
        tx.commit().unwrap();
    }

    fn judge_b(c: &Connection, now: i64) -> Site {
        sites(c, now, &key("a"), "tst-a", RULES)
            .unwrap()
            .into_iter()
            .find(|s| s.node == "tst-b")
            .unwrap()
    }

    #[test]
    fn one_missed_beat_is_not_a_dead_site() {
        let mut c = db();
        beat(&mut c, "b", T0);
        assert_eq!(judge_b(&c, T0 + 2 * MIN).state, State::Up);
        assert_eq!(judge_b(&c, T0 + 5 * MIN).state, State::Up);
        assert_eq!(judge_b(&c, T0 + 7 * MIN).state, State::CutOff);
    }

    #[test]
    fn silent_site_with_a_mesh_path_is_silent_and_without_one_is_cut_off() {
        let mut c = db();
        beat(&mut c, "b", T0);
        let now = T0 + 10 * MIN;
        poll(&mut c, "a", "tst-a", &[("tst-mid", 95.0)], now - MIN);
        poll(
            &mut c,
            "a",
            "tst-mid",
            &[("tst-a", 95.0), ("tst-b", 90.0)],
            now - MIN,
        );
        let b = judge_b(&c, now);
        assert_eq!(b.state, State::Silent);
        assert_eq!(b.hops, Some(2));
        // the link to b drops out of the next poll
        poll(&mut c, "a", "tst-mid", &[("tst-a", 95.0)], now);
        assert_eq!(judge_b(&c, now).state, State::CutOff);
    }

    #[test]
    fn a_weak_path_gets_twice_the_grace() {
        let mut c = db();
        beat(&mut c, "b", T0);
        let now = T0 + 9 * MIN;
        poll(&mut c, "a", "tst-a", &[("tst-b", 95.0)], now);
        assert_eq!(judge_b(&c, now).state, State::Silent);
        poll(&mut c, "a", "tst-a", &[("tst-b", 40.0)], now + 1);
        let b = judge_b(&c, now + 1);
        assert_eq!(b.state, State::Up);
        assert_eq!(b.weakest, Some(40.0));
    }

    #[test]
    fn a_core_still_polling_means_the_app_stopped_not_the_site() {
        let mut c = db();
        beat(&mut c, "b", T0);
        poll(&mut c, "b", "tst-b", &[], T0 + 20 * MIN);
        let b = judge_b(&c, T0 + 21 * MIN);
        assert_eq!(b.state, State::Silent);
        assert!(b.why.contains("app there has stopped"), "{}", b.why);
    }

    #[test]
    fn a_backlog_from_a_dead_site_doesnt_bring_it_back() {
        let mut c = db();
        // b beats promptly, then the mesh splits and b dies at T0 + 4 min; its last
        // beats only reach us when the link heals an hour later
        for m in 0..3 {
            beat(&mut c, "b", T0 + m * MIN);
        }
        let heal = T0 + 60 * MIN;
        beat_at(&mut c, "b", T0 + 4 * MIN, 4 * MIN + 1000, heal);
        let b = judge_b(&c, heal + MIN);
        assert_ne!(b.state, State::Up);
        assert_eq!(b.last_heard, Some(T0 + 4 * MIN));
    }

    #[test]
    fn a_wrong_wall_clock_doesnt_change_the_verdict() {
        let mut c = db();
        // b's clock is three hours slow, but its beats arrive on time
        let skew = -3 * 60 * MIN;
        for m in 0..5 {
            let real = T0 + m * MIN;
            beat_at(&mut c, "b", real + skew, m * MIN + 1000, real + 2000);
        }
        let b = judge_b(&c, T0 + 5 * MIN);
        assert_eq!(b.state, State::Up);
        let err = b.clock_err.unwrap();
        assert!((err - skew).abs() < 5000, "{err}");
    }

    #[test]
    fn directory_sends_people_to_the_nearest_up_site() {
        let mut c = db();
        let now = T0 + MIN;
        beat(&mut c, "a", now);
        beat(&mut c, "b", now);
        beat(&mut c, "c", now);
        poll(
            &mut c,
            "a",
            "tst-a",
            &[("tst-c", 90.0), ("tst-x", 90.0)],
            now,
        );
        poll(&mut c, "a", "tst-x", &[("tst-b", 90.0)], now);
        let all = sites(&c, now, &key("a"), "tst-a", RULES).unwrap();
        assert_eq!(nearest_up(&all).unwrap().node, "tst-c");
        let gone: Vec<Site> = all.into_iter().filter(|s| s.node != "tst-c").collect();
        assert_eq!(nearest_up(&gone).unwrap().node, "tst-b");
    }

    #[test]
    fn nowhere_to_send_people_when_every_other_site_is_down() {
        let mut c = db();
        beat(&mut c, "a", T0);
        beat(&mut c, "b", T0);
        let all = sites(&c, T0 + 30 * MIN, &key("a"), "tst-a", RULES).unwrap();
        assert!(nearest_up(&all).is_none());
    }

    #[test]
    fn this_box_stepping_its_clock_forward_doesnt_kill_everyone_else() {
        let mut c = db();
        // this box booted three hours slow; b's beats were stamped on that clock
        let slow = 3 * 60 * MIN;
        for m in 0..5 {
            let real = T0 + m * MIN;
            beat_at(&mut c, "b", real, m * MIN + 1000, real - slow);
        }
        // NTP arrives: the step is caught and the old stamps moved
        crate::chat::shift_seen(&c, slow).unwrap();
        for m in 5..8 {
            let real = T0 + m * MIN;
            beat_at(&mut c, "b", real, m * MIN + 1000, real);
        }
        let b = judge_b(&c, T0 + 8 * MIN);
        assert_eq!(b.state, State::Up);
    }

    #[test]
    fn a_site_cant_take_another_sites_node_by_claiming_it() {
        let mut c = db();
        beat(&mut c, "a", T0);
        // x's heartbeat says it is tst-a, but x's core never polled tst-a on its LAN
        let e = ev(
            "x",
            SITE,
            json!({"node": "tst-a", "url": "http://tst-x.local.mesh"}),
            T0,
        );
        apply(&mut c, &e, T0);
        poll(&mut c, "a", "tst-a", &[("tst-b", 90.0)], T0);
        let all = sites(&c, T0, &key("a"), "tst-a", RULES).unwrap();
        let x = all.iter().find(|s| s.key == key("x")).unwrap();
        assert_eq!(x.node, "");
        assert_eq!(x.hops, None);
    }
}
