//! The "kill a mountaintop" drill: take one site down on purpose and measure what the
//! spec promises. It posts tagged messages at every site, has you (or a command you give
//! it) take the chosen site down, checks that the others keep taking posts and notice the
//! loss, has the site brought back, and checks that every site ends up with every drill
//! message. It talks to the sites only over their normal HTTP API, so it tests what users
//! get. Once the site is down, it is always brought back and the result always logged
//! (as a signed event, so every site's sites page keeps a tally), even if a step fails or
//! the drill is stopped with Ctrl-C.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use ridgewarden_client::util::norm_node;

use crate::App;

/// The spec's promise: a service survives the loss of its host site within this long.
pub const SURVIVE_LIMIT_MS: i64 = 60_000;
/// Between attempts to post while waiting for a site; slow enough to stay inside the
/// per-address posting limit.
const RETRY: Duration = Duration::from_secs(5);

pub struct Plan {
    /// node name of the site to take down
    pub target: String,
    /// shell commands that take it down and bring it back; without them, the drill asks
    /// you to do it and waits for Enter
    pub down: Option<String>,
    pub up: Option<String>,
    /// messages posted at each site in each phase
    pub per_phase: usize,
    /// how long to wait for anything before calling it failed
    pub timeout: Duration,
}

#[derive(Debug, Default)]
pub struct Report {
    pub target: String,
    pub sites: usize,
    pub sent: usize,
    pub lost: usize,
    /// before the drill, how long until every site had every message
    pub converge_ms: Option<i64>,
    /// the slowest surviving site's time to take a post after the loss
    pub survive_ms: Option<i64>,
    /// how long until this site's sites page stopped calling the lost site up
    pub detect_ms: Option<i64>,
    /// after it came back, how long until every site had every drill message
    pub merge_ms: Option<i64>,
    pub missing: Vec<String>,
    /// why the drill stopped early, if it did
    pub error: Option<String>,
}

impl Report {
    /// Nothing lost, every site checked, and every survivor took posts in time.
    pub fn passed(&self) -> bool {
        self.error.is_none()
            && self.lost == 0
            && self.merge_ms.is_some()
            && self.survive_ms.is_some_and(|t| t <= SURVIVE_LIMIT_MS)
    }

    pub fn text(&self) -> String {
        let t = |v: Option<i64>| {
            v.map_or("not reached".into(), |ms| {
                format!("{:.1} s", ms as f64 / 1000.0)
            })
        };
        let mut out = format!(
            "Drill: took {} down, {} sites\n\
             before: every site had every message after {}\n\
             survivors kept taking posts after {} (the spec asks for 60 s)\n\
             this site saw it go after {}\n\
             after it came back, everything merged after {}\n\
             messages: {} sent, {} lost\n",
            self.target,
            self.sites,
            t(self.converge_ms),
            t(self.survive_ms),
            t(self.detect_ms),
            t(self.merge_ms),
            self.sent,
            self.lost
        );
        for m in &self.missing {
            out.push_str(&format!("missing: {m}\n"));
        }
        if let Some(e) = &self.error {
            out.push_str(&format!("stopped early: {e}\n"));
        }
        out.push_str(if self.passed() {
            "PASSED\n"
        } else {
            "FAILED\n"
        });
        out
    }

    pub fn event(&self, node: &str) -> Value {
        json!({"target": self.target, "sites": self.sites, "sent": self.sent, "lost": self.lost,
               "survive_ms": self.survive_ms, "detect_ms": self.detect_ms,
               "merge_ms": self.merge_ms, "node": node, "error": self.error})
    }
}

struct DrillSite {
    node: String,
    url: String,
}

fn elapsed_ms(t: Instant) -> i64 {
    t.elapsed().as_millis() as i64
}

