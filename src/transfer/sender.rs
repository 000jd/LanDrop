//! Outgoing transfer driver: offer -> poll -> upload with progress.
//!
//! [`spawn_send`] is called by the local `/api/send` endpoint. It:
//!
//! 1. posts an [`Offer`] to the receiver (the caller has already staged the
//!    file to a private staging path),
//! 2. polls `GET /api/v1/offer/{id}` until accepted/rejected/timeout,
//! 3. streams the file to the receiver with progress events,
//! 4. removes the staged file on every outcome.
//!
//! Cancellation is signalled through a `watch` channel stored on the transfer.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::peers::Peer;
use crate::protocol::{Offer, TOKEN_HEADER, UPLOAD_TOKEN_HEADER};
use crate::transfer::{now_secs, Status, TransferManager};

/// Poll interval while waiting for acceptance.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Connect timeout for peer requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for the offer POST.
const OFFER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Deadline for a single status poll.
const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest peer response body we are willing to buffer.
const MAX_PEER_BODY: u64 = 64 * 1024;
/// Throughput floor assumed by the upload deadline (bytes per second).
const MIN_UPLOAD_BYTES_PER_SEC: u64 = 512 * 1024;
/// Progress bookkeeping cadence (bytes).
const PROGRESS_INTERVAL: u64 = 512 * 1024;

/// Generous upload deadline: 2 minutes plus one second per 512 KiB.
fn upload_timeout(size: u64) -> Duration {
    Duration::from_secs(120) + Duration::from_secs(size / MIN_UPLOAD_BYTES_PER_SEC)
}

/// Start an outgoing transfer in the background. Returns the transfer id.
///
/// `local_path` is the private staged copy of the file; it is removed once
/// the transfer reaches any terminal state.
pub async fn spawn_send(
    mgr: &TransferManager,
    peer: Peer,
    filename: String,
    size: u64,
    sha: Option<String>,
    local_path: PathBuf,
) -> Result<String> {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (my_id, my_name, _) = mgr.identity().await;

    let offer = Offer {
        sender_id: my_id,
        sender_name: my_name,
        filename,
        size,
        sha256: sha,
        note: String::new(),
    };

    let id = {
        let mut map = mgr.transfers.write().await;
        let mut t = crate::transfer::Transfer::new_outgoing(&peer, &offer);
        let id = t.id.clone();
        t.cancel = Some(cancel_tx);
        map.insert(id.clone(), t);
        id
    };
    mgr.emit_update(&id, "transfer_updated").await;
    mgr.metrics.transfer_started();

    let mgr = mgr.clone();
    let id_spawn = id.clone();
    let peer_for_reason = peer.clone();
    tokio::spawn(async move {
        if let Err(e) = run_send(&mgr, &id_spawn, peer, offer.clone(), local_path, cancel_rx).await
        {
            // Ask the receiver *why* it stopped before blaming the network: a
            // receiver-side cancel/reject deserves its own message in the UI.
            let reason = remote_status(&mgr, &peer_for_reason, &id_spawn).await;
            let message = match reason.as_deref() {
                Some("cancelled") => "stopped by the receiver (cancelled)".to_string(),
                Some("rejected") => "rejected by the receiver".to_string(),
                _ => e.to_string(),
            };
            let set_failed = {
                let mut map = mgr.transfers.write().await;
                match map.get_mut(&id_spawn) {
                    Some(t)
                        if matches!(
                            t.status,
                            Status::Preparing
                                | Status::Waiting
                                | Status::Accepted
                                | Status::Transferring
                        ) =>
                    {
                        t.status = Status::Failed;
                        t.error = Some(message.clone());
                        t.updated_at = now_secs();
                        true
                    }
                    _ => false,
                }
            };
            if set_failed {
                mgr.metrics.transfer_failed();
                error!("transfer {id_spawn} failed: {e:#}");
                mgr.emit_update(&id_spawn, "transfer_failed").await;
            } else {
                info!("transfer {id_spawn} stopped: {e:#}");
            }
        } else {
            mgr.metrics.transfer_finished();
            mgr.metrics.add_sent(offer.size);
        }
    });

    Ok(id)
}

