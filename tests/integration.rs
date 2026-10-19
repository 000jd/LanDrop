//! End-to-end tests: two in-process LanDrop instances transfer real files.
//!
//! Exercises the full wire protocol (offer -> accept -> upload -> verify) plus
//! the guards: upload tokens, size/checksum enforcement, cancel, path
//! traversal, concurrency, cross-site rejection and graceful shutdown.

use std::sync::Arc;

use landrop::config::Config;
use landrop::peers::{Peer, PeerStore};
use landrop::protocol::{Offer, TransferView, UPLOAD_TOKEN_HEADER};
use landrop::server::{self, AppState};
use landrop::transfer::TransferManager;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

/// Build a configured instance on an ephemeral port, download dir, name.
async fn instance(
    name: &str,
    downloads: std::path::PathBuf,
) -> (AppState, u16, landrop::events::EventBus) {
    let config_dir = downloads.parent().map(|p| p.to_path_buf());
    let mut cfg = Config {
        name: name.to_string(),
        downloads: downloads.to_string_lossy().into_owned(),
        port: 0, // set after bind
        ..Config::default()
    };

    // Bind an ephemeral port first so we know it.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    cfg.port = port;

    let peers = Arc::new(PeerStore::new());
    let transfers = TransferManager::new(peers.clone());
    transfers.set_identity(&cfg.id, &cfg.name, cfg.port).await;
    *transfers.download_dir.write().await = downloads;
    let events = transfers.events.clone();
    let mut state = AppState::new(cfg, peers.clone(), transfers.clone());
    state.config_dir = config_dir;
    let state_server = state.clone();
    tokio::spawn(async move {
        let router = server::router(state_server)
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        axum::serve(listener, router).await.unwrap();
    });

    (state, port, events)
}

/// Compute the SHA-256 of a file.
fn sha256_of(path: &std::path::Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(path).unwrap());
    format!("{:x}", hasher.finalize())
}

/// Manual peer record pointing at a test instance.
fn peer_at(id: &str, port: u16) -> Peer {
    Peer {
        id: id.into(),
        name: id.into(),
        addr: "127.0.0.1".into(),
        port,
        source: "manual".into(),
    }
}

/// Post an offer and accept it; returns the transfer id.
async fn offer_accepted(
    base: &str,
    state: &AppState,
    filename: &str,
    size: u64,
    sha256: Option<String>,
) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/offer"))
        .json(&Offer {
            sender_id: "tester".into(),
            sender_name: "tester".into(),
            filename: filename.into(),
            size,
            sha256,
            note: String::new(),
            rel_path: None,
            batch_id: None,
            batch_name: None,
        })
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "offer failed: {resp:?}");
    let tid = resp.json::<serde_json::Value>().await.unwrap()["transfer_id"]
        .as_str()
        .unwrap()
        .to_string();
    state.transfers.accept(&tid, None).await.unwrap();
    tid
}

/// The one-time upload token minted by `accept`.
async fn upload_token(state: &AppState, id: &str) -> String {
    state
        .transfers
        .offer_status(id)
        .await
        .unwrap()
        .2
        .expect("accepted transfer has an upload token")
}

