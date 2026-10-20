# Security regression fixtures

Live checks that were originally run by hand are lifted into `cargo test`, so a
future change to the address rules or the folder validation fails the suite
instead of needing a manual repro.

Run everything:

```bash
cargo test                    # unit + integration
cargo test --test integration # just the HTTP-level ones
```

## Address validation (`/api/peers/manual`)

The transcript below was produced by hand against a running node, then turned
into `tests/integration.rs::manual_peer_addresses_are_validated_over_http`.

```
POST /api/peers/manual
  192.168.1.255:8420     -> 400 {"error":"refusing broadcast-form address '192.168.1.255:8420'"}
  255.255.255.255:8420   -> 400 {"error":"refusing unsafe target address '255.255.255.255:8420'"}
  0.0.0.0:8420           -> 400 {"error":"refusing unsafe target address '0.0.0.0:8420'"}
  169.254.169.254:80     -> 400 {"error":"refusing link-local target address '169.254.169.254:80'"}
  224.0.0.1:8420         -> 400 {"error":"refusing unsafe target address '224.0.0.1:8420'"}
  127.0.0.1:8420         -> 200 {"ok":true,...}      # request came from this machine
  192.168.1.25:8420      -> 200 {"ok":true,...}
```

| Rule | Enforced by (`#[test]`) |
|---|---|
| `/24` broadcast form (`x.y.z.255`) refused | `integration::manual_peer_addresses_are_validated_over_http`, `peers::parse_peer_addr_refuses_unsafe_targets` |
| Global broadcast / unspecified / multicast / link-local refused | same two tests |
| Loopback refused from the network, allowed from this machine | `peers::parse_peer_addr_refuses_unsafe_targets` (`parse_peer_addr_with(..., true)`), `manual_peer_addresses_are_validated_over_http` |
| IPv6 loopback needs the same opt-in; link-local IPv6 always refused | `peers::parse_peer_addr_ipv6` |
| Device-id input gets a friendly error instead of a parse failure | `peers::parse_peer_addr_defaults_port` |
| The same rules apply to `/api/send` targets | `peers` unit tests + the shared `parse_peer_addr_with` call site in `api_send` |

## Save-folder validation (`accept` with `save_dir`)

```
POST /api/transfers/<id>/accept {"save_dir":"/etc"}          -> 400
POST /api/transfers/<id>/accept {"save_dir":"relative/path"} -> 400
POST /api/transfers/<id>/accept {"save_dir":"/home/me/dl"}   -> 200 (folder picked in Settings)
```

| Rule | Enforced by |
|---|---|
| System directories refused (`/etc`, `/usr`, `/boot`, `/var`, `/dev`, …) | `integration::accept_into_a_system_directory_is_rejected` |
| Relative paths refused | `integration::accept_with_relative_save_dir_is_rejected` |
| Removable media and temp folders stay usable | `handlers::tests::system_directories_are_refused_but_user_folders_are_not` |
| Only the machine hosting the node may pick a folder | `accept` handler's `is_local_peer` gate + `integration::accept_with_custom_save_dir` |

## Filename sanitising

```
"../../etc/passwd"        -> "passwd"
"photo\u{202E}gpj.exe"    -> "photogpj.exe"     # bidi override dropped
"a\u{0}b.txt"             -> "ab.txt"           # control characters dropped
"trailing...  "           -> "trailing"         # Windows-invalid endings trimmed
```

Enforced by `protocol::tests::sanitize_strips_paths_and_dots`,
`sanitize_handles_adversarial_names` and `sanitize_keeps_plain_names_and_unicode`.

## Local-only endpoints