/// One last look at the offer status, used to explain a stopped upload.
async fn remote_status(mgr: &TransferManager, peer: &Peer, id: &str) -> Option<String> {
    let url = format!("http://{}:{}/api/v1/offer/{}", peer.addr, peer.port, id);
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let shared = mgr.shared_token.read().await.clone();
    let mut req = client.get(&url).timeout(OFFER_REQUEST_TIMEOUT);
    if !shared.is_empty() {
        req = req.header(TOKEN_HEADER, shared);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("status")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

/// The full send lifecycle. Always removes the staged file, whatever happens.
async fn run_send(
    mgr: &TransferManager,
    id: &str,
    peer: Peer,
    offer: Offer,
    local_path: PathBuf,
    cancel_rx: watch::Receiver<bool>,
) -> Result<()> {
    let result = run_send_inner(mgr, id, &peer, &offer, &local_path, cancel_rx).await;
    if let Err(e) = tokio::fs::remove_file(&local_path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("failed to remove staged file {}: {e}", local_path.display());
        }
    }
    result
}

async fn run_send_inner(
    mgr: &TransferManager,
    id: &str,
    peer: &Peer,
    offer: &Offer,
    local_path: &Path,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<()> {
    let base = format!("http://{}:{}", peer.addr, peer.port);
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build HTTP client")?;
    let shared = mgr.shared_token.read().await.clone();

    // 1. Offer
    mgr.with_transfer(id, |t| {
        if t.status == Status::Preparing {
            t.status = Status::Waiting;
            t.updated_at = now_secs();
        }
    })
    .await;
    mgr.emit_update(id, "transfer_updated").await;
    let resp = post_json(&client, &format!("{base}/api/v1/offer"), offer, &shared).await?;
    let v: serde_json::Value = resp.json().await.context("offer response JSON")?;
    let remote_id = v["transfer_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("receiver returned no transfer_id"))?
        .to_string();

    // 2. Poll until accepted / rejected / timeout / cancel.
    let poll_url = format!("{base}/api/v1/offer/{remote_id}");
    let (upload_url, upload_token) =
        wait_for_acceptance(&client, &poll_url, &shared, &mut cancel_rx).await?;
    if *cancel_rx.borrow() {
        bail!("cancelled by user");
    }

    // 3. Upload with progress.
    let file = tokio::fs::File::open(local_path)
        .await
        .with_context(|| format!("open staged file {}", local_path.display()))?;
    let total = offer.size;

    mgr.with_transfer(id, |t| {
        if matches!(t.status, Status::Waiting | Status::Accepted) {
            t.status = Status::Transferring;
            t.progress = 0.0;
            t.bytes_done = 0;
            t.started_at = Some(now_secs());
            t.updated_at = now_secs();
        }
    })
    .await;
    mgr.emit_update(id, "transfer_updated").await;

    // Only trust an absolute upload URL that stays on the peer we chose.
    let url = if upload_url.starts_with("http://") || upload_url.starts_with("https://") {
        if !upload_url.starts_with(&base) {
            bail!("receiver returned an upload URL for a different host");
        }
        upload_url
    } else {
        format!("{base}{upload_url}")
    };

    // Stream the file in 64 KiB chunks through a channel, tracking progress
    // and cancellation, while reqwest consumes the channel.
    let (chunk_tx, chunk_rx) =
        tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    {
        let mgr = mgr.clone();
        let id = id.to_string();
        let mut reader = tokio::io::BufReader::new(file);
        let mut cancel_rx = cancel_rx.clone();
        let mut done: u64 = 0;
        let mut next_progress: u64 = 0;
        let mut last_emit = Instant::now();
        let mut buf = vec![0u8; 64 * 1024];
        tokio::spawn(async move {
            loop {
                if *cancel_rx.borrow() {
                    let _ = chunk_tx
                        .send(Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "cancelled",
                        )))
                        .await;
                    break;
                }
                let n = match tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = chunk_tx.send(Err(e)).await;
                        break;
                    }
                };
                if n == 0 {
                    break;
                }
                done += n as u64;
                if done >= next_progress {
                    next_progress = done + PROGRESS_INTERVAL;
                    let p = if total > 0 {
                        done as f64 / total as f64
                    } else {
                        0.0
                    };
                    mgr.with_transfer(&id, |t| {
                        t.progress = p;
                        t.bytes_done = done;
                        t.updated_at = now_secs();
                    })
                    .await;
                    if last_emit.elapsed() >= Duration::from_millis(500) {
                        mgr.emit_update(&id, "transfer_updated").await;
                        last_emit = Instant::now();
                    }
                }
                tokio::select! {
                    sent = chunk_tx.send(Ok(bytes::Bytes::copy_from_slice(&buf[..n]))) => {
                        if sent.is_err() {
                            break;
                        }
                    }
                    _ = cancel_rx.changed() => {
                        let _ = chunk_tx
                            .send(Err(std::io::Error::new(
                                std::io::ErrorKind::Interrupted,
                                "cancelled",
                            )))
                            .await;
                        break;
                    }
                }
            }
        });
    }

    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(chunk_rx));
    let mut req = client
        .post(&url)
        .header(UPLOAD_TOKEN_HEADER, &upload_token)
        .header("Content-Type", "application/octet-stream")
        .timeout(upload_timeout(total));
    if !shared.is_empty() {
        req = req.header(TOKEN_HEADER, &shared);
    }
    let resp = req
        .body(body)
        .send()
        .await
        .context("upload request failed")?;
    let status = resp.status();
    let text = read_body(resp).await;
    if !status.is_success() {
        bail!("upload failed ({}): {}", status, text.trim());
    }

    mgr.with_transfer(id, |t| {
        if t.status == Status::Transferring {
            t.status = Status::Completed;
            t.progress = 1.0;
            t.saved_path = None; // receiver keeps the file
            t.updated_at = now_secs();
        }
    })
    .await;
    mgr.emit_update(id, "transfer_completed").await;
    info!(
        "transfer {id} complete: {} -> {}:{}",
        offer.filename, peer.addr, peer.port
    );
    Ok(())
}