/// Wait until a transfer reaches a status.
async fn wait_for_status(state: &AppState, id: &str, status: &str) -> TransferView {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let t = state.transfers.get(id).await.unwrap();
        if t.status == status {
            return t;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "transfer {id} did not reach {status}: {t:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Wait until an incoming transfer appears with a status.
async fn wait_for_incoming(state: &AppState, status: &str) -> TransferView {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(t) = state
            .transfers
            .list()
            .await
            .into_iter()
            .find(|t| t.direction == "incoming" && t.status == status)
        {
            return t;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no incoming transfer reached {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// File names currently in a directory.
fn dir_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn end_to_end_accept_transfer() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let (state_a, _port_a, _ev_a) = instance("alice", dir_a.path().join("dl")).await;
    let (state_b, port_b, _ev_b) = instance("bob", dir_b.path().join("dl")).await;

    // Create the file Alice will send.
    let payload = b"hello from alice - \x00\x01\x02 binary-ish bytes".to_vec();
    let src = dir_a.path().join("payload.bin");
    std::fs::write(&src, &payload).unwrap();
    let src_sha = sha256_of(&src);

    // 1. Alice spawns an outgoing transfer to Bob.
    let id = landrop::transfer::sender::spawn_send(
        &state_a.transfers,
        peer_at("bob-id", port_b),
        "payload.bin".into(),
        payload.len() as u64,
        Some(src_sha.clone()),
        src,
    )
    .await
    .unwrap();

    // 2. Bob's pending list should contain the offer.
    let pending = wait_for_incoming(&state_b, "waiting").await;
    assert_eq!(pending.filename, "payload.bin");
    assert_eq!(pending.size, payload.len() as u64);
    assert_eq!(pending.peer_name, "alice");

    // 3. Bob accepts.
    state_b.transfers.accept(&pending.id, None).await.unwrap();

    // 4. Wait for the transfer to complete on both sides.
    let bob_done = wait_for_status(&state_b, &pending.id, "completed").await;
    wait_for_status(&state_a, &id, "completed").await;

    // 5. Verify the saved file: content, name, checksum.
    let saved = bob_done
        .saved_path
        .expect("completed transfer has saved_path");
    assert_eq!(
        std::fs::read(&saved).unwrap(),
        payload,
        "received file bytes must match"
    );
    assert_eq!(sha256_of(std::path::Path::new(&saved)), src_sha);
    assert!(saved.ends_with("payload.bin"));

    // 7. The counters moved: this node received exactly the payload.
    let metrics = state_b.transfers.metrics.snapshot();
    assert_eq!(metrics["transfers_started"], 1);
    assert_eq!(metrics["transfers_finished"], 1);
    assert_eq!(metrics["transfers_failed"], 0);
    assert_eq!(metrics["bytes_received"], payload.len() as u64);
    assert_eq!(metrics["bytes_sent"], 0);
}

#[tokio::test]
async fn multi_file_batch_sends_different_types_and_sizes() {
    // One batch, three very different files: a small text file, a chunk of
    // "image-like" binary data and a megabyte "video/archive-like" blob. The UI
    // issues one POST /api/send per file, so this is the same path.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (state_a, _port_a, _) = instance("alice", dir_a.path().join("dl")).await;
    let (state_b, port_b, _) = instance("bob", dir_b.path().join("dl")).await;
    // Batch receiving is the common case: accept the whole batch at once.
    *state_b.transfers.auto_accept.write().await = true;

    let files: Vec<(&str, Vec<u8>)> = vec![
        ("notes.txt", b"plain text, small\n".to_vec()),
        (
            "photo.png",
            (0..180_000u32).map(|i| (i % 251) as u8).collect(),
        ),
        (
            "archive.zip",
            (0..1_000_000u32).map(|i| (i % 97) as u8).collect(),
        ),
    ];

    let mut ids = Vec::new();
    for (name, bytes) in &files {
        let src = dir_a.path().join(name);
        std::fs::write(&src, bytes).unwrap();
        let sha = sha256_of(&src);
        let id = landrop::transfer::sender::spawn_send(
            &state_a.transfers,
            peer_at("bob-id", port_b),
            name.to_string(),
            bytes.len() as u64,
            Some(sha),
            src,
        )
        .await
        .unwrap();
        ids.push((name.to_string(), bytes.clone(), id));
    }

    for (name, bytes, id) in ids {
        let done = wait_for_status(&state_a, &id, "completed").await;
        assert_eq!(done.filename, name);
        let saved = dir_b.path().join("dl").join(&name);
        assert!(
            saved.exists(),
            "{name} must arrive in the receiver's download dir {} (found: {:?})",
            saved.display(),
            dir_names(dir_b.path())
        );
        assert_eq!(
            std::fs::read(&saved).unwrap(),
            bytes,
            "{name} content must match byte for byte"
        );
    }

    // The receiver really did handle three separate transfers.
    let metrics = state_b.transfers.metrics.snapshot();
    assert_eq!(metrics["transfers_started"], 3);
    assert_eq!(metrics["transfers_finished"], 3);
    assert_eq!(metrics["transfers_failed"], 0);
}

#[tokio::test]
async fn folder_sends_recreate_the_tree_and_refuse_escapes() {
    // Folder sends carry a relative path in the offer; the receiver recreates the
    // directories underneath its download dir and refuses anything that tries to
    // climb out.
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let payload = b"nested folder payload".to_vec();

    for (rel, expect) in [
        ("photos/2024", dir.path().join("dl/photos/2024/holiday.jpg")),
        ("../../escape", dir.path().join("dl/holiday.jpg")),
    ] {
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/api/v1/offer"))
            .json(&serde_json::json!({
                "sender_id": "tester",
                "sender_name": "tester",
                "filename": "holiday.jpg",
                "size": payload.len() as u64,
                "sha256": null,
                "note": "",
                "rel_path": rel,
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "offer rejected: {resp:?}");
        let tid = resp.json::<serde_json::Value>().await.unwrap()["transfer_id"]
            .as_str()
            .unwrap()
            .to_string();
        state.transfers.accept(&tid, None).await.unwrap();

        let token = upload_token(&state, &tid).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/upload/{tid}"))
            .header(landrop::protocol::UPLOAD_TOKEN_HEADER, token)
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "upload rejected: {resp:?}");
        wait_for_status(&state, &tid, "completed").await;

        assert!(
            expect.exists(),
            "rel_path {rel:?} should land at {}",
            expect.display()
        );
        assert_eq!(std::fs::read(&expect).unwrap(), payload);
        // A traversal attempt must never create directories outside the download
        // dir, and the escape sequence must not survive as a name either.
        for path in expect.ancestors().take(3) {
            assert!(!path.to_string_lossy().contains(".."));
        }
    }
}

#[tokio::test]
async fn send_path_walks_folders_and_uses_the_published_selection() {
    // This is exactly what the desktop window does on a native drop: it forwards
    // the paths and lets the node walk them, sending to whatever the UI has
    // published as selected.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let node_a = landrop::app::start_node(landrop::app::NodeOptions {
        port: Some(0),
        downloads: Some(dir_a.path().join("dl").to_string_lossy().into_owned()),
        config_dir: Some(dir_a.path().to_string_lossy().into_owned()),
        no_mdns: true,
        port_scan: false,
        ..landrop::app::NodeOptions::default()
    })
    .await
    .unwrap();
    let node_b = landrop::app::start_node(landrop::app::NodeOptions {
        port: Some(0),
        downloads: Some(dir_b.path().join("dl").to_string_lossy().into_owned()),
        config_dir: Some(dir_b.path().to_string_lossy().into_owned()),
        no_mdns: true,
        port_scan: false,
        ..landrop::app::NodeOptions::default()
    })
    .await
    .unwrap();
    *node_b.state.transfers.auto_accept.write().await = true;

    // A dropped folder with a subfolder, plus a loose file: two files total.
    let drop_root = dir_a.path().join("holiday");
    std::fs::create_dir_all(drop_root.join("day1")).unwrap();
    std::fs::write(drop_root.join("day1/beach.jpg"), b"jpeg-ish bytes").unwrap();
    let loose = dir_a.path().join("loose.txt");
    std::fs::write(&loose, b"loose").unwrap();

    let client = reqwest::Client::new();
    let base = node_a.local_url();
    let resp = client
        .post(format!("{base}/api/ui/selection"))
        .json(&serde_json::json!({ "targets": [format!("127.0.0.1:{}", node_b.addr.port())] }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "selection rejected: {resp:?}");

    let resp = client
        .post(format!("{base}/api/send-path"))
        .json(&serde_json::json!({
            "paths": [drop_root.to_string_lossy(), loose.to_string_lossy()],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let summary: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(summary["queued"], 2, "summary: {summary}");
    assert_eq!(summary["files"], 2, "summary: {summary}");
    assert_eq!(summary["devices"], 1, "summary: {summary}");

    // Both files arrive, and the dropped folder keeps its shape.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let done = node_b
            .state
            .transfers
            .list()
            .await
            .iter()
            .filter(|t| t.status == "completed")
            .count();
        if done == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "files did not arrive in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    assert_eq!(
        std::fs::read(dir_b.path().join("dl/holiday/day1/beach.jpg")).unwrap(),
        b"jpeg-ish bytes"
    );
    assert_eq!(
        std::fs::read(dir_b.path().join("dl/loose.txt")).unwrap(),
        b"loose"
    );

    // Without a selection and without explicit targets, the node refuses instead
    // of guessing a device.
    let resp = client
        .post(format!("{base}/api/ui/selection"))
        .json(&serde_json::json!({ "targets": [] }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = client
        .post(format!("{base}/api/send-path"))
        .json(&serde_json::json!({ "paths": [loose.to_string_lossy()] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);

    node_a.shutdown();
    node_b.shutdown();
}

#[tokio::test]
async fn a_batch_is_one_decision_for_every_file() {
    // A dropped folder arrives as several offers that share a batch id: the
    // receiver groups them, so one answer covers the whole folder.
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let mut ids = Vec::new();
    for (name, rel) in [("a.png", "pics"), ("b.png", "pics/2026"), ("notes.txt", "")] {
        let resp = client
            .post(format!("{base}/api/v1/offer"))
            .json(&serde_json::json!({
                "sender_id": "tester",
                "sender_name": "tester",
                "filename": name,
                "size": 4,
                "sha256": null,
                "note": "",
                "rel_path": rel,
                "batch_id": "batch-1",
                "batch_name": "pics",
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "offer rejected: {resp:?}");
        ids.push(
            resp.json::<serde_json::Value>().await.unwrap()["transfer_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    // One batch of three, described by count and total size.
    let waiting = state.transfers.waiting_in_batch("batch-1").await;
    assert_eq!(waiting.len(), 3, "batch members: {waiting:?}");
    let (files, bytes) = state.transfers.batch_totals(&waiting).await;
    assert_eq!((files, bytes), (3, 12));
    // A different batch is a different decision.
    assert!(state.transfers.waiting_in_batch("batch-2").await.is_empty());

    // Accepting the batch releases every member — exactly what the batch prompt
    // and the batch card's "Accept all" do.
    for id in &waiting {
        state.transfers.accept(id, None).await.unwrap();
    }
    assert!(state.transfers.waiting_in_batch("batch-1").await.is_empty());
    for id in &ids {
        assert_eq!(state.transfers.get(id).await.unwrap().status, "accepted");
    }
}

#[tokio::test]
async fn one_upload_reaches_every_selected_device() {
    // One multipart request carries both targets: the node stages and hashes the
    // file once, then queues a transfer per device.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_s = tempfile::tempdir().unwrap();
    let (state_a, port_a, _) = instance("alice", dir_a.path().join("dl")).await;
    let (state_b, port_b, _) = instance("bob", dir_b.path().join("dl")).await;
    let (_state_s, port_s, _) = instance("sender", dir_s.path().join("dl")).await;
    *state_a.transfers.auto_accept.write().await = true;
    *state_b.transfers.auto_accept.write().await = true;

    let payload = b"one upload, two devices".to_vec();
    let tmp = dir_s.path().join("payload.bin");
    std::fs::write(&tmp, &payload).unwrap();
    let part = reqwest::multipart::Part::file(&tmp)
        .await
        .unwrap()
        .file_name("payload.bin");
    let form = reqwest::multipart::Form::new()
        .text("target", format!("127.0.0.1:{port_a}"))
        .text("target", format!("127.0.0.1:{port_b}"))
        .part("file", part);
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port_s}/api/send"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "send rejected: {resp:?}");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["transfer_ids"].as_array().unwrap().len(), 2, "{body}");

    for (dir, state) in [(&dir_a, &state_a), (&dir_b, &state_b)] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let done = state
                .transfers
                .list()
                .await
                .iter()
                .any(|t| t.status == "completed");
            if done {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "file did not arrive in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        assert_eq!(
            std::fs::read(dir.path().join("dl/payload.bin")).unwrap(),
            payload
        );
    }

    // The base staged file is removed after fan-out, and each per-device link is
    // removed when its transfer ends: nothing may linger in outbox/.
    let outbox = dir_s.path().join("outbox");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let empty = !outbox.exists() || dir_names(&outbox).is_empty();
        if empty {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "outbox not cleaned up: {:?}",
            dir_names(&outbox)
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn batch_accept_releases_every_member_in_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for name in ["a.txt", "b.txt", "c.txt"] {
        let resp = client
            .post(format!("{base}/api/v1/offer"))
            .json(&serde_json::json!({
                "sender_id": "tester",
                "sender_name": "tester",
                "filename": name,
                "size": 3,
                "sha256": null,
                "note": "",
                "batch_id": "batch-9",
                "batch_name": "folder",
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
    assert_eq!(state.transfers.waiting_in_batch("batch-9").await.len(), 3);

    // One request accepts the whole folder.
    let resp = client
        .post(format!("{base}/api/batches/batch-9/accept"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "batch accept failed: {resp:?}");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["accepted"], 3, "{body}");
    assert!(state.transfers.waiting_in_batch("batch-9").await.is_empty());

    // Cancelling a batch is one request too.
    for name in ["d.txt", "e.txt"] {
        let resp = client
            .post(format!("{base}/api/v1/offer"))
            .json(&serde_json::json!({
                "sender_id": "tester",
                "sender_name": "tester",
                "filename": name,
                "size": 3,
                "sha256": null,
                "note": "",
                "batch_id": "batch-10",
                "batch_name": "other folder",
            }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }
    let resp = client
        .post(format!("{base}/api/batches/batch-10/cancel"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "batch cancel failed: {resp:?}");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["cancelled"], 2, "{body}");
    assert!(state
        .transfers
        .waiting_in_batch("batch-10")
        .await
        .is_empty());
}

#[tokio::test]
async fn metrics_endpoint_reports_counters() {
    let dir = tempfile::tempdir().unwrap();
    let (_, port, _) = instance("dave", dir.path().join("dl")).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/api/metrics"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let m: serde_json::Value = resp.json().await.unwrap();
    for key in [
        "transfers_started",
        "transfers_finished",
        "transfers_failed",
        "bytes_sent",
        "bytes_received",
    ] {
        assert!(m[key].is_u64(), "{key} must be a number in {m}");
    }
    assert_eq!(m["bytes_received"], 0, "a fresh node has moved nothing");
}

#[tokio::test]
async fn end_to_end_reject_transfer() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let (state_a, _port_a, _) = instance("alice", dir_a.path().join("dl")).await;
    let (state_b, port_b, _) = instance("bob", dir_b.path().join("dl")).await;

    let src = dir_a.path().join("rejectme.txt");
    std::fs::write(&src, b"nope").unwrap();

    let id = landrop::transfer::sender::spawn_send(
        &state_a.transfers,
        peer_at("bob-id", port_b),
        "rejectme.txt".into(),
        4,
        Some(sha256_of(&src)),
        src,
    )
    .await
    .unwrap();

    let pending = wait_for_incoming(&state_b, "waiting").await;
    state_b.transfers.reject(&pending.id).await.unwrap();

    // Both sides end up rejected/failed, and nothing is saved on Bob's side.
    wait_for_status(&state_b, &pending.id, "rejected").await;
    let t = wait_for_status(&state_a, &id, "failed").await;
    assert!(
        t.error.clone().unwrap_or_default().contains("rejected"),
        "error should mention rejection: {t:?}"
    );

    let dl = dir_b.path().join("dl");
    let saved_files = if dl.exists() {
        dir_names(&dl)
    } else {
        Vec::new()
    };
    assert!(
        saved_files.is_empty(),
        "no file should be saved on rejection"
    );
}

#[tokio::test]
async fn upload_without_accept_is_rejected_by_receiver() {
    let dir_a = tempfile::tempdir().unwrap();
    let (state_b, port_b, _) = instance("bob", dir_a.path().join("dl")).await;
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port_b}");

    let resp = client
        .post(format!("{base}/api/v1/offer"))
        .json(&Offer {
            sender_id: "x".into(),
            sender_name: "x".into(),
            filename: "guard.txt".into(),
            size: 5,
            sha256: None,
            note: String::new(),
            rel_path: None,
            batch_id: None,
            batch_name: None,
        })
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let tid = resp.json::<serde_json::Value>().await.unwrap()["transfer_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Upload before accept → rejected.
    let resp = client
        .post(format!("{base}/api/v1/upload/{tid}"))
        .header(UPLOAD_TOKEN_HEADER, "whatever")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Missing upload token → rejected.
    let resp = client
        .post(format!("{base}/api/v1/upload/{tid}"))
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Accept once → ok; accept again → conflict.
    state_b.transfers.accept(&tid, None).await.unwrap();
    assert!(state_b.transfers.accept(&tid, None).await.is_err());

    let dl = dir_a.path().join("dl");
    let saved: Vec<_> = if dl.exists() {
        std::fs::read_dir(&dl).unwrap().collect()
    } else {
        Vec::new()
    };
    assert!(saved.is_empty());
}

#[tokio::test]
async fn upload_token_is_single_use() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let tid = offer_accepted(&base, &state, "once.txt", 5, None).await;
    let token = upload_token(&state, &tid).await;
    let url = format!("{base}/api/v1/upload/{tid}");

    let first = client
        .post(&url)
        .header(UPLOAD_TOKEN_HEADER, &token)
        .body("hello")
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success());
    wait_for_status(&state, &tid, "completed").await;

    // The same token must not work a second time.
    let second = client
        .post(&url)
        .header(UPLOAD_TOKEN_HEADER, &token)
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), reqwest::StatusCode::UNAUTHORIZED);

    let dl = dir.path().join("dl");
    assert_eq!(dir_names(&dl), vec!["once.txt".to_string()]);
}

#[tokio::test]
async fn oversized_upload_is_rejected_and_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    // Declared 5 bytes, but the body writes 100.
    let tid = offer_accepted(&base, &state, "small.txt", 5, None).await;
    let token = upload_token(&state, &tid).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/upload/{tid}"))
        .header(UPLOAD_TOKEN_HEADER, token)
        .body("x".repeat(100))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

    let t = wait_for_status(&state, &tid, "failed").await;
    assert!(
        t.error.clone().unwrap_or_default().contains("size"),
        "error should mention size: {t:?}"
    );
    let dl = dir.path().join("dl");
    if dl.exists() {
        assert!(dir_names(&dl).is_empty(), "no file (or .part) may remain");
    }
}

#[tokio::test]
async fn zero_size_offer_rejects_nonempty_body() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    // Declared 0 bytes: only an empty body is acceptable.
    let tid = offer_accepted(&base, &state, "empty.txt", 0, None).await;
    let token = upload_token(&state, &tid).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/upload/{tid}"))
        .header(UPLOAD_TOKEN_HEADER, token)
        .body("not empty")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    wait_for_status(&state, &tid, "failed").await;

    let dl = dir.path().join("dl");
    if dl.exists() {
        assert!(dir_names(&dl).is_empty());
    }
}

#[tokio::test]
async fn checksum_mismatch_fails_the_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let wrong = "0".repeat(64);
    let tid = offer_accepted(&base, &state, "wrong.txt", 5, Some(wrong)).await;
    let token = upload_token(&state, &tid).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/upload/{tid}"))
        .header(UPLOAD_TOKEN_HEADER, token)
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let t = wait_for_status(&state, &tid, "failed").await;
    assert!(t.error.clone().unwrap_or_default().contains("checksum"));

    let dl = dir.path().join("dl");
    if dl.exists() {
        assert!(dir_names(&dl).is_empty());
    }
}

#[tokio::test]
async fn traversal_filename_is_sanitized() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let tid = offer_accepted(&base, &state, "../../evil.txt", 2, None).await;
    let token = upload_token(&state, &tid).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/upload/{tid}"))
        .header(UPLOAD_TOKEN_HEADER, token)
        .body("hi")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let t = wait_for_status(&state, &tid, "completed").await;
    let saved = t.saved_path.unwrap();
    assert!(saved.ends_with("evil.txt"), "sanitized name: {saved}");

    let dl = std::fs::canonicalize(dir.path().join("dl")).unwrap();
    let saved_canon = std::fs::canonicalize(&saved).unwrap();
    assert!(
        saved_canon.starts_with(&dl),
        "file must stay inside the download dir: {}",
        saved_canon.display()
    );
    assert_eq!(std::fs::read(&saved_canon).unwrap(), b"hi");
}

#[tokio::test]
async fn large_file_roundtrip() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (state_a, _port_a, _) = instance("alice", dir_a.path().join("dl")).await;
    let (state_b, port_b, _) = instance("bob", dir_b.path().join("dl")).await;

    // 5 MB of deterministic pseudo-random bytes.
    let mut payload = vec![0u8; 5 * 1024 * 1024];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i * 31 % 251) as u8;
    }
    let src = dir_a.path().join("big.bin");
    std::fs::write(&src, &payload).unwrap();
    let src_sha = sha256_of(&src);

    let id = landrop::transfer::sender::spawn_send(
        &state_a.transfers,
        peer_at("bob-id", port_b),
        "big.bin".into(),
        payload.len() as u64,
        Some(src_sha.clone()),
        src,
    )
    .await
    .unwrap();

    let pending = wait_for_incoming(&state_b, "waiting").await;
    state_b.transfers.accept(&pending.id, None).await.unwrap();

    let done = wait_for_status(&state_b, &pending.id, "completed").await;
    wait_for_status(&state_a, &id, "completed").await;

    let saved = done.saved_path.unwrap();
    assert_eq!(sha256_of(std::path::Path::new(&saved)), src_sha);
}

#[tokio::test]
async fn concurrent_same_name_transfers_do_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let a = offer_accepted(&base, &state, "same.txt", 3, None).await;
    let b = offer_accepted(&base, &state, "same.txt", 3, None).await;
    let ta = upload_token(&state, &a).await;
    let tb = upload_token(&state, &b).await;

    let (ra, rb) = tokio::join!(
        client
            .post(format!("{base}/api/v1/upload/{a}"))
            .header(UPLOAD_TOKEN_HEADER, ta)
            .body("AAA")
            .send(),
        client
            .post(format!("{base}/api/v1/upload/{b}"))
            .header(UPLOAD_TOKEN_HEADER, tb)
            .body("BBB")
            .send(),
    );
    assert!(ra.unwrap().status().is_success());
    assert!(rb.unwrap().status().is_success());

    let dl = dir.path().join("dl");
    let names = dir_names(&dl);
    assert_eq!(
        names,
        vec!["same-1.txt".to_string(), "same.txt".to_string()],
        "both files must survive with distinct names"
    );
    let contents: Vec<Vec<u8>> = names
        .iter()
        .map(|n| std::fs::read(dl.join(n)).unwrap())
        .collect();
    assert!(contents.contains(&b"AAA".to_vec()));
    assert!(contents.contains(&b"BBB".to_vec()));
}

#[tokio::test]
async fn cancel_interrupts_incoming_receive() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let tid = offer_accepted(&base, &state, "cancel-me.bin", 10 * 1024 * 1024, None).await;
    let token = upload_token(&state, &tid).await;

    // Stream a body we control: send one chunk, then keep the stream open.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    let body = reqwest::Body::wrap_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let upload_url = format!("{base}/api/v1/upload/{tid}");
    let sender = tokio::spawn(async move {
        reqwest::Client::new()
            .post(upload_url)
            .header(UPLOAD_TOKEN_HEADER, token)
            .body(body)
            .send()
            .await
    });

    tx.send(Ok(bytes::Bytes::from(vec![7u8; 1024 * 1024])))
        .await
        .unwrap();
    // Let the receiver start streaming.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while state.transfers.get(&tid).await.unwrap().status != "transferring" {
        assert!(tokio::time::Instant::now() < deadline, "never started");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Cancel mid-stream.
    assert!(state.transfers.cancel(&tid).await);
    assert_eq!(state.transfers.get(&tid).await.unwrap().status, "cancelled");

    // Closing the body lets the receiver finish; it must keep the cancelled
    // status and leave no partial file behind.
    drop(tx);
    let _ = sender.await;
    let dl = dir.path().join("dl");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if dl.exists() && dir_names(&dl).is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "partial files should be removed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(state.transfers.get(&tid).await.unwrap().status, "cancelled");
}

#[tokio::test]
async fn cancelled_send_stops_and_stays_cancelled() {
    // Stub receiver: offers are accepted but the poll always stays "pending".
    let stub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_port = stub.local_addr().unwrap().port();
    let router = axum::Router::new()
        .route(
            "/api/v1/offer",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"transfer_id": "remote-1", "status": "pending"}))
            }),
        )
        .route(
            "/api/v1/offer/{id}",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "pending"})) }),
        );
    tokio::spawn(async move {
        let _ = axum::serve(stub, router).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (state, _port, _) = instance("alice", dir.path().join("dl")).await;
    let src = dir.path().join("cancelme.txt");
    std::fs::write(&src, b"data").unwrap();

    let id = landrop::transfer::sender::spawn_send(
        &state.transfers,
        peer_at("stub", stub_port),
        "cancelme.txt".into(),
        4,
        None,
        src.clone(),
    )
    .await
    .unwrap();

    wait_for_status(&state, &id, "waiting").await;
    assert!(state.transfers.cancel(&id).await);

    // Give the driver a moment to observe the cancel signal; the terminal
    // state must stay "cancelled" (no failure, no resurrection).
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let t = state.transfers.get(&id).await.unwrap();
    assert_eq!(t.status, "cancelled", "{t:?}");
    assert_eq!(t.error, None, "cancel is not an error: {t:?}");
    assert!(!src.exists(), "staged file should be cleaned up");
}

#[tokio::test]
async fn cross_site_requests_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // Host header that is not an IP/localhost → DNS-rebinding protection.
    let resp = client
        .get(format!("{base}/api/info"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // Cross-origin browser request → rejected before it reaches the handler.
    let resp = client
        .post(format!("{base}/api/settings"))
        .header("Origin", "http://evil.example")
        .json(&serde_json::json!({ "auto_accept": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);

    // Same-origin (loopback) requests still work.
    let resp = client.get(format!("{base}/api/info")).send().await.unwrap();
    assert!(resp.status().is_success());
}

#[tokio::test]
async fn oversized_json_body_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/offer"))
        .json(&serde_json::json!({
            "sender_id": "x",
            "sender_name": "x",
            "filename": "f.txt",
            "size": 1,
            "sha256": null,
            "note": "a".repeat(200 * 1024),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn offer_with_bad_sha256_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/offer"))
        .json(&Offer {
            sender_id: "x".into(),
            sender_name: "x".into(),
            filename: "f.txt".into(),
            size: 1,
            sha256: Some("not-a-hash".into()),
            note: String::new(),
            rel_path: None,
            batch_id: None,
            batch_name: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn start_node_binds_and_serves() {
    // The reusable startup API must bind, serve /api/info, and shut down
    // gracefully — the same path the CLI and the Tauri wrapper use.
    let dir = tempfile::tempdir().unwrap();
    let node = landrop::app::start_node(landrop::app::NodeOptions {
        port: Some(0), // ephemeral
        downloads: Some(dir.path().join("dl").to_string_lossy().into_owned()),
        name: Some("node-test".into()),
        no_mdns: true,
        port_scan: false,
        config_dir: Some(dir.path().to_string_lossy().into_owned()),
        ..landrop::app::NodeOptions::default()
    })
    .await
    .unwrap();

    assert!(node.addr.port() != 0);
    assert_eq!(node.config.name, "node-test");

    let url = format!("http://127.0.0.1:{}", node.addr.port());
    let resp = reqwest::get(format!("{url}/api/info")).await.unwrap();
    assert!(resp.status().is_success());
    let info: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(info["name"], "node-test");

    // Graceful shutdown stops the server, and the socket actually closes.
    node.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), node.wait_for_shutdown())
        .await
        .expect("server task should finish after shutdown");
    let after = reqwest::get(format!("{url}/api/info")).await;
    assert!(
        after.is_err() || !after.unwrap().status().is_success(),
        "server must be closed after a graceful shutdown"
    );

    // CLI overrides are not persisted: the on-disk config keeps its defaults,
    // because only an ephemeral *config* port is written back.
    let cfg_path = dir.path().join("config.toml");
    let saved = landrop::config::Config::load_from(&cfg_path).unwrap();
    assert_eq!(
        saved.port,
        landrop::config::DEFAULT_PORT,
        "run-only --port is not persisted"
    );
    assert_ne!(saved.name, "node-test", "run-only --name is not persisted");
}

/// Post an offer and return its id (no accept yet).
async fn offer_pending(base: &str, filename: &str, size: u64) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/v1/offer"))
        .json(&Offer {
            sender_id: "tester".into(),
            sender_name: "tester".into(),
            filename: filename.into(),
            size,
            sha256: None,
            note: String::new(),
            rel_path: None,
            batch_id: None,
            batch_name: None,
        })
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "offer failed: {resp:?}");
    resp.json::<serde_json::Value>().await.unwrap()["transfer_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn accept_with_custom_save_dir() {
    let dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let tid = offer_pending(&base, "picked.txt", 5).await;
    let resp = client
        .post(format!("{base}/api/transfers/{tid}/accept"))
        .json(&serde_json::json!({ "save_dir": target_dir.path() }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "accept failed: {resp:?}");

    let token = upload_token(&state, &tid).await;
    let resp = client
        .post(format!("{base}/api/v1/upload/{tid}"))
        .header(UPLOAD_TOKEN_HEADER, token)
        .body("hello")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let t = wait_for_status(&state, &tid, "completed").await;
    let saved = std::path::PathBuf::from(t.saved_path.expect("saved path"));
    assert_eq!(
        saved.parent().unwrap(),
        target_dir.path().canonicalize().unwrap(),
        "file must land in the chosen folder"
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"hello");

    let dl = dir.path().join("dl");
    if dl.exists() {
        assert!(
            dir_names(&dl).is_empty(),
            "the default folder stays untouched"
        );
    }
}

#[tokio::test]
async fn accept_with_relative_save_dir_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let tid = offer_pending(&base, "rel.txt", 5).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/transfers/{tid}/accept"))
        .json(&serde_json::json!({ "save_dir": "relative/path" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    // Still pending, and a normal accept works afterwards.
    assert_eq!(state.transfers.get(&tid).await.unwrap().status, "waiting");
    state.transfers.accept(&tid, None).await.unwrap();
}

#[tokio::test]
async fn accept_into_a_system_directory_is_rejected() {
    // Accepting a file into /etc (or another system path) is never intended,
    // even when the account could write there.
    let dir = tempfile::tempdir().unwrap();
    let (state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");

    let tid = offer_pending(&base, "passwd", 5).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/transfers/{tid}/accept"))
        .json(&serde_json::json!({ "save_dir": "/etc" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(state.transfers.get(&tid).await.unwrap().status, "waiting");
}

#[tokio::test]
async fn manual_peer_addresses_are_validated_over_http() {
    // Regression fixture for the address rules (see docs/security-tests.md):
    // the /24 broadcast form, the global broadcast, unspecified, link-local and
    // multicast targets are refused; a real LAN address works, and loopback is
    // allowed because this request comes from this machine.
    let dir = tempfile::tempdir().unwrap();
    let (_, port, _) = instance("carol", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    for bad in [
        "192.168.1.255:8420",
        "255.255.255.255:8420",
        "0.0.0.0:8420",
        "169.254.169.254:80",
        "224.0.0.1:8420",
    ] {
        let resp = client
            .post(format!("{base}/api/peers/manual"))
            .json(&serde_json::json!({ "address": bad }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{bad} must be refused"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        let msg = body["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("refusing"),
            "{bad}: unexpected message {msg:?}"
        );
    }

    for good in ["192.168.1.25:8420", "127.0.0.1:8420"] {
        let resp = client
            .post(format!("{base}/api/peers/manual"))
            .json(&serde_json::json!({ "address": good }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "{good} must be accepted"
        );
    }
}

#[tokio::test]
async fn focus_and_quit_endpoints_drive_the_window_and_the_node() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static FOCUSED: AtomicBool = AtomicBool::new(false);

    let dir = tempfile::tempdir().unwrap();
    let node = landrop::app::start_node(landrop::app::NodeOptions {
        port: Some(0),
        downloads: Some(dir.path().join("dl").to_string_lossy().into_owned()),
        no_mdns: true,
        port_scan: false,
        config_dir: Some(dir.path().to_string_lossy().into_owned()),
        on_focus: Some(landrop::app::FocusHook(std::sync::Arc::new(|| {
            FOCUSED.store(true, Ordering::SeqCst)
        }))),
        ..landrop::app::NodeOptions::default()
    })
    .await
    .unwrap();
    let url = format!("http://127.0.0.1:{}", node.addr.port());
    let client = reqwest::Client::new();

    // A second launch raises the window it already has instead of opening one.
    let resp = client
        .post(format!("{url}/api/window/focus"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(FOCUSED.load(Ordering::SeqCst), "the focus hook must run");

    // A newer build can retire this copy through the same local-only surface.
    let resp = client.post(format!("{url}/api/quit")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    tokio::time::timeout(std::time::Duration::from_secs(5), node.wait_for_shutdown())
        .await
        .expect("POST /api/quit must stop the node");
}

#[tokio::test]
async fn settings_save_does_not_persist_run_only_overrides() {
    // A settings save later in the run must not leak the CLI overrides
    // (`--port` / `--name`): they apply to that run only.
    let dir = tempfile::tempdir().unwrap();
    let node = landrop::app::start_node(landrop::app::NodeOptions {
        port: Some(0), // ephemeral, this run only
        downloads: Some(dir.path().join("dl").to_string_lossy().into_owned()),
        name: Some("node-test".into()),
        no_mdns: true,
        port_scan: false,
        config_dir: Some(dir.path().to_string_lossy().into_owned()),
        ..landrop::app::NodeOptions::default()
    })
    .await
    .unwrap();

    let url = format!("http://127.0.0.1:{}", node.addr.port());
    let resp = reqwest::Client::new()
        .post(format!("{url}/api/settings"))
        .json(&serde_json::json!({ "auto_accept": true }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "settings save should succeed");

    let saved = landrop::config::Config::load_from(&dir.path().join("config.toml")).unwrap();
    assert!(saved.auto_accept, "the requested setting is persisted");
    assert_eq!(
        saved.port,
        landrop::config::DEFAULT_PORT,
        "run-only --port stays out of the config file"
    );
    assert_ne!(
        saved.name, "node-test",
        "run-only --name stays out of the file"
    );

    node.shutdown();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), node.wait_for_shutdown()).await;
}

#[tokio::test]
async fn folder_listing_and_mkdir() {
    let dir = tempfile::tempdir().unwrap();
    let dl = dir.path().join("dl");
    std::fs::create_dir_all(dl.join("alpha")).unwrap();
    std::fs::create_dir_all(dl.join("beta")).unwrap();
    std::fs::write(dl.join("not-a-dir.txt"), "x").unwrap();
    let (_state, port, _) = instance("bob", dl.clone()).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let listing: serde_json::Value = client
        .get(format!("{base}/api/fs/list"))
        .query(&[("path", dl.to_string_lossy())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<String> = listing["dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    assert!(
        !listing["places"].as_array().unwrap().is_empty(),
        "shortcut places are offered"
    );

    // Create a folder, then it appears on disk.
    client
        .post(format!("{base}/api/fs/mkdir"))
        .json(&serde_json::json!({ "parent": dl.to_string_lossy(), "name": "gamma" }))
        .send()
        .await
        .unwrap();
    assert!(dl.join("gamma").is_dir());

    // Bad names are refused; missing paths are a clean error.
    let resp = client
        .post(format!("{base}/api/fs/mkdir"))
        .json(&serde_json::json!({ "parent": dl.to_string_lossy(), "name": "../evil" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let resp = client
        .get(format!("{base}/api/fs/list"))
        .query(&[("path", "/definitely/not/here")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_ask_save_path_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, port, _) = instance("bob", dir.path().join("dl")).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/settings"))
        .json(&serde_json::json!({ "ask_save_path": true }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let s: serde_json::Value = client
        .get(format!("{base}/api/settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["ask_save_path"], true);

    // The setting is persisted next to the node's config, not the default dir.
    let saved = landrop::config::Config::load_from(&dir.path().join("config.toml")).unwrap();
    assert!(saved.ask_save_path);
}
