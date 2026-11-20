# ridgewarden-failover

[![CI](https://github.com/Londopy/ridgewarden-failover/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-failover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-failover)](https://github.com/Londopy/ridgewarden-failover/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-failover/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-failover)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**MeshChat that keeps working when the site hosting it goes dark.**

A ridgewarden app for AREDN meshes. It is a MeshChat-compatible chat backend, with shared files and a status board, that runs at every site instead of one. Each message is a signed event in the site's [ridgewarden](https://github.com/Londopy/ridgewarden) core log, the cores copy their logs to every site they can reach, and each site serves the merged view. Lose a site and the others carry on, because there was never a single server. When the mesh splits, both sides keep working; when the link comes back, both sides merge and nothing is lost. People keep the MeshChat page they know.

It needs a ridgewarden core on the same box, and the cores at the different sites must trust each other (an invite does that). Spec sheet 02 of the AREDN app suite.

## Features

- Works with the stock MeshChat web client unchanged: login, chat, channels, the user list, files and the status page. The same chat is also on this app's own pages, which need no JavaScript.
- Messages are kept for good, in the core's main log. No 500-message cap, and nothing lives in `/tmp`, so a reboot loses nothing.
- Clients poll a version number every 15 s, as MeshChat does, and fetch messages only when it changes. Answers come from the app's own SQLite view, never from a full copy of another site's database.
- Every service says how it merges after a split, and the sites page shows it: chat is an append-only log (both sides' messages kept, every site in the same order), files are files (every upload kept, only the uploader deletes), and the status board is a mergeable document (newest edit shown, the other side's edit kept and flagged until someone settles it). Anything the last split left to sort out is listed there too.
- A message that reached this site long after it was posted is marked "arrived 14 min late: sent across a split". Each message shows the site that signed it; the call sign is whatever the person typed, as in MeshChat, and the page says it isn't checked.
- The pages say when the mesh is split: which chat sites this side can reach, which it can't, and that their changes will merge when the link returns.
- Sites are judged the way an RF mesh needs. A site counts as up while its heartbeats keep arriving, with several missed beats of grace, and twice that when the mesh path to it runs over a weak link. When a site goes quiet, the mesh's own link data tells "the mesh split" (no path) from "its Pi stopped" (path up) from "its chat app stopped" (its core still sends).
- Liveness never trusts another site's wall clock. Heartbeats carry a run id and a monotonic counter, and this site's core stamps when each arrived, so a site with its clock hours out is judged right, a backlog of old heartbeats delivered after a split can't make a dead site look alive, and the sites page says when a site's clock is off. If this box's own clock steps (NTP arriving on a Pi with no clock battery), the app notices and moves the arrival times it recorded on the old clock.
- A site's node is the one its own core polls on its LAN, not what its heartbeat claims, so one station can't pose as another site to win the directory or placement.
- `/go` on every site is a directory. It sends people to this site while its core works, and otherwise to the nearest site that's up; with `directory = "home"` it sends everyone to the home site.
- Placement picks one home site for jobs that must run in one place: the up site that reaches the most other chat sites, then the one with the steadiest power, then the operator's weight. Home fails over at once and fails back only after the better site has been up for `failback_s` (15 min by default), so a flapping link doesn't bounce it.
- A bridge to MeshChat nodes, so a mesh where some sites still run MeshChat works as one chat. It runs on the home site and uses MeshChat's own API (`messages_version`, `messages_raw` with its `Content-MD5`, `send_message`).
- Shared files go through the core's blob store. Every site fetches every file, up to a total you set (or only when someone asks, with `prefetch = false`), so a file stays downloadable when the site that shared it is down; the files page says how many sites hold each one.
- `ridgewarden-failover drill` is the "kill a mountaintop" drill as a tool: it takes one site down (by a command you give it, or by asking you to pull the plug), checks the others keep taking posts and notice, brings it back, and checks every site ends up with every message. Results are logged as signed events, so every site's sites page keeps the tally against the spec's goal of 20 drills with none lost.
- Posting needs no login, as in MeshChat, so each address may post 30 times a minute (an upload counts as five, and so does each new login one), at most 500 logins are kept per site, and an operator at any site can hide a message, file or board version everywhere with `ridgewarden-failover hide <id>`.
- The app answers only to mesh names, addresses, its own public name and `allow_hosts`, so another web page can't point its own domain at the Pi (DNS rebinding), and its pages tell browsers not to run scripts from anywhere else.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-failover/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-failover/releases/latest): `sudo apt install ./ridgewarden-failover_<version>_arm64.deb` (`_amd64` on a PC). The install script checks the download against the release key your core already trusts. Either way `/etc/ridgewarden/failover.toml` is written if missing, and the service starts once the core is set up. The chat is on port 8799.

Do this at two or more sites. Then, in each site's `ridgewarden.toml`:

```toml
[sync]
interval_s = 15        # chat between sites is only as quick as this; the core's default is 120

[blobs]
window = ""            # fetch shared files at any hour
```

and make sure each core trusts the others (`ridgewarden invite` on one, `ridgewarden join` on the rest).

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-failover`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-failover ridgewarden-failover --config /etc/ridgewarden/failover.toml <command>`.

## Usage

```sh
ridgewarden-failover init --config /etc/ridgewarden/failover.toml
ridgewarden-failover run  --config /etc/ridgewarden/failover.toml
```

- `http://<pi>:8799/` is the chat page, `/board` the status board, `/files` the shared files, and `/sites` every chat site, the home site, each service's merge rule, drill results and the MeshChat bridge.
- `http://<pi>:8799/go` is the link to hand out. Bookmark it on any site: it always lands on a site that works. `/go/meshchat` does the same for the MeshChat client.
- `http://<pi>:8799/meshchat/` is the stock MeshChat client, once `client_dir` is set (below).
- `/api/sites` and `/api/services` return the sites view and the services with their open items as JSON.

Advertise the chat on your node with a link to `http://<pi>:8799/go/meshchat` on every site's node, under a name other than the MeshChat zone name. AREDN takes an hour or more to drop a dead service from its list, so don't move the advert when a site fails: the link stays the same and `/go` does the moving. A service named after the zone makes MeshChat nodes try their own sync against it; use the bridge (below) for MeshChat nodes instead.

### Hiding something

To take something off the air at every site (Part 97 still applies to what people type), find its id under the message, file or board entry and run:

```sh
sudo -u ridgewarden ridgewarden-failover --config /etc/ridgewarden/failover.toml hide 1a2b3c4d5e6f
```

The hide is a signed event like any other, so every site that trusts this one hides it too. Run it as the service user (or root), since it needs the core's token.

### The drill

Run it at any site except the one going down. With commands, it does everything itself:

```sh
ridgewarden-failover --config /etc/ridgewarden/failover.toml drill --kill tst-hilltop \
  --down "ssh pi@tst-hilltop-pi sudo systemctl stop ridgewarden ridgewarden-failover" \
  --up   "ssh pi@tst-hilltop-pi sudo systemctl start ridgewarden ridgewarden-failover"
```

Without `--down` and `--up` it tells you when to pull the plug and when to put it back, and waits for Enter, which is the real "kill a mountaintop" test. It posts a few tagged messages (channel `drill`, call sign `DRILL`) at every site before, during and after, and prints how long the survivors took to keep taking posts (the spec asks for 60 s), how long this site took to notice, how long the merge took, and how many messages were lost. It exits non-zero if anything was lost or a survivor was too slow. `--no-record` skips logging the result.

### The status board

`/board` holds short named entries ("Net control", "Shelter at the school") anyone can update from any site. Each update records which versions its writer saw. If two people update the same entry on different sides of a split, both versions are kept: the newer one (by its writer's clock) is shown and the other is listed under it until someone writes an update having seen both. Saving an entry empty clears it.

### The stock MeshChat client

The client isn't bundled: two of its files (`md5.js` and `alert.mp3`) carry no clear license. To serve it, copy the `www` folder of [hickey/meshchat](https://github.com/hickey/meshchat) (GPL-3.0, built on Trevor Paskett's MIT-licensed original) to the Pi and point `client_dir` at it:

```sh
git clone --depth 1 https://github.com/hickey/meshchat /tmp/meshchat
sudo cp -r /tmp/meshchat/www /var/lib/ridgewarden-failover/meshchat-www
```

The client calls `/cgi-bin/meshchat` and `/meshchat/` on whatever host served it, and this app answers both.

### Bridging to MeshChat nodes

List the MeshChat nodes under `[meshchat] peers` on every site; only the current home site talks to them. Every `interval_s` it asks each node for its `messages_version`, copies in anything new from `messages_raw` (checked against its `Content-MD5`), and sends messages posted on this mesh with `send_message`, keeping their ids. A message that comes back with an id and call sign already here is recognised as the same message, even when MeshChat has changed its text (it doesn't escape backslashes), so nothing goes around twice. Messages go to MeshChat HTML-escaped, since its client shows them as HTML. Messages from MeshChat show "via MeshChat on <node>".

What goes out is what reached this site in the last hour and the node doesn't already have, so a site that becomes home by failover sends what was posted while home was moving, without replaying history MeshChat has trimmed. What comes in is at most 50 messages per round and 600 an hour per node, since everything copied in is kept at every site for good. User lists and files aren't bridged.

### MeshChat API

`/cgi-bin/meshchat` takes `action` and its parameters from the query string or a urlencoded body, like the Lua MeshChat, and always answers HTTP 200 with `{"status":500,"response":"..."}` on failure. Supported actions:

| Action | Notes |
|---|---|
| `config` | `node` is this site's node, `zone` from the config, `platform` "pi" |
| `messages_version_ui` | `{"messages_version": n}`; with `call_sign` and `id` it also marks the user as online here |
| `messages` | newest first, at most `messages_per_fetch`; add `since=<messages_version>` to get only newer ones |
| `send_message` | `message`, `call_sign`, `channel`, `id` (the client's 8-hex id, kept so its own copy isn't shown twice) |
| `users` | people online here and at other sites, from the sites' heartbeats |
| `files`, `file_download`, `upload_file`, `delete_file` | multipart field `uploadfile`; download links point at the site the client is using |
| `sync_status` | when each other site was last heard from |
| `action_log` | always `[]` |
| `messages_download` | every message in MeshChat's tab-separated format |

Differences from MeshChat:

- `send_message`, `upload_file` and `delete_file` need the `X-Requested-With: XMLHttpRequest` header. The stock client's jQuery sends it, and another web page can't, so a stranger's page can't post through a visitor's browser. They are also limited per address (`posts_per_minute`).
- Messages go out HTML-escaped, because the stock client puts them into its page as HTML. Channel and file names are limited to letters, digits, spaces and `. _ - ( ) # + ,` for the same reason, and the client's pages are served with a Content-Security-Policy that only runs scripts from this app.
- `messages_version` is a counter that goes up with every new or hidden message here, not a sum of ids.
- The node-to-node sync actions (`messages_raw`, `users_raw`, `local_files_raw`, `messages_version`, `messages_md5`, `hosts`, `hosts_raw`, `meshchat_nodes`) aren't served; sites sync through their cores, and the bridge talks to MeshChat nodes from this side.
- A file name shared at two sites shows the newest upload in the MeshChat client; the files page here lists every version.

## Configuration

`/etc/ridgewarden/failover.toml` (`init` writes a commented one):

```toml
listen = "0.0.0.0:8799"
public_url = "http://tst-hilltop-pi.local.mesh:8799"   # where other sites' directories send people
zone = "MeshChat"
client_dir = "/var/lib/ridgewarden-failover/meshchat-www"
messages_per_fetch = 500     # per fetch by the MeshChat client; the chat page pages through everything
late_after_s = 600           # later than this and a message is marked as having crossed a split
posts_per_minute = 30        # per address; an upload counts as five, a new login one
directory = "nearest"        # or "home": where /go sends people
allow_hosts = []             # other sites' addresses must be mesh hosts unless listed here

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # lets this app post through the core
data_dir = "/var/lib/ridgewarden"                 # shared files go via its imports/exports folders

[files]
enabled = true
max_file_kb = 2048           # every site keeps a copy of every file
max_total_kb = 102400
prefetch = true              # false: fetch a file only when someone downloads it here

[peers]
beat_s = 60                  # heartbeat, also carries who is logged in here
grace_s = 360                # a site not heard from for this long isn't counted as up
weak_quality = 70            # paths with a link below this (percent) get twice the grace

[placement]
power = "grid"               # grid, generator, solar, battery or unknown
weight = 50                  # 0-100; 0 means never home
failback_s = 900             # a better site must be up this long before home moves back

[meshchat]
peers = ["http://tst-hilltop.local.mesh:8080"]
interval_s = 30

[site_names]
"tst-hilltop" = "Hilltop"
```

AREDN nodes usually NAT their LAN, so a whole site may post from one address; keep `posts_per_minute` well above what one person does.

`grace_s` has to cover the core's sync interval plus a couple of missed beats. With `sync.interval_s = 15` in the core, 120 is plenty, and that also bounds how long `directory = "home"` takes to move people off a dead home site.

Other sites' addresses (from their heartbeats) are used only if they're mesh hosts: `*.local.mesh`, a bare node name, or a 10.x or 44.x address. Add anything else, such as a test setup on 127.0.0.1, to `allow_hosts`.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`.

## Limits

- Drills have been run against the simulator that ships with the core, with three sites; none on real Pis over real RF yet.
- The drill with a club, on real mountaintops, isn't software: the `drill` command is the tool for it.
- Winlink through Pat isn't here: the spec makes it conditional on a club asking for it.
- Asterisk and cameras aren't here: the spec lists them as services that can't move (phones must re-register; cameras are where they are).
- Liveness assumes this box's clock only steps, not drifts badly; a site that restarted on the far side of a split and then died is judged from the backlog it left.
- A site's node is known only once its core's LAN polls arrive; a site whose core doesn't share telemetry (a data mule) shows by call sign and gets no mesh path.
- Messages are ordered by the poster's clock (capped at 10 minutes ahead of the receiver's), not corrected by the clock error the sites page measures: that estimate moves as heartbeats arrive, and correcting by it would make different sites order the same history differently.
- The MeshChat bridge copies messages only, not MeshChat's user lists or files.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-failover
cd ridgewarden-failover
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
