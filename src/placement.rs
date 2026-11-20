//! Placement: which site is "home" for the jobs that must run in one place (today the
//! bridge to MeshChat nodes, and the directory when `directory = "home"`).
//!
//! Chat itself runs everywhere; placement only picks where a single-site job runs. Every
//! site works it out on its own from the same shared data, so they agree whenever they
//! can hear each other. The rule, borrowed from Nomad's rescheduling:
//!
//! - candidates are sites that are up, with a placement weight above 0;
//! - the best is the one that reaches the most other chat sites over the mesh (the
//!   best-connected part of the network), then has the steadier power, then the higher
//!   operator weight, then is fewest hops from the others on average;
//! - if home stops being up, the best candidate takes over at once (failover);
//! - a better site takes home only after it has ranked higher for `failback_s` without a
//!   break, so neither a site that keeps dropping out nor a link that keeps changing the
//!   ranking moves the job back and forth (hysteresis).

use std::fmt::Write as _;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::chat::POWER;
use crate::sites::{MeshView, Site, State};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS placement (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    home  TEXT NOT NULL,
    since INTEGER NOT NULL
);
-- the site that ranks above home, and since when it has without a break
CREATE TABLE IF NOT EXISTS challenger (
    key   TEXT PRIMARY KEY,
    since INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS placement_log (
    ts   INTEGER NOT NULL,
    home TEXT NOT NULL,
    why  TEXT NOT NULL
);";

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// How a candidate ranks; smaller is better.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    pub reach: std::cmp::Reverse<usize>,
    pub power: usize,
    pub weight: std::cmp::Reverse<i64>,
    /// average hops to the other chat sites it reaches, in tenths
    pub spread: usize,
    pub key: String,
}

impl Rank {
    pub fn text(&self) -> String {
        let mut t = format!(
            "{} power, weight {}, reaches {} other site{}",
            POWER.get(self.power).copied().unwrap_or("unknown"),
            self.weight.0,
            self.reach.0,
            if self.reach.0 == 1 { "" } else { "s" }
        );
        if self.reach.0 > 0 {
            let _ = write!(t, ", {:.1} hops away on average", self.spread as f64 / 10.0);
        }
        t
    }
}

fn is_up(s: &Site, here_ok: bool) -> bool {
    match s.state {
        State::Here => here_ok,
        State::Up => true,
        State::Silent | State::CutOff => false,
    }
}

