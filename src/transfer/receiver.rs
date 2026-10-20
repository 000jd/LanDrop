//! Incoming transfer handling: stream upload to a deterministic partial file,
//! verify, rename — and keep the partial file across a broken connection so the
//! sender can resume from the byte offset reported by `HEAD /api/v1/upload/{id}`.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::protocol::{canonical_hex, reserve_unique_path};
use crate::transfer::{
    destination_dir, now_secs, part_path_for, Direction, Status, TransferManager,
};

/// How often (bytes) the receive loop re-checks status and reports progress.
const PROGRESS_INTERVAL: u64 = 512 * 1024;

/// Emission cadence for progress SSE updates.
const EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often the system progress notification is refreshed.
const NOTIFY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Size of the chunks used both for the upload stream and for re-feeding an
/// existing partial file into the SHA-256 hasher.
const CHUNK: usize = 64 * 1024;

enum FailKind {
    /// The transfer already reached a terminal state (cancelled, rejected,
    /// expired, completed) — leave that state untouched.
    Terminal,
    /// A new failure that this receive attempt must record.
    Failed,
}

/// Receive an upload body for an accepted transfer:
///
/// - validates state and claims the single upload slot,
/// - streams the body to `.{filename}.part-{id}` in the destination directory
///   (or resumes at `offset`, re-hashing the existing prefix first),
/// - verifies expected size (and SHA-256 when provided),
/// - atomically renames to a collision-safe final name,
/// - updates the transfer record and emits events.
///
/// A read error after some bytes arrived keeps the partial file on disk and
/// leaves the transfer in `Transferring`, so a retry can resume. Protocol
/// violations (overflow, checksum mismatch, clean EOF before the declared size)
/// delete the partial file, burn the upload token and fail the transfer.
pub async fn receive_stream<B>(
    mgr: &TransferManager,
    id: &str,
    body: B,
    offset: u64,
) -> Result<PathBuf>
where
    B: tokio::io::AsyncRead + Unpin,
{
    let result = receive_stream_inner(mgr, id, body, offset).await;
    // Always release the upload slot, whatever happened.
    mgr.reset_uploading(id).await;
    result
}

