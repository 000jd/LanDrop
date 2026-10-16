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
| `POST /api/settings`, folder picker, "open download folder" | machine-local by design | `is_local_peer` gates in the handlers |
| Cross-origin browser requests and non-IP `Host` headers | rejected globally | `integration::cross_site_requests_are_rejected` |

## Upload / transfer invariants

| Rule | Enforced by |
|---|---|
| Upload without an accepted offer is refused | `integration::upload_without_accept_is_rejected_by_receiver` |
| The upload token is single-use | `integration::upload_token_is_single_use` |
| Declared size is enforced while streaming (`size: 0` included) | `integration::oversized_upload_is_rejected_and_not_saved`, `zero_size_offer_rejects_nonempty_body` |
| SHA-256 mismatch fails the transfer | `integration::checksum_mismatch_fails_the_transfer`, `offer_with_bad_sha256_is_rejected` |
| Same-name transfers never overwrite each other | `integration::concurrent_same_name_transfers_do_not_overwrite` |
| Cancel interrupts an in-flight receive and stays cancelled | `integration::cancel_interrupts_incoming_receive`, `cancelled_send_stops_and_stays_cancelled` |
| JSON bodies are capped (64 KiB) | `integration::oversized_json_body_is_rejected` |
