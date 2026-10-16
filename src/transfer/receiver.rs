//! Incoming transfer handling: stream upload to temp file, verify, rename.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::protocol::{canonical_hex, reserve_unique_path};
use crate::transfer::{now_secs, Direction, Status, TransferManager};

/// How often (bytes) the receive loop re-checks status and reports progress.
const PROGRESS_INTERVAL: u64 = 512 * 1024;

/// Emission cadence for progress SSE updates.
const EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often the system progress notification is refreshed.
const NOTIFY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

enum FailKind {
    Cancelled,
    AlreadyFailed,
    Failed,
}

/// Receive an upload body for an accepted transfer:
///
/// - validates state,
/// - streams the body to a temp file in the download dir (aborting early on
///   oversize bodies, cancel or sweep),
/// - verifies expected size (and SHA-256 when provided),
/// - atomically renames to a collision-safe final name,
/// - updates the transfer record and emits events.
///
/// On any failure the temp file is removed; a cancelled transfer stays
/// cancelled.
pub async fn receive_stream<B>(mgr: &TransferManager, id: &str, mut body: B) -> Result<PathBuf>
where
    B: tokio::io::AsyncRead + Unpin,
{
    // Snapshot expectations up front (lock order: download_dir, then transfers).
    let default_dir = mgr.download_dir.read().await.clone();
    let (filename, expected_size, expected_sha, save_dir, rel_dirs) = {
        let map = mgr.transfers.read().await;
        let t = map
            .get(id)
            .ok_or_else(|| anyhow!("unknown transfer {id}"))?;
        if t.direction != Direction::Incoming {
            bail!("transfer {id} is not incoming");
        }
        if t.status != Status::Accepted {
            bail!(
                "transfer {id} is not accepted (status={})",
                t.status.as_str()
            );
        }
        (
            t.filename.clone(),
            t.size,
            t.sha256.clone(),
            t.save_dir.clone(),
            t.rel_path
                .as_deref()
                .and_then(crate::protocol::sanitize_rel_dirs),
        )
    };
    // A per-transfer folder chosen by the user wins over the default.
    let dest_dir = save_dir.unwrap_or(default_dir);
    // Folder sends keep their shape: the sanitized segments were stripped of
    // anything that could leave the download directory.
    let dest_dir = match rel_dirs {
        Some(dirs) => dirs.iter().fold(dest_dir, |dir, part| dir.join(part)),
        None => dest_dir,
    };

    // Refuse the upload up front when it clearly cannot fit on disk.
    if expected_size > 0 {
        let free = crate::util::free_space(&dest_dir);
        if free > 0 && expected_size > free {
            bail!("not enough free disk space: need {expected_size} bytes");
        }
    }

    // Mark transferring.
    mgr.with_transfer(id, |t| {
        t.status = Status::Transferring;
        t.progress = 0.0;
        t.bytes_done = 0;
        t.started_at = Some(now_secs());
        t.updated_at = now_secs();
    })
    .await;
    mgr.emit_update(id, "transfer_updated").await;

    // Progress notifications (desktop / background mode only).
    let notify_on = mgr
        .interactive_prompts
        .load(std::sync::atomic::Ordering::Relaxed);
    let started = now_secs();
    if notify_on {
        crate::notify::progress(
            id,
            "LanDrop — receiving",
            &format!("{filename} — starting…"),
            0,
            false,
        );
    }

    // Temp file inside the download dir (same filesystem -> atomic rename).
    tokio::fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("create download dir {}", dest_dir.display()))?;
    let tmp = dest_dir.join(format!(".{filename}.part-{}", uuid::Uuid::new_v4()));
    mgr.metrics.transfer_started();

    let result = async {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("create temp file {}", tmp.display()))?;

        // Stream with size + sha256 computed on the fly.
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;
        let mut next_update: u64 = 0;
        let mut last_emit = Instant::now();
        let mut last_notify = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut body, &mut buf)
                .await
                .context("read upload body")?;
            if n == 0 {
                break;
            }
            done += n as u64;
            // Early abort: never write more than the offer declared. This also
            // covers `size: 0` offers, which must yield an empty file only.
            if done > expected_size {
                bail!("size mismatch: expected {expected_size} bytes");
            }
            AsyncWriteExt::write_all(&mut file, &buf[..n])
                .await
                .context("write temp file")?;
            hasher.update(&buf[..n]);

            if done >= next_update {
                next_update = done + PROGRESS_INTERVAL;
                // Abort when the transfer was cancelled or swept meanwhile.
                let current = {
                    let map = mgr.transfers.read().await;
                    map.get(id).map(|t| t.status)
                };
                match current {
                    Some(Status::Transferring) => {}
                    Some(other) => {
                        bail!("transfer {id} was interrupted (status={})", other.as_str())
                    }
                    None => bail!("transfer {id} disappeared"),
                }
                let p = if expected_size > 0 {
                    done as f64 / expected_size as f64
                } else {
                    0.0
                };
                mgr.with_transfer(id, |t| {
                    t.progress = p;
                    t.bytes_done = done;
                    t.updated_at = now_secs();
                })
                .await;
                if last_emit.elapsed() >= EMIT_INTERVAL {
                    mgr.emit_update(id, "transfer_updated").await;
                    last_emit = Instant::now();
                }
                if notify_on && last_notify.elapsed() >= NOTIFY_INTERVAL {
                    let pct = (p * 100.0).round().clamp(0.0, 100.0) as u8;
                    let elapsed = (now_secs() - started).max(1) as f64;
                    let speed = done as f64 / elapsed;
                    let eta = if speed > 0.0 && expected_size > done {
                        Some(((expected_size - done) as f64 / speed).ceil() as u64)
                    } else {
                        None
                    };
                    let body = match eta {
                        Some(secs) => format!(
                            "{filename} — {pct}% · ~{} left",
                            crate::util::human_duration(secs)
                        ),
                        None => format!("{filename} — {pct}%"),
                    };
                    crate::notify::progress(id, "LanDrop — receiving", &body, pct, false);
                    last_notify = Instant::now();
                }
            }
        }
        AsyncWriteExt::flush(&mut file)
            .await
            .context("flush temp file")?;

        // Size check (also enforces zero-byte offers).
        if done != expected_size {
            bail!("size mismatch: expected {expected_size} bytes, received {done}");
        }

        // Checksum check (the value was normalized when the offer arrived).
        if let Some(expected) = &expected_sha {
            let got = format!("{:x}", hasher.finalize());
            if canonical_hex(&got) != canonical_hex(expected) {
                bail!("checksum mismatch: expected {expected}, got {got}");
            }
        }

        file.sync_all().await.context("sync temp file")?;
        drop(file);

        // Reserve a collision-safe final name atomically, then publish.
        let final_path = tokio::task::spawn_blocking({
            let dir = dest_dir.clone();
            let name = filename.clone();
            move || reserve_unique_path(&dir, &name)
        })
        .await
        .context("join path reservation")??;
        if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
            let _ = tokio::fs::remove_file(&final_path).await;
            return Err(anyhow::Error::new(e).context(format!(
                "rename {} -> {}",
                tmp.display(),
                final_path.display()
            )));
        }

        Ok::<_, anyhow::Error>(final_path)
    }
    .await;

    match result {
        Ok(final_path) => {
            info!("received {} -> {}", filename, final_path.display());
            mgr.metrics.transfer_finished();
            mgr.metrics.add_received(expected_size);
            mgr.with_transfer(id, |t| {
                if t.status == Status::Transferring {
                    t.status = Status::Completed;
                    t.progress = 1.0;
                    t.saved_path = Some(final_path.clone());
                    t.updated_at = now_secs();
                }
            })
            .await;
            mgr.emit_update(id, "transfer_completed").await;
            if notify_on {
                crate::notify::progress(
                    id,
                    "LanDrop — received",
                    &format!("{filename} — saved to {}", final_path.display()),
                    100,
                    true,
                );
            } else {
                crate::notify::notify("LanDrop", &format!("Received '{}'", filename));
            }
            Ok(final_path)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            mgr.metrics.transfer_failed();
            warn!("receive failed: {e:#}");
            if notify_on {
                crate::notify::progress(
                    id,
                    "LanDrop — receive failed",
                    &format!("{filename} — failed"),
                    100,
                    true,
                );
            }
            let kind = mgr
                .with_transfer(id, |t| match t.status {
                    Status::Cancelled => FailKind::Cancelled,
                    Status::Failed => FailKind::AlreadyFailed,
                    _ => {
                        t.status = Status::Failed;
                        t.error = Some(e.to_string());
                        t.updated_at = now_secs();
                        FailKind::Failed
                    }
                })
                .await;
            match kind {
                Some(FailKind::Failed) => mgr.emit_update(id, "transfer_failed").await,
                Some(_) => mgr.emit_update(id, "transfer_updated").await,
                None => {}
            }
            Err(e)
        }
    }
}
