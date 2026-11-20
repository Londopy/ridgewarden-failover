# Changelog

All notable changes to ridgewarden-failover are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Runs as its own system user, `ridgewarden-failover`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- MeshChat-compatible chat at every site: each message is a signed event in the ridgewarden core's main log, and each site serves the merged view, so losing a site loses neither the chat nor its history.
- The MeshChat API at `/cgi-bin/meshchat` (config, messages_version_ui, messages, send_message, users, files, file_download, upload_file, delete_file, sync_status, action_log, messages_download), so the stock MeshChat web client works unchanged; `client_dir` serves a copy of it at `/meshchat/`.
- Both sides of a split keep taking posts and merge with nothing lost when the link returns; every site shows the same order (timestamp, then event id), and messages that crossed a split are marked as arriving late.
- No message cap: everything is kept, and the chat page pages through all of it. The MeshChat client polls a version number and fetches only on change.
- The split shown on every page: which chat sites this side can reach, which it can't, and that their messages will merge.
- Site health judged from heartbeats and core polls with RF-sized grace, doubled over weak links, and the mesh's link data to tell a stopped site from a split.
- `/go` and `/go/meshchat` on every site: a directory that sends people to this site while its core works, otherwise to the nearest site that's up.
- Shared files through the core's blob store, copied to every site; only the uploading site can delete one.
- A chat page, files page and sites page that work without JavaScript, and `/api/sites` as JSON.
- `ridgewarden-failover hide <id>` hides a message, shared file or board version at every site, and posting is limited per address (12 messages a minute by default).
- A status board (`/board`): named entries anyone can update from any site, merged as a document; edits made on both sides of a split are both kept, the newest shown and the other flagged until someone settles it.
- Every service declares how it merges (append-only log, mergeable document or files), its rule for who wins on rejoin, and where it runs; the sites page and `/api/services` list them with anything a split left to sort out.
- Placement: a home site for single-site jobs, chosen by mesh connectivity, power source and operator weight, failing over at once and failing back only after `failback_s` up (hysteresis). `directory = "home"` sends everyone to it.
- A bridge to MeshChat nodes over MeshChat's own API (`messages_version`, `messages_raw` checked against its `Content-MD5`, `send_message`), run by the home site, so mixed meshes work as one chat.
- `ridgewarden-failover drill`: takes one site down (by command, or by asking you), checks the others keep taking posts and nothing is lost when it returns, and logs the result so every site's sites page shows the tally.
- Site liveness from each heartbeat's run counter and arrival time, never the sender's wall clock; the sites page warns when a site's clock is off, and tells "its chat app stopped" apart from "its Pi stopped" and "the mesh split".
- The files page shows how many sites hold each file; `files.prefetch = false` fetches only on demand.
- A site's node comes from its core's own LAN polls, a heartbeat dated in the future can't freeze a site's details, and a step of this box's clock is caught and corrected for.
- Fail-back needs the challenger to rank higher without a break for `failback_s`, so a flapping link doesn't move home.
- The drill always brings the site back and records the result, even when a step fails or it's stopped with Ctrl-C.
- The MeshChat bridge escapes what it sends, recognises its own messages by id and call sign, caps what it copies in, and sends what arrived in the last hour that the node lacks.
- Shared files: one bad file no longer stops the others, blob sizes are checked before fetching and before reading, deleted files are released from the core, and the quota counts only files this site holds.
- New logins count against the per-address limit (default raised to 30 a minute for NATed sites), at most 500 logins are kept, and file deletes are limited too.
- The app refuses requests for unknown host names (DNS rebinding) and sends nosniff and a Content-Security-Policy on every page.
- Other sites' addresses are used only if they're mesh hosts (`*.local.mesh`, node names, 10.x, 44.x) or listed in `allow_hosts`.

[Unreleased]: https://github.com/Londopy/ridgewarden-failover/commits/main
