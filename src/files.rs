//! Shared files, MeshChat's file page, carried by the core's blob store: the uploading
//! site publishes the file as a blob and logs a `failover.file` event naming it; every
//! other site asks its core to fetch the blob, so the file is still there when the
//! uploading site isn't.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::App;
use crate::chat::{self, SharedFile};

/// Blob priority for chat files: small, and someone is waiting for them, so ahead of
/// bulk downloads like map tiles.
const PRIORITY: u8 = 3;

/// Why an upload or delete was turned down, in words for the person who tried.
/// Anything else that goes wrong is this site's problem and goes to its log.
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refuse<T>(why: impl Into<String>) -> Result<T> {
    Err(Refused(why.into()).into())
}

/// What to tell the person: the refusal, or a plain line while the details go to the log.
pub fn user_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<Refused>() {
        Some(r) => r.0.clone(),
        None => {
            tracing::warn!("shared file: {e:#}");
            "This site couldn't store or reach the file just now; try again, or use another site"
                .into()
        }
    }
}

/// Remove a scratch file, saying so if that fails; nothing depends on it being gone.
fn tidy(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("removing {}: {e}", path.display());
    }
}

fn random_hex() -> Result<String> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).context("no randomness from the OS")?;
    Ok(hex::encode(b))
}

fn cache_path(app: &App, root: &str) -> PathBuf {
    app.cfg.cache_dir().join(root)
}

/// Write `data` to `path` without anyone seeing half a file.
fn write_whole(path: &PathBuf, data: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let tmp = path.with_extension(format!("{}.part", random_hex()?));
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Share an uploaded file with every site. The error text is shown to the uploader.
pub async fn upload(app: &App, name: &str, data: &[u8], call: &str) -> Result<()> {
    if !app.cfg.files.enabled {
        return refuse("File sharing is turned off on this site");
    }
    let Some(name) = chat::clean_file_name(name) else {
        return refuse(
            "That file name can't be shared; use letters, digits, spaces and . _ - ( ) # + ,",
        );
    };
    if data.is_empty() {
        return refuse("The file is empty");
    }
    if data.len() as u64 > app.cfg.max_file_bytes() {
        return refuse(format!(
            "Files are limited to {} KB here, so every site can keep a copy",
            app.cfg.files.max_file_kb
        ));
    }
    let used = chat::used_bytes(&app.db(), &app.me().0)? as u64;
    if used + data.len() as u64 > app.cfg.max_total_bytes() {
        return refuse("Not enough storage, delete some files");
    }
    let imports = app.cfg.core.data_dir.join("imports");
    let file = format!("failover-{}.upload", random_hex()?);
    let path = imports.join(&file);
    write_whole(&path, data)?;
    let added = app
        .core
        .blob_add(&file, &format!("meshchat/{name}"), PRIORITY)
        .await;
    // the core has its own copy in its chunk store now, or failed
    tidy(&path);
    let root = added?;
    write_whole(&cache_path(app, &root), data)?;
    let node = app.whoami().await.map(|w| w.node).unwrap_or_default();
    let e = app
        .core
        .append(
            chat::FILE,
            json!({"root": root, "name": name, "size": data.len(), "call_sign": call, "node": node}),
        )
        .await?;
    app.apply_own(&e)
}

/// Delete a file. Only the site it was uploaded at may, as with MeshChat's own files.
pub async fn delete(app: &App, f: &SharedFile) -> Result<()> {
    let (me, _) = app.me();
    if f.author_key != me {
        let at = if f.node.is_empty() {
            "another site"
        } else {
            &f.node
        };
        return refuse(format!(
            "Only the site it was uploaded at ({at}) can delete this file"
        ));
    }
    let e = app
        .core
        .append(chat::FILE_GONE, json!({"root": f.root, "name": f.name}))
        .await?;
    app.apply_own(&e)?;
    tidy(&cache_path(app, &f.root));
    Ok(())
}

pub enum Fetch {
    Ready(PathBuf),
    /// not here yet; why, for the person waiting
    Pending(String),
}

/// A shared file on this site's disk, getting it out of the core's blob store if needed.
pub async fn local_copy(app: &App, f: &SharedFile) -> Result<Fetch> {
    let cached = cache_path(app, &f.root);
    if cached.is_file() {
        return Ok(Fetch::Ready(cached));
    }
    let from = if f.node.is_empty() {
        "the site that shared it"
    } else {
        &f.node
    };
    let export = format!("failover-{}.bin", random_hex()?);
    let path = match app.core.blob_export(&f.root, &export).await {
        Ok(p) => PathBuf::from(p),
        Err(e) => {
            tracing::debug!("export of {} not ready: {e:#}", f.root);
            want(app, &f.root, f.size).await?;
            return Ok(Fetch::Pending(format!(
                "{} is still being copied to this site from {from}. Try again in a few minutes.",
                f.name
            )));
        }
    };
    let len = std::fs::metadata(&path)
        .with_context(|| format!("checking {}", path.display()))?
        .len();
    // the size came from another station's event; the blob is what's real
    if len as i64 != f.size || len > app.cfg.max_file_bytes() {
        tidy(&path);
        anyhow::bail!(
            "blob {} is {len} bytes but was shared as {}",
            f.root,
            f.size
        );
    }
    let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()));
    tidy(&path);
    write_whole(&cached, &data?)?;
    announce(app, &f.root).await?;
    Ok(Fetch::Ready(cached))
}