| Endpoint | Scope | Guarded by |
|---|---|---|
| `POST /api/quit` | quits this node; used by a newer build to retire an older copy | `is_local_peer` (403 otherwise) + `integration::focus_and_quit_endpoints_drive_the_window_and_the_node` |
| `POST /api/window/focus` | raises the running window (desktop only, 501 in browser/CLI mode) | `is_local_peer` + the same integration test |
| `POST /api/peers/trust` | adds/removes a trusted device id | `is_local_peer` gate in the handler; exposed by `integration::trusted_peers_auto_accept_and_expose_pins` |
| `POST /api/transfers/clear` | forgets terminal history and removes `history.json` (never deletes saved files) | `is_local_peer` gate + `integration::clear_history_keeps_active_transfers_and_saved_files`, `integration::transfer_history_survives_a_restart_and_clear_wipes_the_file` |
| `POST/GET /api/ui/shared-inbox` | queues/drains Android share-sheet paths and text; only existing absolute files are accepted | `is_local_peer` gate + `integration::shared_inbox_queues_files_and_text_for_the_ui` |
| Share-link create/revoke | stages a browser-download file (or a multi-file/folder `.zip`) for 15 minutes; only this machine may create or revoke one | `is_local_peer` gates + `integration::qr_codes_and_browser_share_links`, `integration::share_links_bundle_multi_file_and_folder_selections_as_zip` |
| `POST /api/share-link-path` | shares absolute machine-local files/folders; a single file is staged separately so expiry never deletes the source | `is_local_peer` gate + the same zip integration test |
| `GET /api/info` | reports `is_local` so the UI can disable host-only settings for a phone/other laptop; the handlers still enforce `403` | `integration::api_info_marks_loopback_requests_as_local` + the `is_local_peer` gates above |
| `POST /api/settings`, folder picker, "open download folder" | machine-local by design | `is_local_peer` gates in the handlers |
| Cross-origin browser requests and non-IP `Host` headers | rejected globally | `integration::cross_site_requests_are_rejected` |

## Upload / transfer invariants

| Rule | Enforced by |
|---|---|
| Upload without an accepted offer is refused | `integration::upload_without_accept_is_rejected_by_receiver` |
| The upload token stays valid for a resume but is burned on completion/protocol failure | `integration::upload_token_is_single_use`, `interrupted_upload_resumes_from_the_reported_offset`, `oversized_upload_is_rejected_and_not_saved`, `checksum_mismatch_fails_the_transfer` |
| A second concurrent stream for one transfer is `409` | `TransferManager::begin_upload` (`uploading` flag); not separately integration-tested |
| `HEAD` reports the partial file length without consuming the token | `integration::interrupted_upload_resumes_from_the_reported_offset` |
| A resume offset must match the on-disk partial file; a short read keeps the partial file, a clean short EOF fails it | same test + `src/transfer/receiver.rs` error paths |
| `X-LanDrop-Offset` reports **plaintext** bytes for an encrypted transfer, and decryption resumes at that offset | `integration::encrypted_transfer_handshake_pin_and_resume` |
| A negotiated encrypted transfer requires `X-LanDrop-Encrypted: chacha20-v1` | `v1_upload` check + the same integration test |
| Declared size is enforced while streaming (`size: 0` included) | `integration::oversized_upload_is_rejected_and_not_saved`, `zero_size_offer_rejects_nonempty_body` |
| SHA-256 mismatch fails the transfer | `integration::checksum_mismatch_fails_the_transfer`, `offer_with_bad_sha256_is_rejected` |
| Same-name transfers never overwrite each other | `integration::concurrent_same_name_transfers_do_not_overwrite` |
| Cancel interrupts an in-flight receive and stays cancelled | `integration::cancel_interrupts_incoming_receive`, `cancelled_send_stops_and_stays_cancelled` |
| A text snippet still requires consent and writes no file | `integration::text_snippets_need_consent_and_write_no_file` |
| JSON bodies are capped (64 KiB) | `integration::oversized_json_body_is_rejected` |

## Payload encryption and cryptographic SAS (v0.2.0)

Every v0.2.0 file offer carries `dh_pub`, a fresh ephemeral X25519 public key.
The receiver generates its own key pair, derives a session bound to its transfer
id, and returns `dh_pub` + `receiver_id`. Upload chunks are XORed with the RFC
8439 ChaCha20 keystream at their plaintext offset; the receiver decrypts at the
same offset before hashing/writing. The raw shared secret also seeds the
`NNN-NNN` PIN, so a LAN MITM (different secret on each leg) produces mismatched
PINs on the two screens.