/// Poll `GET /api/v1/offer/{id}` until accepted (or rejected/timeout/cancel).
async fn wait_for_acceptance(
    client: &reqwest::Client,
    poll_url: &str,
    shared: &str,
    cancel_rx: &mut watch::Receiver<bool>,
) -> Result<(String, String)> {
    let deadline = tokio::time::Instant::now() + crate::transfer::OFFER_TIMEOUT;
    loop {
        if *cancel_rx.borrow() {
            bail!("cancelled by user");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for acceptance (120s)");
        }
        match get_json(client, poll_url, shared).await {
            Ok(v) => {
                let status = v["status"].as_str().unwrap_or("");
                match status {
                    "accepted" => {
                        let url = v["upload_url"].as_str().unwrap_or("").to_string();
                        let token = v["upload_token"].as_str().unwrap_or("").to_string();
                        if url.is_empty() || token.is_empty() {
                            bail!("receiver accepted but returned no upload_url/token");
                        }
                        return Ok((url, token));
                    }
                    "rejected" => bail!("receiver rejected the transfer"),
                    "cancelled" => bail!("receiver cancelled the transfer"),
                    "failed" => bail!("offer expired on the receiver"),
                    "completed" => bail!("receiver already completed this transfer"),
                    "pending" | "waiting" => {
                        tokio::select! {
                            _ = tokio::time::sleep(POLL_INTERVAL) => {}
                            _ = cancel_rx.changed() => {}
                        }
                    }
                    other => bail!("unexpected offer status '{other}'"),
                }
            }
            Err(e) => {
                // Transient network error: keep polling until deadline.
                warn!("poll {poll_url}: {e:#}");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(1500)) => {}
                    _ = cancel_rx.changed() => {}
                }
            }
        }
    }
}

/// POST JSON, adding the shared token header when set.
async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &impl serde::Serialize,
    shared: &str,
) -> Result<reqwest::Response> {
    let mut req = client.post(url).json(body).timeout(OFFER_REQUEST_TIMEOUT);
    if !shared.is_empty() {
        req = req.header(TOKEN_HEADER, shared);
    }
    let resp = req.send().await.context("request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let text = read_body(resp).await;
        bail!("{url}: {} {}", status, text.trim());
    }
    Ok(resp)
}

/// GET JSON, adding the shared token header when set.
async fn get_json(client: &reqwest::Client, url: &str, shared: &str) -> Result<serde_json::Value> {
    let mut req = client.get(url).timeout(POLL_REQUEST_TIMEOUT);
    if !shared.is_empty() {
        req = req.header(TOKEN_HEADER, shared);
    }
    let resp = req.send().await.context("request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let text = read_body(resp).await;
        bail!("{url}: {} {}", status, text.trim());
    }
    let text = read_body(resp).await;
    serde_json::from_str(&text).context("response JSON")
}

/// Read a peer response body, capped so a hostile peer cannot exhaust memory.
async fn read_body(resp: reqwest::Response) -> String {
    if resp
        .content_length()
        .map(|l| l > MAX_PEER_BODY)
        .unwrap_or(false)
    {
        return "<response body too large>".to_string();
    }
    match resp.text().await {
        Ok(t) if t.len() as u64 <= MAX_PEER_BODY => t,
        Ok(_) => "<response body too large>".to_string(),
        Err(_) => String::new(),
    }
}
