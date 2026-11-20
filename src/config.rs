//! `failover.toml`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// how people on the mesh reach this app, e.g. "http://tst-hilltop-pi.local.mesh:8799";
    /// other sites' directories send users here. Empty keeps this site out of them.
    #[serde(default)]
    pub public_url: String,
    /// shown by the MeshChat client, like MeshChat's own zone name
    #[serde(default = "d_zone")]
    pub zone: String,
    /// a copy of the stock MeshChat web client (the `www` folder of its repo), served at
    /// /meshchat/; empty serves this app's own pages only
    #[serde(default)]
    pub client_dir: Option<PathBuf>,
    /// newest messages handed to the MeshChat client per fetch; the chat page has them all
    #[serde(default = "d_per_fetch")]
    pub messages_per_fetch: usize,
    /// a message that reached this site this long after it was posted is marked as having
    /// crossed a split
    #[serde(default = "d_late")]
    pub late_after_s: u64,
    /// posts a minute one address may make (an upload counts as five, a new login one);
    /// everything posted is copied to every site for good. A node's NAT puts a whole
    /// site behind one address.
    #[serde(default = "d_posts")]
    pub posts_per_minute: u32,
    /// where /go sends people: "nearest" (this site while it works, else the nearest
    /// site that's up) or "home" (the placement's home site, else the nearest)
    #[serde(default = "d_directory")]
    pub directory: String,
    /// hosts to accept in other sites' addresses besides mesh names and 10.x / 44.x
    /// addresses (a simulator, say)
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub files: FilesCfg,
    #[serde(default)]
    pub peers: PeersCfg,
    #[serde(default)]
    pub placement: PlacementCfg,
    #[serde(default)]
    pub meshchat: MeshChatCfg,
    /// friendly site names: node name -> label, e.g. "tst-hilltop" = "Hilltop"
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
}

/// Where the ridgewarden core runs on this box.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to post anything
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    /// the core's data_dir: shared files go in through its `imports` folder and come out
    /// of its `exports` folder
    #[serde(default = "d_core_data")]
    pub data_dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FilesCfg {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// largest single upload; every site copies every file, so keep it small
    #[serde(default = "d_max_file")]
    pub max_file_kb: u64,
    /// all shared files together, across every site
    #[serde(default = "d_max_total")]
    pub max_total_kb: u64,
    /// fetch every shared file as soon as it's announced, so it survives the loss of the
    /// site that shared it; off fetches only when someone downloads it here
    #[serde(default = "d_true")]
    pub prefetch: bool,
}

/// How this site ranks as the home of single-site jobs (see placement.rs).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PlacementCfg {
    /// "grid", "generator", "solar", "battery" or "unknown"; steadier power ranks higher
    #[serde(default = "d_power")]
    pub power: String,
    /// the operator's preference, 0-100; 0 means never home
    #[serde(default = "d_weight")]
    pub weight: u8,
    /// a better site must be up this long without a break before it takes home back
    #[serde(default = "d_failback")]
    pub failback_s: u64,
}

/// MeshChat nodes to bridge with; only the home site talks to them.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct MeshChatCfg {
    /// e.g. "http://tst-hilltop.local.mesh:8080"
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default = "d_bridge_every")]
    pub interval_s: u64,
}

/// How this site judges the others. Defaults suit a core syncing every 120 s; with a
/// faster `sync.interval_s` in ridgewarden.toml, grace_s can come down.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PeersCfg {
    /// how often this site tells the others it's alive (and who is logged in)
    #[serde(default = "d_beat")]
    pub beat_s: u64,
    /// a site not heard from for this long is no longer counted as up
    #[serde(default = "d_grace")]
    pub grace_s: u64,
    /// over a path whose weakest link is below this quality (percent), a site gets twice
    /// the grace, so a lossy hop doesn't make it look dead
    #[serde(default = "d_weak")]
    pub weak_quality: f64,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-failover")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8799".parse().unwrap()
}
fn d_zone() -> String {
    "MeshChat".into()
}
fn d_per_fetch() -> usize {
    500
}
fn d_late() -> u64 {
    600
}
fn d_directory() -> String {
    "nearest".into()
}
fn d_power() -> String {
    "unknown".into()
}
fn d_weight() -> u8 {
    50
}
fn d_failback() -> u64 {
    900
}
fn d_bridge_every() -> u64 {
    30
}
fn d_posts() -> u32 {
    30
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_core_data() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden")
}
fn d_true() -> bool {
    true
}
fn d_max_file() -> u64 {
    2048
}
fn d_max_total() -> u64 {
    100 * 1024
}
fn d_beat() -> u64 {
    60
}
fn d_grace() -> u64 {
    360
}
fn d_weak() -> f64 {
    70.0
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
            data_dir: d_core_data(),
        }
    }
}