```
offer (sender dh_pub) -> reply (receiver dh_pub, receiver_id)
session_key   = SHA-256("landrop-session-v1:" || X25519(shared) || receiver_transfer_id)
session_nonce = SHA-256("landrop-nonce-v1:"   || X25519(shared) || receiver_transfer_id)[..12]
SAS PIN       = SHA-256("landrop-sas:" || shared || sender_id || receiver_id || key)
```

| Rule | Enforced by |
|---|---|
| Two key pairs agree on the shared secret / keystream | `crypto::tests::x25519_peers_derive_the_same_secret` |
| ChaCha20 matches RFC 8439 §2.4.2 | `crypto::tests::chacha20_matches_rfc_8439_test_vector` |
| Arbitrary chunk splits and a mid-stream resume stay keystream-aligned | `crypto::tests::keystream_seeks_to_arbitrary_offsets_and_resumes` |
| SAS matches only with the same secret and context; changes with a MITM key or different key/receiver | `crypto::tests::sas_pin_changes_with_the_secret_and_the_context` |
| Malformed / low-order / all-zero public keys are refused | `crypto::tests::derive_session_rejects_bad_keys` |
| A full encrypted transfer reports `encrypted: true` on both nodes, matching PINs, and the exact plaintext SHA-256 | `integration::encrypted_transfer_handshake_pin_and_resume` |
| A dropped encrypted upload resumes from `HEAD` and still decrypts correctly | same integration test |
| Offers without `dh_pub` keep the v0.1.0 unencrypted path | `integration` suite (all legacy offer helpers omit it) |

Offer metadata (filenames, names, notes), text snippets and the upload token are
not encrypted by this layer; it protects file payload bytes only.

## Persistent history (v0.2.0)

`history.json` lives next to `config.toml`, is written with
`config::write_atomic_0600`, and holds the 100 most recent terminal transfers
(completed / failed / cancelled / rejected) including received snippets.

| Rule | Enforced by |
|---|---|
| The file round-trips and keeps only the newest 100 entries | `transfer::tests::history_round_trips_and_keeps_the_newest_entries` |
| A restart restores completed file paths and snippets; Clear removes the file | `integration::transfer_history_survives_a_restart_and_clear_wipes_the_file` |
| Clearing keeps active transfers and saved files | `integration::clear_history_keeps_active_transfers_and_saved_files` |

## Verification PINs and trusted peers

```
legacy PIN = SHA-256("{sender_id}:{receiver_id}:{batch_id or filename:size}")
             first 4 bytes, big-endian, mod 1_000_000 -> "NNN-NNN"
```

Encrypted transfers replace the legacy derivation with the cryptographic SAS
above; both formats keep the `NNN-NNN` shape in the UI.

| Rule | Enforced by |
|---|---|
| PIN derivation is deterministic, padded and keyed by batch/file | `protocol::tests::transfer_pin_is_deterministic_and_padded` |
| Every transfer view carries a PIN, including batches | `integration::trusted_peers_auto_accept_and_expose_pins` |
| The SAS PIN matches on both sides of an encrypted transfer and marks it `encrypted` | `integration::encrypted_transfer_handshake_pin_and_resume` |
| Trusted-only auto-accept accepts a listed sender and leaves strangers waiting | `integration::trusted_peers_auto_accept_and_expose_pins` |
| `POST /api/peers/trust` toggles the flag exposed by `GET /api/peers` and persists it | same test |
| The PIN is a human verification aid, not authorization (the upload token remains required) | `upload_without_accept_is_rejected_by_receiver`, `upload_token_is_single_use` |

## Share sheet inbox

| Rule | Enforced by |
|---|---|
| Only absolute paths that exist and are regular files are queued | `integration::shared_inbox_queues_files_and_text_for_the_ui` |
| The queue is drained exactly once and returns paths + text | same test |
| Empty payloads are `400`, oversized text is `413` | same test |