async fn post(http: &reqwest::Client, url: &str, id: &str, text: &str) -> Result<()> {
    use crate::web::enc;
    let body = format!(
        "action=send_message&call_sign=DRILL&id={id}&channel=drill&message={}",
        enc(text)
    );
    let v: Value = http
        .post(format!("{url}/cgi-bin/meshchat"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-requested-with", "XMLHttpRequest")
        .body(body)
        .send()
        .await?
        .json()
        .await?;
    if v["status"] != 200 {
        bail!("{}", v["response"].as_str().unwrap_or("refused"));
    }
    Ok(())
}

/// Ids of the drill's messages a site has.
async fn held(http: &reqwest::Client, url: &str, tag: &str) -> Result<BTreeSet<String>> {
    let v: Value = http
        .get(format!("{url}/cgi-bin/meshchat?action=messages"))
        .send()
        .await?
        .json()
        .await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["message"].as_str().is_some_and(|t| t.contains(tag)))
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect())
}

/// What was still missing when a wait ran out.
struct Gaps {
    /// drill messages some answering site didn't have
    ids: BTreeSet<String>,
    /// "id at site", and sites that didn't answer, for the report
    notes: Vec<String>,
}

/// Wait until every site has every id; how long it took, or what's still missing.
async fn converge(
    http: &reqwest::Client,
    sites: &[DrillSite],
    tag: &str,
    want: &BTreeSet<String>,
    timeout: Duration,
) -> Result<i64, Gaps> {
    let t = Instant::now();
    loop {
        let mut gaps = Gaps {
            ids: BTreeSet::new(),
            notes: vec![],
        };
        for s in sites {
            match held(http, &s.url, tag).await {
                Ok(have) => {
                    for id in want.difference(&have) {
                        gaps.ids.insert(id.clone());
                        gaps.notes.push(format!("{id} at {}", s.node));
                    }
                }
                Err(e) => gaps.notes.push(format!("{} didn't answer: {e:#}", s.node)),
            }
        }
        if gaps.notes.is_empty() {
            return Ok(elapsed_ms(t));
        }
        if t.elapsed() > timeout {
            return Err(gaps);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn step(what: &str, cmd: &Option<String>) -> Result<()> {
    match cmd {
        Some(c) => {
            println!("{what}: running {c}");
            let status = if cfg!(windows) {
                tokio::process::Command::new("cmd")
                    .args(["/C", c])
                    .status()
                    .await
            } else {
                tokio::process::Command::new("sh")
                    .args(["-c", c])
                    .status()
                    .await
            }
            .with_context(|| format!("running {c}"))?;
            if !status.success() {
                bail!("{c} exited with {status}");
            }
        }
        None => {
            println!("{what}, then press Enter.");
            tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)
            })
            .await??;
        }
    }
    Ok(())
}

fn id() -> Result<String> {
    Ok(format!("{:08x}", getrandom::u32()?))
}

