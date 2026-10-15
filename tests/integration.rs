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

    // 6. The staged copy on the sender side is cleaned up after the transfer.
    let outbox = state_a.config_dir.clone().unwrap().join("outbox");
    if outbox.exists() {
        assert!(
            dir_names(&outbox).is_empty(),
            "staging dir should be empty after a completed transfer"
        );
    }
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