/// Rank every site that could be home.
pub fn ranks(mesh: &MeshView, sites: &[Site], here_ok: bool) -> Vec<(Rank, Site)> {
    let up: Vec<&Site> = sites.iter().filter(|s| is_up(s, here_ok)).collect();
    let mut out: Vec<(Rank, Site)> = up
        .iter()
        .filter(|s| s.weight > 0)
        .map(|s| {
            let hops: Vec<usize> = up
                .iter()
                .filter(|o| o.key != s.key)
                .filter_map(|o| mesh.route(&s.node, &o.node).map(|r| r.0))
                .collect();
            let spread = if hops.is_empty() {
                0
            } else {
                hops.iter().sum::<usize>() * 10 / hops.len()
            };
            let rank = Rank {
                reach: std::cmp::Reverse(hops.len()),
                power: POWER
                    .iter()
                    .position(|p| *p == s.power)
                    .unwrap_or(POWER.len()),
                weight: std::cmp::Reverse(s.weight),
                spread,
                key: s.key.clone(),
            };
            (rank, (*s).clone())
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Home {
    pub key: String,
    pub since: i64,
}

pub fn home(conn: &Connection) -> Result<Option<Home>> {
    Ok(conn
        .query_row("SELECT home, since FROM placement WHERE id = 1", [], |r| {
            Ok(Home {
                key: r.get(0)?,
                since: r.get(1)?,
            })
        })
        .optional()?)
}

/// Changes of home, newest first: (when, home key, why).
pub fn log(conn: &Connection, limit: usize) -> Result<Vec<(i64, String, String)>> {
    let v = conn
        .prepare("SELECT ts, home, why FROM placement_log ORDER BY ts DESC LIMIT ?1")?
        .query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

fn set_home(conn: &Connection, key: &str, now: i64, why: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO placement (id, home, since) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET home = excluded.home, since = excluded.since",
        params![key, now],
    )?;
    conn.execute(
        "INSERT INTO placement_log (ts, home, why) VALUES (?1, ?2, ?3)",
        params![now, key, why],
    )?;
    conn.execute(
        "DELETE FROM placement_log WHERE ts < (SELECT min(ts) FROM
             (SELECT ts FROM placement_log ORDER BY ts DESC LIMIT 50))",
        [],
    )?;
    Ok(())
}

/// Look at the sites again and move home if the rules say so. Returns the home, if any
/// site can be. Until `settled` (this app has run long enough to hear the others), no
/// first choice is made, so a site that just started doesn't crown itself.
pub fn tick(
    conn: &Connection,
    mesh: &MeshView,
    sites: &[Site],
    here_ok: bool,
    now: i64,
    failback_ms: i64,
    settled: bool,
) -> Result<Option<Home>> {
    let ranked = ranks(mesh, sites, here_ok);
    let Some((best_rank, best)) = ranked.first() else {
        conn.execute("DELETE FROM challenger", [])?;
        return home(conn);
    };
    let label = |s: &Site| {
        if s.node.is_empty() {
            s.call.clone()
        } else {
            s.node.clone()
        }
    };
    let Some(cur) = home(conn)? else {
        if settled {
            let why = format!("first choice: {}", best_rank.text());
            set_home(conn, &best.key, now, &why)?;
        }
        return home(conn);
    };
    match ranked.iter().find(|(_, s)| s.key == cur.key) {
        None => {
            let gone = sites
                .iter()
                .find(|s| s.key == cur.key)
                .map(|s| format!("{} stopped being up ({})", label(s), s.why))
                .unwrap_or_else(|| "the old home is no longer known".into());
            set_home(conn, &best.key, now, &format!("failover: {gone}"))?;
        }
        Some((r, _)) if best_rank < r => {
            // the challenger has to stay better without a break, not merely stay up
            conn.execute("DELETE FROM challenger WHERE key <> ?1", [&best.key])?;
            conn.execute(
                "INSERT OR IGNORE INTO challenger (key, since) VALUES (?1, ?2)",
                params![best.key, now],
            )?;
            let since: i64 = conn.query_row("SELECT since FROM challenger", [], |r| r.get(0))?;
            if now - since >= failback_ms {
                let why = format!(
                    "{} has ranked higher ({}) for {}",
                    label(best),
                    best_rank.text(),
                    ridgewarden_client::util::human_duration(now - since)
                );
                set_home(conn, &best.key, now, &why)?;
            }
            return home(conn);
        }
        Some(_) => {}
    }
    conn.execute("DELETE FROM challenger", [])?;
    home(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::{T0, db};
    use crate::sites::sites;
    use crate::sites::tests::{MIN, RULES, beat_cfg, key, poll};

    fn beat(c: &mut Connection, site: &str, power: &str, weight: i64, ts: i64) {
        beat_cfg(c, site, ts, ts - T0 + 1000, ts, power, weight);
    }

    fn run(c: &mut Connection, now: i64, failback: i64) -> String {
        let all = sites(c, now, &key("a"), "tst-a", RULES).unwrap();
        let mesh = MeshView::load(c, now).unwrap();
        tick(c, &mesh, &all, true, now, failback, true)
            .unwrap()
            .unwrap()
            .key
    }

    fn mesh(c: &mut Connection, ts: i64) {
        poll(c, "a", "tst-a", &[("tst-b", 90.0), ("tst-c", 90.0)], ts);
        poll(c, "a", "tst-b", &[("tst-a", 90.0), ("tst-c", 90.0)], ts);
    }

    #[test]
    fn steadier_power_wins_and_weight_zero_never_does() {
        let mut c = db();
        init(&c).unwrap();
        beat(&mut c, "a", "battery", 50, T0);
        beat(&mut c, "b", "solar", 50, T0);
        beat(&mut c, "c", "grid", 0, T0);
        mesh(&mut c, T0);
        assert_eq!(run(&mut c, T0, 15 * MIN), key("b"));
    }

    #[test]
    fn failover_is_immediate_and_failback_waits() {
        let mut c = db();
        init(&c).unwrap();
        beat(&mut c, "a", "battery", 50, T0);
        beat(&mut c, "b", "grid", 50, T0);
        mesh(&mut c, T0);
        assert_eq!(run(&mut c, T0, 15 * MIN), key("b"));
        // b goes quiet: home moves to a as soon as b is judged down
        for m in 1..=7 {
            beat(&mut c, "a", "battery", 50, T0 + m * MIN);
            mesh(&mut c, T0 + m * MIN);
        }
        assert_eq!(run(&mut c, T0 + 7 * MIN, 15 * MIN), key("a"));
        // b comes back; a keeps home until b has been up for the failback time
        for m in 8..=30 {
            let now = T0 + m * MIN;
            beat(&mut c, "a", "battery", 50, now);
            beat(&mut c, "b", "grid", 50, now);
            mesh(&mut c, now);
            let want = if m >= 8 + 15 { key("b") } else { key("a") };
            assert_eq!(run(&mut c, now, 15 * MIN), want, "minute {m}");
        }
        let changes = log(&c, 10).unwrap();
        assert_eq!(changes.len(), 3);
        assert!(changes[1].2.starts_with("failover"), "{}", changes[1].2);
        assert!(changes[0].2.contains("for 15 min"), "{}", changes[0].2);
    }

    #[test]
    fn better_connected_beats_better_powered() {
        let mut c = db();
        init(&c).unwrap();
        beat(&mut c, "a", "battery", 50, T0);
        beat(&mut c, "b", "grid", 50, T0);
        beat(&mut c, "c", "battery", 50, T0);
        // b sits alone at the far end of the mesh; a and c can reach each other
        poll(&mut c, "a", "tst-a", &[("tst-c", 90.0)], T0);
        poll(&mut c, "a", "tst-x", &[("tst-b", 90.0)], T0);
        let all = sites(&c, T0, &key("a"), "tst-a", RULES).unwrap();
        let mesh = MeshView::load(&c, T0).unwrap();
        let r = ranks(&mesh, &all, true);
        assert_eq!(r.last().unwrap().1.node, "tst-b");
    }

    #[test]
    fn a_link_that_keeps_flapping_doesnt_move_home() {
        let mut c = db();
        init(&c).unwrap();
        // a-c is steady; the a-b link, b's only way in, comes and goes
        let polls = |c: &mut Connection, ts: i64, ab: bool| {
            if ab {
                poll(c, "a", "tst-a", &[("tst-b", 90.0), ("tst-c", 90.0)], ts);
                poll(c, "a", "tst-b", &[("tst-a", 90.0)], ts);
            } else {
                poll(c, "a", "tst-a", &[("tst-c", 90.0)], ts);
                poll(c, "a", "tst-b", &[], ts);
            }
            poll(c, "a", "tst-c", &[("tst-a", 90.0)], ts);
        };
        let beats = |c: &mut Connection, ts: i64| {
            beat(c, "a", "battery", 50, ts);
            beat(c, "b", "grid", 50, ts);
            beat(c, "c", "battery", 10, ts);
        };
        // the a-b link is down at first: a reaches c, b reaches nobody, so a is home
        beats(&mut c, T0);
        polls(&mut c, T0, false);
        assert_eq!(run(&mut c, T0, 15 * MIN), key("a"));
        // with the link up b ranks higher (grid power); it comes and goes every 2 minutes
        for m in 1..=40 {
            let now = T0 + m * MIN;
            beats(&mut c, now);
            polls(&mut c, now, (m / 2) % 2 == 1);
            assert_eq!(run(&mut c, now, 15 * MIN), key("a"), "minute {m}");
        }
        // once it stays up, b takes home after 15 minutes
        let mut moved = None;
        for m in 41..=60 {
            let now = T0 + m * MIN;
            beats(&mut c, now);
            polls(&mut c, now, true);
            if run(&mut c, now, 15 * MIN) == key("b") && moved.is_none() {
                moved = Some(m);
            }
        }
        assert_eq!(moved, Some(41 + 15));
    }
}