/// Post this phase's messages at `sites`; returns their ids.
async fn phase(
    http: &reqwest::Client,
    sites: &[&DrillSite],
    tag: &str,
    name: &str,
    numbers: std::ops::Range<usize>,
) -> Result<Vec<String>> {
    let mut ids = vec![];
    for s in sites {
        for i in numbers.clone() {
            let id = id()?;
            let text = format!("[{tag}] {name} at {} #{i}", s.node);
            post(http, &s.url, &id, &text)
                .await
                .with_context(|| format!("posting at {}", s.node))?;
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Where this site's own app answers, for the drill to talk to it like a user would.
fn own_url(app: &App) -> String {
    if app.cfg.public_url.is_empty() {
        format!("http://127.0.0.1:{}", app.cfg.listen.port())
    } else {
        app.cfg.public_url.trim_end_matches('/').to_string()
    }
}

/// The target's state as this site's sites page sees it.
async fn state_of(http: &reqwest::Client, own: &str, target: &str) -> Result<String> {
    let v: Value = http
        .get(format!("{own}/api/sites"))
        .send()
        .await?
        .json()
        .await?;
    Ok(v["sites"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["node"] == target)
        .and_then(|s| s["state"].as_str())
        .unwrap_or("UNKNOWN")
        .to_string())
}

pub async fn run(app: &App, plan: Plan) -> Result<Report> {
    let http = &app.http;
    let own = own_url(app);
    let target = norm_node(&plan.target);
    let v: Value = http
        .get(format!("{own}/api/sites"))
        .send()
        .await
        .with_context(|| format!("asking this site's app at {own} for the sites list"))?
        .json()
        .await?;
    let mut sites = vec![];
    for s in v["sites"].as_array().into_iter().flatten() {
        let (node, state) = (
            s["node"].as_str().unwrap_or(""),
            s["state"].as_str().unwrap_or(""),
        );
        let url = if state == "THIS SITE" {
            own.clone()
        } else {
            s["url"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_string()
        };
        if matches!(state, "THIS SITE" | "UP") && !url.is_empty() {
            sites.push(DrillSite {
                node: node.to_string(),
                url,
            });
        }
    }
    if !sites.iter().any(|s| s.node == target) {
        bail!("{target} isn't an up site with an address; check the sites page");
    }
    if v["sites"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["node"] == target.as_str() && s["state"] == "THIS SITE")
    {
        bail!("run the drill from a site other than the one being taken down");
    }
    if sites.len() < 2 {
        bail!("a drill needs at least two sites that are up");
    }
    let tag = format!("drill {}", &id()?[..6]);
    let mut report = Report {
        target: target.clone(),
        sites: sites.len(),
        ..Default::default()
    };
    let all: Vec<&DrillSite> = sites.iter().collect();
    let survivors: Vec<&DrillSite> = sites.iter().filter(|s| s.node != target).collect();
    let lost_site = sites
        .iter()
        .find(|s| s.node == target)
        .expect("checked above");

    println!(
        "Posting {} messages at each of {} sites",
        plan.per_phase,
        sites.len()
    );
    let mut want: BTreeSet<String> = phase(http, &all, &tag, "before", 1..plan.per_phase + 1)
        .await?
        .into_iter()
        .collect();
    report.converge_ms = converge(http, &sites, &tag, &want, plan.timeout).await.ok();

    let run = Run {
        http,
        plan: &plan,
        tag: &tag,
        own: &own,
        target: &target,
    };
    // from here on the site may be down, so whatever happens it gets brought back and the
    // result is recorded, Ctrl-C included
    let outcome = tokio::select! {
        r = run.lose(&survivors, &mut want, &mut report) => r,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted with Ctrl-C")),
    };
    if let Err(e) = outcome {
        println!("Drill stopped: {e:#}");
        report.error = Some(format!("{e:#}"));
    }
    match run.bring_back(lost_site, &mut want).await {
        Ok(back) => {
            report.sent = want.len();
            match converge(http, &sites, &tag, &want, plan.timeout).await {
                Ok(_) => report.merge_ms = Some(elapsed_ms(back)),
                Err(gaps) => {
                    report.lost = gaps.ids.len();
                    report.missing = gaps.notes;
                }
            }
        }
        Err(e) => {
            println!("Bringing {target} back failed: {e:#}");
            report.sent = want.len();
            let why = format!("bringing it back: {e:#}");
            report.error = Some(match report.error.take() {
                Some(first) => format!("{first}; {why}"),
                None => why,
            });
        }
    }
    Ok(report)
}

/// The parts of a drill after its "before" messages.
struct Run<'a> {
    http: &'a reqwest::Client,
    plan: &'a Plan,
    tag: &'a str,
    own: &'a str,
    target: &'a str,
}

impl Run<'_> {
    /// Take the target down, then check the survivors take posts and this site notices.
    async fn lose(
        &self,
        survivors: &[&DrillSite],
        want: &mut BTreeSet<String>,
        report: &mut Report,
    ) -> Result<()> {
        let (http, tag, target) = (self.http, self.tag, self.target);
        step(
            &format!("Take {target} down now (pull its power, or stop its Pi)"),
            &self.plan.down,
        )
        .await?;
        let down = Instant::now();
        let mut survive = 0;
        for s in survivors {
            loop {
                let id = id()?;
                let text = format!("[{tag}] during at {} #1", s.node);
                match post(http, &s.url, &id, &text).await {
                    Ok(()) => {
                        want.insert(id);
                        survive = survive.max(elapsed_ms(down));
                        break;
                    }
                    Err(e) if down.elapsed() < self.plan.timeout => {
                        println!("{} not taking posts yet: {e:#}", s.node);
                        tokio::time::sleep(RETRY).await;
                    }
                    Err(e) => bail!("{} never took a post after the loss: {e:#}", s.node),
                }
            }
        }
        report.survive_ms = Some(survive);
        want.extend(phase(http, survivors, tag, "during", 2..self.plan.per_phase + 1).await?);
        println!(
            "Survivors took posts after {:.1} s; waiting for this site to notice",
            survive as f64 / 1000.0
        );
        while down.elapsed() < self.plan.timeout {
            if state_of(http, self.own, target).await? != "UP" {
                report.detect_ms = Some(elapsed_ms(down));
                break;
            }
            tokio::time::sleep(RETRY).await;
        }
        Ok(())
    }

    /// Bring the target back and post at it; returns when it came back.
    async fn bring_back(&self, site: &DrillSite, want: &mut BTreeSet<String>) -> Result<Instant> {
        let (http, timeout) = (self.http, self.plan.timeout);
        step(&format!("Bring {} back", self.target), &self.plan.up).await?;
        let back = Instant::now();
        loop {
            let ok = http
                .get(format!("{}/healthz", site.url))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ok {
                break;
            }
            if back.elapsed() > timeout {
                bail!(
                    "{} didn't come back within {} s",
                    self.target,
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        // its core needs a moment to take posts again
        loop {
            match phase(http, &[site], self.tag, "after", 1..self.plan.per_phase + 1).await {
                Ok(ids) => {
                    want.extend(ids);
                    return Ok(back);
                }
                Err(_) if back.elapsed() < timeout => tokio::time::sleep(RETRY).await,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Log the result as a signed event, so every site's sites page counts it.
pub async fn record(app: &App, report: &Report) -> Result<()> {
    let node = app.whoami().await.map(|w| w.node).unwrap_or_default();
    let e = app
        .core
        .append(crate::chat::DRILL, report.event(&node))
        .await?;
    app.apply_own(&e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drill_passes_only_with_nothing_lost_everything_checked_and_a_quick_survivor() {
        let good = Report {
            target: "tst-b".into(),
            sites: 3,
            sent: 12,
            survive_ms: Some(400),
            merge_ms: Some(9000),
            ..Default::default()
        };
        assert!(good.passed());
        assert!(good.text().ends_with(
            "PASSED
"
        ));
        let lost = Report {
            lost: 1,
            ..good_copy(&good)
        };
        assert!(!lost.passed());
        let slow = Report {
            survive_ms: Some(SURVIVE_LIMIT_MS + 1),
            ..good_copy(&good)
        };
        assert!(!slow.passed());
        let stopped = Report {
            error: Some("interrupted with Ctrl-C".into()),
            ..good_copy(&good)
        };
        assert!(!stopped.passed());
        assert!(stopped.text().contains("stopped early: interrupted"));
        assert_eq!(stopped.event("tst-a")["error"], "interrupted with Ctrl-C");
        let unchecked = Report {
            merge_ms: None,
            ..good_copy(&good)
        };
        assert!(!unchecked.passed());
        assert_eq!(good.event("tst-a")["survive_ms"], 400);
    }

    fn good_copy(r: &Report) -> Report {
        Report {
            target: r.target.clone(),
            sites: r.sites,
            sent: r.sent,
            survive_ms: r.survive_ms,
            merge_ms: r.merge_ms,
            ..Default::default()
        }
    }
}