impl Default for FilesCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_kb: d_max_file(),
            max_total_kb: d_max_total(),
            prefetch: true,
        }
    }
}

impl Default for PlacementCfg {
    fn default() -> Self {
        Self {
            power: d_power(),
            weight: d_weight(),
            failback_s: d_failback(),
        }
    }
}

impl Default for MeshChatCfg {
    fn default() -> Self {
        Self {
            peers: vec![],
            interval_s: d_bridge_every(),
        }
    }
}

impl Default for PeersCfg {
    fn default() -> Self {
        Self {
            beat_s: d_beat(),
            grace_s: d_grace(),
            weak_quality: d_weak(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.public_url.is_empty() && !crate::chat::valid_url(&self.public_url) {
            bail!(
                "public_url must be an http:// or https:// URL, not {:?}",
                self.public_url
            );
        }
        if self.zone.trim().is_empty() || self.zone.len() > 64 {
            bail!("zone must be 1 to 64 characters");
        }
        if !matches!(self.directory.as_str(), "nearest" | "home") {
            bail!(
                "directory must be \"nearest\" or \"home\", not {:?}",
                self.directory
            );
        }
        if !crate::chat::POWER.contains(&self.placement.power.as_str()) {
            bail!(
                "placement.power must be one of {}, not {:?}",
                crate::chat::POWER.join(", "),
                self.placement.power
            );
        }
        if self.placement.weight > 100 {
            bail!("placement.weight is 0 to 100");
        }
        if self.meshchat.interval_s < 10 {
            bail!("meshchat.interval_s must be at least 10");
        }
        for p in &self.meshchat.peers {
            if !crate::chat::valid_url(p) {
                bail!("meshchat.peers entries are http:// URLs of MeshChat nodes, not {p:?}");
            }
        }
        if self.posts_per_minute == 0 {
            bail!("posts_per_minute must be at least 1");
        }
        if self.messages_per_fetch == 0 {
            bail!("messages_per_fetch must be at least 1");
        }
        if self.peers.beat_s < 10 {
            bail!("peers.beat_s must be at least 10");
        }
        if self.peers.grace_s < 2 * self.peers.beat_s {
            bail!("peers.grace_s must be at least two beats, so one lost beat isn't a dead site");
        }
        if !(0.0..=100.0).contains(&self.peers.weak_quality) {
            bail!("peers.weak_quality is a percentage, 0 to 100");
        }
        if self.files.max_file_kb == 0 || self.files.max_file_kb > self.files.max_total_kb {
            bail!("files.max_file_kb must be at least 1 and no more than files.max_total_kb");
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("failover.db")
    }

    /// Where finished shared files are kept, by blob root.
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("files")
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| ridgewarden_client::util::norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.files.max_file_kb * 1024
    }

    pub fn max_total_bytes(&self) -> u64 {
        self.files.max_total_kb * 1024
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-failover configuration
data_dir = "/var/lib/ridgewarden-failover"
listen = "0.0.0.0:8799"
# how people on the mesh reach this app; other sites' directories send users here
# public_url = "http://tst-hilltop-pi.local.mesh:8799"
zone = "MeshChat"
# a copy of the stock MeshChat web client (the www folder of github.com/hickey/meshchat),
# served at /meshchat/ so people keep the client they know
# client_dir = "/var/lib/ridgewarden-failover/meshchat-www"
messages_per_fetch = 500
late_after_s = 600
# per address; an upload counts as five. AREDN nodes usually NAT their LAN, so a whole
# site posts from one address: keep this well above what one person does
posts_per_minute = 30
# where /go sends people: "nearest" (this site while it works) or "home" (the placement's home)
directory = "nearest"
# hosts to accept in other sites' addresses besides mesh names and 10.x / 44.x addresses
allow_hosts = []

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
data_dir = "/var/lib/ridgewarden"

[files]
enabled = true
# every site keeps a copy of every file
max_file_kb = 2048
max_total_kb = 102400
# fetch every file as soon as it's shared, so it outlives the site that shared it
prefetch = true

[peers]
beat_s = 60
# with the core's default 120 s sync; with sync.interval_s = 15, 120 is plenty
grace_s = 360
weak_quality = 70

# how this site ranks as home for single-site jobs (the MeshChat bridge, directory = "home")
[placement]
# grid, generator, solar, battery or unknown
power = "unknown"
# 0 to 100; 0 means never home
weight = 50
# a better site must be up this long before it takes home back
failback_s = 900

# MeshChat nodes to exchange messages with; only the home site talks to them
[meshchat]
# peers = ["http://tst-hilltop.local.mesh:8080"]
peers = []
interval_s = 30

[site_names]
# "tst-hilltop" = "Hilltop"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen.port(), 8799);
    }

    #[test]
    fn grace_shorter_than_two_beats_is_refused() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.peers.grace_s = c.peers.beat_s;
        assert!(c.validate().is_err());
    }
}