/// Ask the core to fetch a blob, once its manifest is here and says the size the file
/// was shared with, within this site's limit. Returns whether it was asked.
async fn want(app: &App, root: &str, size: i64) -> Result<bool> {
    let Some(st) = app.core.blob_status(root).await? else {
        return Ok(false);
    };
    if st.size as i64 != size || st.size > app.cfg.max_file_bytes() {
        tracing::warn!(
            "not fetching blob {root}: it is {} bytes, shared as {size}, limit {}",
            st.size,
            app.cfg.max_file_bytes()
        );
        return Ok(false);
    }
    app.core.blob_want(root).await?;
    Ok(true)
}

/// Tell the other sites this one now holds a complete copy, once.
async fn announce(app: &App, root: &str) -> Result<()> {
    let (me, _) = app.me();
    if me.is_empty() || chat::have(&app.db(), root, &me)? {
        return Ok(());
    }
    let e = app.core.append(chat::HAVE, json!({"root": root})).await?;
    app.apply_own(&e)
}

/// Drop copies of deleted files and, with `prefetch` on, ask the core for files other
/// sites shared and copy out the ones it has finished.
pub async fn fetch_round(app: &App) -> Result<()> {
    let (me, _) = app.me();
    if me.is_empty() || !app.cfg.files.enabled {
        return Ok(());
    }
    let gone = chat::deleted_roots(&app.db())?;
    for root in gone {
        // the core may never have had it; either way there's nothing left to keep
        if let Err(e) = app.core.blob_unwant(&root).await {
            tracing::debug!("letting go of blob {root}: {e:#}");
        }
        tidy(&cache_path(app, &root));
        chat::mark_released(&app.db(), &root)?;
    }
    if !app.cfg.files.prefetch {
        return Ok(());
    }
    // files already asked for: copy out the ones the core has finished, so this site
    // can say it holds them. One bad file mustn't hold up the rest.
    let list = chat::files(&app.db())?;
    for f in list {
        if f.wanted
            && f.author_key != me
            && !cache_path(app, &f.root).is_file()
            && let Err(e) = local_copy(app, &f).await
        {
            tracing::warn!("shared file {} ({}): {e:#}", f.name, f.root);
        }
    }
    let todo = chat::unwanted(&app.db(), &me, app.cfg.max_file_bytes() as i64)?;
    let mut held = chat::held_bytes(&app.db(), &me)?;
    for (root, size) in todo {
        if held + size > app.cfg.max_total_bytes() as i64 {
            tracing::warn!("not fetching shared file {root}: files.max_total_kb is reached");
            break;
        }
        match want(app, &root, size).await {
            Ok(true) => {
                chat::mark_wanted(&app.db(), &root)?;
                held += size;
            }
            // usually the blob's manifest hasn't synced yet; next round
            Ok(false) => {}
            Err(e) => tracing::debug!("blob {root} not wanted yet: {e:#}"),
        }
    }
    Ok(())
}