async fn receive_stream_inner<B>(
    mgr: &TransferManager,
    id: &str,
    mut body: B,
    offset: u64,
) -> Result<PathBuf>
where
    B: tokio::io::AsyncRead + Unpin,
{
    // Snapshot expectations up front (lock order: download_dir, then transfers).
    let default_dir = mgr.download_dir.read().await.clone();
    let (filename, expected_size, expected_sha, dest_dir, part_path, previous_status) = {
        let map = mgr.transfers.read().await;
        let t = map
            .get(id)
            .ok_or_else(|| anyhow!("unknown transfer {id}"))?;
        if t.direction != Direction::Incoming {
            bail!("transfer {id} is not incoming");
        }
        if !matches!(t.status, Status::Accepted | Status::Transferring) {
            bail!(
                "transfer {id} is not accepted (status={})",
                t.status.as_str()
            );
        }
        let dest_dir = destination_dir(&default_dir, t.save_dir.as_deref(), t.rel_path.as_deref());
        let part_path = t.part_path.clone().unwrap_or_else(|| {
            part_path_for(
                &default_dir,
                t.save_dir.as_deref(),
                t.rel_path.as_deref(),
                &t.filename,
                id,
            )
        });
        (
            t.filename.clone(),
            t.size,
            t.sha256.clone(),
            dest_dir,
            part_path,
            t.status,
        )
    };

    // Refuse the upload up front when it clearly cannot fit on disk.
    let remaining = expected_size.saturating_sub(offset);
    if remaining > 0 {
        let free = crate::util::free_space(&dest_dir);
        if free > 0 && remaining > free {
            bail!("not enough free disk space: need {remaining} bytes");
        }
    }

    // Mark transferring. Resuming keeps the same status but starts a fresh
    // speed window at the offset already on disk.
    let first_attempt = previous_status == Status::Accepted;
    mgr.with_transfer(id, |t| {
        t.status = Status::Transferring;
        t.progress = if expected_size > 0 {
            offset as f64 / expected_size as f64
        } else {
            0.0
        };
        t.bytes_done = offset;
        t.started_at = Some(std::time::Instant::now());
        t.updated_at = now_secs();
        t.part_path = Some(part_path.clone());
    })
    .await;
    mgr.emit_update(id, "transfer_updated").await;
    if first_attempt {
        mgr.metrics.transfer_started();
    }

    // Progress notifications (desktop / background mode only).
    let notify_on = mgr
        .interactive_prompts
        .load(std::sync::atomic::Ordering::Relaxed);
    let started = std::time::Instant::now();
    if notify_on {
        crate::notify::progress(
            id,
            "LanDrop — receiving",
            &format!("{filename} — starting…"),
            0,
            false,
        );
    }

    tokio::fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("create download dir {}", dest_dir.display()))?;

    // Set when the body could not be read any further, as opposed to a
    // protocol violation or a cancel. Those are the only cases that keep the
    // partial file around for a resume.
    let mut interrupted = false;
    // Set when the transfer stopped being ours while streaming (cancelled,
    // rejected, expired or completed elsewhere).
    let mut terminal = false;

    let result = async {
        // Open the partial file at the requested position. For a resume, the
        // existing prefix is fed back into the hasher before appending.
        let (mut file, mut hasher, mut done) = if offset == 0 {
            let mut opts = tokio::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let file = opts
                .open(&part_path)
                .await
                .with_context(|| format!("create partial file {}", part_path.display()))?;
            (file, Sha256::new(), 0u64)
        } else {
            let on_disk = tokio::fs::metadata(&part_path)
                .await
                .with_context(|| format!("stat partial file {}", part_path.display()))?
                .len();
            if on_disk != offset {
                bail!(
                    "resume offset mismatch: partial file has {on_disk} bytes, request wants {offset}"
                );
            }
            let mut existing = tokio::fs::File::open(&part_path)
                .await
                .with_context(|| format!("open partial file {}", part_path.display()))?;
            let mut hasher = Sha256::new();
            let mut fed: u64 = 0;
            let mut buf = vec![0u8; CHUNK];
            while fed < offset {
                let want = ((offset - fed) as usize).min(CHUNK);
                let n = tokio::io::AsyncReadExt::read(&mut existing, &mut buf[..want])
                    .await
                    .context("read partial file")?;
                if n == 0 {
                    bail!(
                        "resume offset mismatch: partial file ended at {fed} bytes, wanted {offset}"
                    );
                }
                hasher.update(&buf[..n]);
                fed += n as u64;
            }
            drop(existing);
            let file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part_path)
                .await
                .with_context(|| format!("append to partial file {}", part_path.display()))?;
            (file, hasher, offset)
        };

        // Stream with size + sha256 computed on the fly.
        let mut next_update: u64 = done + PROGRESS_INTERVAL;
        let mut last_emit = Instant::now();
        let mut last_notify = Instant::now();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match tokio::io::AsyncReadExt::read(&mut body, &mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    // The peer went away: keep what is on disk and let the
                    // sender resume from `done`.
                    let _ = AsyncWriteExt::flush(&mut file).await;
                    mgr.with_transfer(id, |t| {
                        t.bytes_done = done;
                        t.updated_at = now_secs();
                    })
                    .await;
                    interrupted = true;
                    return Err(anyhow::Error::new(e).context("read upload body"));
                }
            };
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
                .context("write partial file")?;
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
                    Some(
                        status @ (Status::Cancelled
                        | Status::Rejected
                        | Status::Failed
                        | Status::Completed),
                    ) => {
                        terminal = true;
                        bail!(
                            "transfer {id} was interrupted (status={})",
                            status.as_str()
                        )
                    }
                    Some(other) => {
                        bail!(
                            "transfer {id} stopped streaming (status={})",
                            other.as_str()
                        )
                    }
                    None => {
                        terminal = true;
                        bail!("transfer {id} disappeared");
                    }
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
                    let elapsed = started.elapsed().as_secs_f64().max(0.001);
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
            .context("flush partial file")?;

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

        file.sync_all().await.context("sync partial file")?;
        drop(file);

        // Reserve a collision-safe final name atomically, then publish.
        let final_path = tokio::task::spawn_blocking({
            let dir = dest_dir.clone();
            let name = filename.clone();
            move || reserve_unique_path(&dir, &name)
        })
        .await
        .context("join path reservation")??;
        if let Err(e) = tokio::fs::rename(&part_path, &final_path).await {
            let _ = tokio::fs::remove_file(&final_path).await;
            return Err(anyhow::Error::new(e).context(format!(
                "rename {} -> {}",
                part_path.display(),
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
                    t.bytes_done = expected_size;
                    t.saved_path = Some(final_path.clone());
                    // A completed transfer can never be resumed: burn the token.
                    t.upload_token = None;
                    t.part_path = None;
                    t.updated_at = now_secs();
                }
            })
            .await;
            mgr.emit_update(id, "transfer_completed").await;
            mgr.persist_history().await;
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
            // A broken connection leaves the partial file in place so the
            // sender can HEAD it and resume. Everything else is final.
            if interrupted && !terminal {
                warn!(
                    "receive interrupted: {e:#} — kept {} for resume",
                    part_path.display()
                );
                mgr.emit_update(id, "transfer_updated").await;
                return Err(e);
            }

            let _ = tokio::fs::remove_file(&part_path).await;
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
                    Status::Cancelled | Status::Rejected | Status::Failed | Status::Completed => {
                        FailKind::Terminal
                    }
                    _ => {
                        t.status = Status::Failed;
                        t.error = Some(e.to_string());
                        t.updated_at = now_secs();
                        // Protocol violation / unrecoverable write error: the
                        // upload token must never be reusable.
                        t.upload_token = None;
                        t.part_path = None;
                        FailKind::Failed
                    }
                })
                .await;
            match kind {
                Some(FailKind::Failed) => {
                    mgr.emit_update(id, "transfer_failed").await;
                    mgr.persist_history().await;
                }
                Some(FailKind::Terminal) => mgr.emit_update(id, "transfer_updated").await,
                None => {}
            }
            Err(e)
        }
    }
}
