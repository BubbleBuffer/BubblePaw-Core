# Remote transport

Status: library, relay and pluggable trust built 2026-10-29. Runner wiring
(`bubblepaw remote serve | pair | devices`), the SDK remote client and profile
enforcement over the tunnel built 2026-11-02. Also 2026-11-02: the host's own
WebSocket endpoint with direct connect, Cloudflare Tunnel reach (`quick` and
`domain` modes, `remote tunnel install | login | token`), pairing and revoking
while `remote serve` runs, and the relay binary in releases. Hub trust, service
files and Desktop/phone UI are deferred. Device kind, last seen, device key,
saved hosts, presence, serve status and Cloudflare status SDK calls: 2026-11-02.

`bubblepaw-remote` carries opaque host-protocol messages between paired devices.
It supplies identities, key files, bounded encrypted streams, pairing stores and
an authenticated WebSocket rendezvous relay. It does not interpret host requests
or enforce permission profiles; the runner's [remote host](#runner-remote-host)
serves each tunnel through the host-API method table with the device's profile.

## Device identities and `.bpkey`

An identity is an Ed25519 seed and its public key. The device ID is the first ten
bytes of SHA-256 over the raw public key, encoded as sixteen Crockford base32
characters in four groups, such as `K7MQ-2X9F-D4RT-8WHN`. Parsing accepts ASCII
letter case, hyphens and Crockford aliases O/0 and I/L/1. The fingerprint is the
full SHA-256 of the public key, written as lowercase hexadecimal.

The BubbleBuffer device-key format, version 1, is covered by public test vectors
in [`tests/fixtures`](../crates/bubblepaw-remote/tests/fixtures/README.md).
It permits protected and plain files; callers choose protection explicitly using
`Some(passphrase)`.

Version 1 is JSON with `format: "bubblebuffer-device-key"`, `version: 1`,
`type: "ed25519"`, `id`, `label`, `created`, `public`, `kdf`, `aead` and `private`.
Public key, salt, nonce and private bytes use canonical unpadded base64url.

- Protected: Argon2id v1.3, 65,536 KiB, three passes, one lane, 16-byte salt,
  32-byte output; AES-256-GCM with a 12-byte nonce. `private` contains the encrypted
  32-byte seed followed by its 16-byte authentication tag.
- Authenticated metadata is canonical JSON of the header, excluding `private`
  and `aead.nonce`, retaining `aead.alg` and the KDF salt. Imported ID spelling is
  preserved because it is authenticated. Unknown/duplicate fields and malformed
  encodings are rejected. Public-key/seed/ID consistency is checked.
- Plain: `kdf` and `aead` are both null; `private` contains the seed. Treat this
  JSON as a secret. Debug implementations omit private material.

X25519 static keys use the [libsodium conversion](https://doc.libsodium.org/advanced/ed25519-curve25519):
SHA-512 of the Ed25519 seed, then clamp the first 32 bytes; convert the public
Edwards point to Montgomery form. Invalid/noncanonical, small-order and
non-prime-subgroup public keys are rejected. Secrets and Argon2 work buffers are
zeroized on drop. Applications own key-file paths and passphrase acquisition.

## Tunnel and framing

Ordinary sessions use `Noise_XX_25519_ChaChaPoly_BLAKE2s`, with the prologue
`bubblepaw-remote/noise/v1`. After the handshake both peers check the received
static X25519 key against an encrypted Ed25519 identity exchange. The conversion
must match before the tunnel calls the caller's `PeerAuthorizer` with the derived
device ID and Ed25519 public key. Peers then exchange encrypted admission
decisions. Application data is unavailable until both accept. The returned
tunnel exposes the peer identity and locally granted profile in `authorization()`.

## Trust

Trust and transport are separate boundaries. `PeerAuthorizer` decides which peer
is authorized and returns its `read`, `edit` or `full` profile. Its public-key
argument is the Ed25519 identity already bound to the Noise static key, not an
unverified ID supplied by the relay. Authorization runs on each new handshake.

Standalone BubblePaw uses `PairStore<PairedClient>` as the host authorizer. The
pairing flow explicitly approves and saves clients; their saved profiles govern
the resulting grants. `PairStore<PairedHost>` admits saved hosts with a full grant.
When connecting to a particular host, constrain the client's authorizer to that
host. Public-key arrays/slices remain a convenience authorizer with full grants.

An embedder can instead supply a `DeviceListAuthorizer` populated with validated
`(device ID, Ed25519 public key, profile)` records. `replace()` validates the whole
new list before atomically swapping it; clones share the same live list. A failed
replacement preserves the old snapshot. A removed device is rejected on its next
handshake. Existing tunnels retain their original grant; the embedder must close
them for immediate revocation. Per-device pairing is not required for list-based
trust.

Scratch can sign devices into its hub using OAuth, refresh the list from the
account's devices, and supply the meeting point. The hub is trusted to say who
belongs to the account and what profile each device gets. The same crate supplies
device identity and end-to-end crypto; Scratch does not need another crypto
implementation. The relay network still sees only ciphertext and metadata.
Host dispatchers enforce the returned profiles for individual operations.

## Carriers

`FrameIo` makes the tunnel independent of its carrier:

- `ByteFrames<T>` accepts any Tokio `AsyncRead + AsyncWrite + Unpin + Send`.
  Each record has a four-byte big-endian length and that many bytes. The length
  is checked before allocating.
- `WebSocketFrames<T>` sends one record per binary message. Construct sockets
  using `frame::websocket_config()` so individual frames and fragmented message
  reassembly are bounded before allocation. Unbounded socket configurations and
  text messages are rejected; Ping/Pong controls are handled by the carrier.
- `RelaySession` implements the same interface and strips the relay envelope.

The [Noise wire maximum](https://noiseprotocol.org/noise.html#message-format) is
65,535 bytes including the 16-byte AEAD tag, below the specified 64 KiB plaintext
cap. An encrypted continuation byte leaves 65,518 payload bytes per chunk.
Continuation 1 means another full chunk follows; 0 ends a message. The aggregate
message limit is 16 MiB, including for untrusted senders. Empty messages work.
Malformed, altered, replayed and out-of-order records fail authentication or
framing. A failed or cancelled tunnel operation invalidates that tunnel; reconnect
instead of guessing whether a nonce or external effect was consumed. Delivery
does not establish host-operation completion and must not trigger effect replay.

## Pairing and stores

The host creates `PairingCode::generate()`: ten independently random Crockford
characters (50 bits), valid for five minutes on a monotonic clock. The host owns
the code and serializes attempts through a mutable borrow. Wrong-code attempts
do not consume it; a successful PSK proof consumes it before user confirmation,
including a declined or interrupted approval.

Pairing uses `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`. The PSK is Argon2id v1.3
with 65,536 KiB, three passes, one lane and a 32-byte result. Its password is the
normalized ten-character code and its salt is the UTF-8 canonical, grouped host
device ID. Expensive derivation runs on Tokio's blocking pool. Before Noise,
the host sends a ready/version/host-ID record or an expired/reused status. After
message three, it sends a status so the initiator can distinguish a wrong PSK.
These cleartext statuses can be spoofed to deny service; they do not grant trust.

Both sides then exchange encrypted Ed25519 identities and bind them to the
handshake's static keys. The client verifies that the public key matches the
expected host ID. The comparison is the first four big-endian bytes of
SHA-256(`"bubblepaw-pair-comparison-v1" || handshake_hash`), modulo 1,000,000,
formatted as six digits. `PendingHost::comparison()` and
`PendingClient::comparison()` expose the same number.

The caller displays both numbers. Only after the host user confirms should it
call `PendingHost::approve(name, profile, store)`; `decline()` sends an encrypted
rejection. Approval checks the deadline again, saves the client and sends an
encrypted decision. `PendingClient::finish(store)` saves the host only after
that decision. Wrong-code, preflight-expired, reused and declined outcomes have
distinct errors. Expiry during an interrupted identity exchange can appear as
a transport error to the other side; no approval or client store entry is created.
A crash/disconnect between the two local commits can leave one side paired;
inspect/revoke that entry and issue a fresh code. No automatic approval retry.

`PairStore<PairedClient>` holds the client public key, host-chosen name and
`read`, `edit` or `full` profile. `PairStore<PairedHost>` holds the host ID, public
key, name and caller-supplied reach address. Each caller supplies its JSON path.
The versioned document has `version: 1` and a `devices` object keyed by canonical
ID. Keys, names and IDs are validated on load. Missing means empty; corrupt
means error. Files are capped at 4 MiB. Writes use a random same-directory
temporary file, flush/sync and rename; Unix files are mode 0600 and the parent
directory is synced. A post-rename directory-sync failure returns
`StoreDurability`: the new snapshot is already committed and remains reflected
in memory, while crash durability is uncertain. Serialize writers per path.
`revoke(id)` deletes an entry
and atomically saves. Revocation affects future admissions; the host must also
close any already active sessions it wants to revoke immediately.

## Relay

Run `bubblepaw-relay [IP:PORT]` (default `127.0.0.1:8787`). This plain WebSocket
listener is intended behind Caddy's TLS endpoint. Host/client APIs support
`wss://` with native root certificates. TLS termination does not decrypt Noise.
Configuration and limits are exposed by `RelayConfig` and `Limits`. A `Relay`
owns its connection tasks; `shutdown().await` closes and joins them. Ctrl-C
shuts down the binary. The library owns no operating-system background process.

Every connection receives a fresh random 32-byte challenge. The device signs
`"bubblepaw-relay-v1\0" || challenge || role_byte || target_ID_UTF8` with
Ed25519. The domain separator ends in one NUL byte. The signature binds the
role and target, is checked strictly, and cannot be replayed against a new
challenge. A registration ID is derived from its authenticated public key.
This relay protocol is BubblePaw-specific.

`RelayAdmission` is the policy hook after mandatory device-key proof verification.
It receives a verified `RelayProof` and decides `admit_host`, `admit_client` and
`may_connect(client, host)`. The default `PermissiveAdmission`, used by the binary,
admits any valid device proof and permits any target; the target tunnel's
`PeerAuthorizer` still decides whether application traffic is allowed.
`RelayConfig.admission` supplies a policy for the standalone listener.

For a hub-owned HTTP route, share a `RelayRouter` between accepted sockets. Use
`router.websocket_config()` before upgrading, authenticate the HTTP account/session
in the hub, and call `router.serve_websocket(socket, peer_ip, admission).await`.
The admission object can capture the authenticated account and its device-owner
map, admitting only owned devices and connections within that account. Untrusted
relay messages cannot set this context. No OAuth implementation or credentials
are stored in this crate. `register_socket` and `connect_socket` also accept
caller-created client WebSockets, allowing the embedder to supply authentication
headers/cookies at upgrade without putting credentials in URLs.

The router applies common routing and connection limits to all these sockets and
rejects sockets whose frame/message configuration lacks the required bounds.
`Limits::websocket_config()` supplies the corresponding client configuration.
The hub owns the serving futures/tasks; cancelling one releases its registration,
sessions and IP slot. `router.shutdown().await` stops admission and drains active
serving calls. Pass a trusted peer IP from the transport or trusted proxy layer,
not arbitrary forwarded headers.
Admission hooks run when registering or connecting; immediate account/session
revocation also requires the hub to cancel the affected active serving tasks.

One binary WebSocket message is one relay frame. Integers are unsigned big-endian.

| Tag | Meaning | Body after tag |
| --- | --- | --- |
| 1 | Challenge | 32-byte nonce |
| 2 | Register | 32-byte public key, 64-byte signature; empty target |
| 3 | Connect | 32-byte public key, 64-byte signature, target ID UTF-8 |
| 4 | Registered | empty |
| 5 | Incoming | 8-byte session ID, 32-byte client public key |
| 6 | Accept | 8-byte session ID |
| 7 | Data | 8-byte session ID, opaque Noise record |
| 8 | Close | 8-byte session ID |
| 9 | Connected | 8-byte session ID, 32-byte host public key |
| 10 | Reject | empty |

`register(url, key, limits)` returns a `Host`. Its `accept()` returns independent
`RelaySession`s over the same registration. `connect(url, key, target, limits)`
waits for the host to accept. Each session runs its own Noise handshake and
nonce sequence. The relay's authenticated identity is a routing hint: the host
and client still verify their paired static keys end to end. Session IDs cannot
address another client's stream. Duplicate host registrations are rejected.

Defaults: 65,535 data bytes, 120-second idle timeout, 32 connections per source
IP, 16 pending clients and 128 total sessions per host, and bounded queues of 16.
WebSocket frame/message allocation also has an envelope allowance. Queues fail
closed under overload, with one exception (2026-11-02): a `connect` client's
connection carries a single session, so its inbound queue waits for the reader
(up to the idle timeout) and pushes back through TCP instead of failing a burst
such as a turn's event notifications. The relay's routing queues and the host's
per-session inbound queues still fail closed; there is no end-to-end flow
control, so a client that reads slower than the host writes for long enough is
disconnected. Behind Caddy the source IP is the proxy's IP, so this
limit becomes shared unless separate trusted proxy handling is added later.
State is transient: online registrations and the live routing/limit bookkeeping
needed to forward sessions; nothing is persisted. Logs never include payloads.

## Runner remote host

The runner serves its host API to paired devices. Devices reach the host in
one of three modes, set by `remote.mode` (restart-only, see [config](config.md)):

| Mode | How devices reach the host | Address | Account |
| --- | --- | --- | --- |
| `relay` (default) | the host dials out to `remote.relay` | `wss://relay...` + host ID | none |
| `quick` | a trycloudflare.com quick tunnel in front of the host's endpoint | `wss://<random>.trycloudflare.com`, new on every start | none |
| `domain` | a named Cloudflare Tunnel on your own domain | `wss://<remote.hostname>` | Cloudflare |

```toml
[remote]
enabled = true                    # remote serve and pair refuse to run otherwise
mode = "domain"                   # relay | quick | domain
relay = "wss://relay.example"     # mode relay
port = 0                          # host endpoint on 127.0.0.1 (quick, domain); 0 = free port
hostname = "paw.example.com"      # mode domain
# cloudflared = "/path/to/cloudflared"
```

In `quick` and `domain` modes the host serves its own WebSocket endpoint on
`127.0.0.1:<port>` only (never another interface), and a managed `cloudflared`
publishes it. Each WebSocket connection starts with the same selector frame and
Noise handshake as a relay session, so `cloudflared` and Cloudflare see only
ciphertext; TLS at Cloudflare's edge does not decrypt the tunnel. The device
dials the address from the pairing code directly. A connection idle for 120
seconds is closed (the 20-second keepalive keeps live tunnels open), and at most
128 direct connections are open at once.

Files live in `<data_dir>/remote/`: `host.bpkey` (the host's device key, created
on first use, plain format, owner-only 0600 on Unix, never printed),
`clients.json` (the `PairStore<PairedClient>` of paired devices with profile and
`paired_at`), `serve.lock`, `control/` (the running server's control endpoint)
and `cloudflare/` (tunnel state, below).

### Cloudflare Tunnel setup

`cloudflared` is found at `remote.cloudflared`, else at
`<data_dir>/remote/cloudflare/bin/cloudflared[.exe]`, else on `PATH`. Nothing
downloads it automatically. `bubblepaw remote tunnel install` downloads the
pinned release (`2026.10.1`, `CLOUDFLARED_VERSION`) from GitHub into that
`bin/` directory and checks its SHA-256 against GitHub's asset digest and the
checksum line in the release notes (both must agree when both exist). A
mismatch is refused and leaves no file or new directory behind. Builds exist
for Linux (amd64, arm64, arm) and Windows (amd64, 386); on macOS install it with
Homebrew and put it on `PATH`.

`quick` needs nothing else. `serve` runs `cloudflared --no-autoupdate tunnel
--url http://127.0.0.1:<port>`, reads the `https://<name>.trycloudflare.com`
address from its log and prints `Devices reach this host at wss://...`. Every
start, including an automatic restart after `cloudflared` exits, gets a new
address: devices paired at the old one must pair again, and the server prints
the new address.

`domain` needs one of:

- `bubblepaw remote tunnel login`, which runs `cloudflared tunnel login` (a
  browser login that leaves `~/.cloudflared/cert.pem`). Setup then runs
  `cloudflared tunnel create` and `cloudflared tunnel route dns`.
- An API token with Cloudflare Tunnel edit and DNS edit permissions, in
  `BUBBLEPAW_CLOUDFLARE_TOKEN` or the owner-only file
  `<data_dir>/remote/cloudflare/api-token`, written by
  `bubblepaw remote tunnel token` from stdin
  (`bubblepaw remote tunnel token < token.txt`). Never put it in the config
  file. Setup then uses the Cloudflare API: it finds the zone of `hostname`,
  creates a locally managed tunnel `bubblepaw-<hostname with dashes>` with a
  fresh 32-byte secret, and adds a proxied CNAME `hostname ->
  <tunnel id>.cfargotunnel.com` (an existing CNAME to another tunnel is
  updated; any other record for that name stops setup and is left alone).

Setup runs once, on the first `serve`. The credentials file (`tunnel.json`) and
`state.json` stay in `<data_dir>/remote/cloudflare/`, owner-only. Later starts
run `cloudflared --no-autoupdate tunnel run --credentials-file ... --url
http://127.0.0.1:<port> <tunnel id>`; the host is online once `cloudflared`
logs a registered connection. To move to another hostname, delete that
directory and the old tunnel in Cloudflare. The address `wss://<hostname>` never
changes, so devices stay paired across restarts.

`cloudflared` runs as a child of `serve` with stdin held open, its output logged
at debug level (`bubblepaw::cloudflared`), and is killed when the server stops.
When it exits the server goes offline, says why, and restarts it with backoff
(1 s doubling to 60 s, reset after a minute of running).

### Autostart

Only `relay` and `domain` hosts may start sharing without a person starting
them. A service or login item runs `bubblepaw remote serve --autostart`; in
`quick` mode that prints a note and exits successfully, so the host starts
unshared. Run `bubblepaw remote serve` by hand to share it through a quick
tunnel. Core never switches to `quick` on its own. Embedders use
`RemoteMode::autostarts()` for the same rule.

### Commands

- `bubblepaw remote serve [--autostart]` runs in the foreground until Ctrl-C. In
  relay mode it registers the host key with the relay and reconnects with
  backoff (1 s doubling to 60 s); in the tunnel modes it serves the endpoint and
  runs `cloudflared`. It prints the address and status changes on stderr and
  serves each admitted tunnel as a host-API session with the device's profile,
  host-wide reads (`ReadScope::Host`) and `event` / `events.gap` notifications,
  exactly as stdio. When another process already owns the data directory (a
  running `bubblepaw stdio`, web host or embedded SDK host), it serves through
  that host's control endpoint with `attach.session {profile}` (see
  [host attach](host-attach.md)) and exits when that host stops. Otherwise it
  opens the store itself.
- `bubblepaw remote pair [--profile read|edit|full] [--egress]` (default `read`;
  `--egress` also lets the device send model traffic through this host, see
  [model traffic](#model-traffic-through-a-paired-device)) prints a
  terminal QR code and the ten-character code, valid five minutes. The QR holds
  the `PairingPayload` JSON: `{"v":1,"relay":...,"host":...,"code":...}` for a
  relay host, `{"v":1,"direct":"wss://...","host":...,"code":...}` for a tunnel
  host (`PairingPayload::parse` reads it on the device and requires exactly one
  of the two). When a device proves the code, both sides show the same six-digit
  check number and the host asks `[y/N]`; only `y` stores the device with the
  profile. A wrong code keeps the offer open and stores nothing. With `--json`
  it prints JSON lines instead (`offer` with `payload`, `code`, `expires_in`;
  `request` with `device_name`, `device_id`, `comparison`, answered by a `y`
  line on stdin; `paired`).
- `bubblepaw remote devices` lists ID, profile, egress grant, paired date and label
  (`--json` prints `PairedDevice` records). `bubblepaw remote devices revoke <id>`
  removes the device and closes its live tunnels at once; the device's next
  handshake is refused.
- `bubblepaw remote tunnel install | login | token`: see above.

### Pairing and revoking while serving

`remote serve` opens a control endpoint in `<data_dir>/remote/control/`, a Unix
socket or named pipe for the current user only, like the
[host's own](host-attach.md) (`host.json` names it). `remote pair` and `remote
devices revoke` use it when it answers: the running server makes the offer with
its current address, so the change takes effect without a restart. The endpoint
speaks JSON lines: `{"op":"pair","profile":"full"}`, then the events above and an
`{"op":"approve"}` or `{"op":"decline"}` answer; `{"op":"revoke","id":"..."}`
answers `{"event":"revoked","removed":true}`. Closing the connection withdraws
the offer. Pairing codes cross only this user-only endpoint and are never logged.

Without a running server, `remote pair` starts one for the pairing itself
(relay and domain modes; a quick tunnel's address would die with it, so quick
mode asks you to start `remote serve` first), and `remote devices revoke` edits
the store. A running server also notices store changes by other writers within
a second. `serve.lock` keeps a second server from starting for the same data
directory.

### Flow control

There is no end-to-end flow control. The relay and the host queue a bounded
number of messages per session and close a session that overruns them; a
client that reads slower than the host writes (a slow phone during a burst of
event notifications) may be disconnected and must reconnect. Reconnecting
loses no host state: `events.gap` says what it missed.

### Relay binary

Releases ship `bubblepaw-relay` next to `bubblepaw` for Linux x86_64 and Windows
x86_64 (`bubblepaw-relay-<tag>-<target>.tar.gz` / `.zip`, listed in
`SHA256SUMS`). See [Relay](#relay) for running it.

### Wire use

Each relay session or direct WebSocket connection starts with one cleartext selector frame from the device:
`bubblepaw-remote/host-api/v1` for a host-API tunnel or `bubblepaw-remote/pair/v1`
for pairing. It routes only; trust comes from the Noise handshake that follows.
After admission, each Noise message carries one JSON-RPC record (the stdio
framing without the newline, bounded to 1 MiB). Both sides send an empty message
every 20 seconds as a keepalive, and the relay client pings, so idle tunnels
outlive the relay's 120-second idle timeout. A profile change or revocation
cancels the affected tunnels. A call cut off by a closed tunnel fails with
`Disconnected` and is never replayed.

### SDK

`bubblepaw_sdk::remote` re-exports the pieces; see [Rust SDK](sdk.md#remote-hosts).
`Client::serve_remote(RemoteServeOptions)` serves an embedded host and returns a
`RemoteServer` (`host_id`, `status` / `wait_online`, `address` / `watch_address`,
`local_port`, `last_error`, `pair(profile)` returning a `PairingOffer`,
`devices`, `revoke`, `open_control(dir)`, `shutdown`). `RemoteServeOptions.reach`
is a `RemoteReach`: `Relay(url)`, `Local { port, address }` (the endpoint on
127.0.0.1 for a caller that publishes it itself) or `Cloudflare(CloudflareTunnel
{ mode: TunnelMode::Quick | Domain { hostname }, port, cloudflared, dir, api })`
with `api: Option<CloudflareApi>` for token setup. `find_cloudflared` and
`install_cloudflared(bin_dir, &InstallSource::default())` locate and install the
program. On the device, `RemotePair` and `RemoteConnect` take an `address:
RemoteAddress` (`Relay(url)` or `Direct(url)`; `PairingPayload::address()` gives
it) and the host ID. `remote::pair(RemotePair)` returns a `PendingPairing` (`comparison`, `finish`) and
`Client::connect_remote(RemoteConnect)` returns a `Client` with the same `call`
and `subscribe` as a local one. Failures are `RemoteError` (`WrongCode`,
`ExpiredCode`, `ReusedCode`, `Declined`, `NotPaired`, `Refused`, `Unreachable`).

### Device list, device key, saved hosts and Cloudflare status (SDK)

Added 2026-11-02 so an app's Remote access pages need nothing from the CLI.
Everything is in `bubblepaw_sdk::remote`; none of it is a host method (see
[exclusions](host-api.md#remote-device-management-is-outside-the-method-table)).

- **Paired devices.** `PairedDevice` (host side) and the stored entries carry
  `kind` (`DeviceKind`: `desktop | phone | tablet | other`) and `last_seen`
  (unix seconds, written when a tunnel is accepted, at most once a minute per
  device). The connecting side declares the kind in `RemotePair.device_kind`; it
  travels in the encrypted pairing identity, is only a display hint and never
  affects trust. `Other` is not sent, so old hosts keep working, and old entries
  read as `other`. A saved host also stores the granted `profile`, `paired_at`
  and `last_connected`.
- **Device key.** `device_key_info(key_file) -> DeviceKeyInfo {id, fingerprint}`,
  `export_device_key(key_file, dest, passphrase)` (Argon2id and AES-GCM `.bpkey`;
  an empty passphrase or an existing `dest` is refused),
  `import_device_key(&DeviceKeyPaths, src, passphrase)` and
  `regenerate_device_key(&DeviceKeyPaths)`, both returning a
  `DeviceKeyChange {old_id, id, fingerprint, cleared_devices, cleared_hosts}`.
  A new ID breaks every pairing in both directions (devices paired with this
  host look for the old host ID; saved hosts no longer know this device), so both
  stores are cleared and the counts returned. Importing a key with the same ID
  changes nothing. A wrong passphrase or a damaged file changes nothing. Both
  refuse while the serve lock is held. Host and client roles share one key file
  (`RemotePaths::key_file`). No key material or passphrase is logged or returned.
- **Saved hosts.** `list_hosts(hosts_file) -> Vec<SavedHost {id, label, address,
  profile, paired_at, last_connected}>`, `forget_host(hosts_file, id) -> bool`
  (this device only; the host keeps the device until it revokes it) and
  `probe_host(&SavedHost, key_file, Limits, timeout) -> HostPresence`
  (`Online | Offline`). A relay host is asked with a presence query; an
  unreachable relay is an error. A direct or Cloudflare address is probed with a
  WebSocket connection attempt dropped at once; a failure reads as offline.
- **Presence query.** Relay auth role byte 11 (`PRESENCE`), signed like CONNECT
  and naming the target ID. The relay answers one frame `[12, 0|1]`
  (`Wire::Presence`) and closes. It passes `admit_client` and `may_connect`, so it
  reveals only online or offline for an ID the device may connect to, and opens no
  session.
- **Serve lock.** `serve_remote` and `bubblepaw remote serve | pair` take the same
  lock (`RemotePaths::lock`, held by the `RemoteServer` until it is dropped or
  shut down); a second server is refused. `serve_status(&RemotePaths) ->
  ServeStatus {shared, pid, process: Cli | Embedded, host_id}` reports the holder
  from `serve.json` beside the lock (on Windows the lock itself blocks reads).
- **Cloudflare.** `cloudflare_login_state(paths, cert_override) -> CloudflareLogin
  {cert, token}` (`token`: `Env | File`), `store_cloudflare_token` and
  `forget_cloudflare_token`, `tunnel_login(cloudflared, cert_override) ->
  TunnelLogin` (`url()`, `wait(timeout)`, `cancel()`; Core opens no browser),
  `cloudflare_account(paths, cert_override, base_url) -> CloudflareAccount {name,
  zones}` (with an API token `GET /zones`; with only `cert.pem` the token in its
  `ARGO TUNNEL TOKEN` block is used in memory for a read-only `GET /zones/{id}`;
  the account name comes from the zone data when the API gives it),
  `cloudflared_info(program, paths) -> CloudflaredInfo {path, version, pinned,
  update_available, managed}` and `update_cloudflared(paths, &InstallSource)`,
  the same checksum-verified install as `remote tunnel install`. The cert-only
  path (reading the token from cert.pem) is unverified against real Cloudflare.
- `RemoteMode` and `RemoteSettings` are re-exported from the runner config. Core
  has no default relay URL.

## Model traffic through a paired device

Built 2026-11-03 (core v0.11.0). A host whose network blocks its model provider
can send only its model traffic through another paired PC. The agent loop, tools,
files and API key stay on the host; this is byte forwarding, not remote
inference.

```toml
[providers.openai]
kind = "responses"
base_url = "https://api.openai.com/v1"
key_env = "OPENAI_API_KEY"
via = "<device id of a saved host>"   # unset: connect directly
```

How it works:

- The model HTTP client is pointed at a local HTTP CONNECT proxy on
  `127.0.0.1`, a random port, started on the first request and stopped with the
  provider. Nothing provider-specific changes: the adapter opens its normal TLS
  connection to the provider, but through the proxy.
- Each CONNECT becomes one egress session to the forwarding device: the cleartext
  selector `bubblepaw-remote/egress/v1`, then the usual Noise tunnel, then a
  JSON hello `{v:1, target, hosts}` (the target host and the provider hostnames
  declared from the provider config) and a JSON reply `{ok, code, reason}`. After
  an accepted reply both sides copy bytes: tunnel messages tagged `1` carry up to
  16 KiB of data, `2` ends the stream, an empty message is a keepalive (sent
  every 20 seconds). The forwarding device opens TCP to `host:443` and copies
  bytes. TLS starts and ends on the requesting host, so the forwarding device
  sees only ciphertext, never a prompt, a reply or the key.
- The forwarding device must be sharing (`bubblepaw remote serve` or
  `serve_remote`); the requesting host must have paired with it. `via` accepts
  the device ID shown by `remote devices` on the requesting side.

Grant. Forwarding is its own pairing grant, separate from the read, edit and full
profiles. The host user gives it when pairing (`bubblepaw remote pair --egress`,
or `RemoteServer::pair_with_egress(profile, true)`); it is stored as
`egress = true` on the paired device and shown in the `egress` column of
`remote devices` (`PairedDevice::egress`). The device learns the grant in the
pairing answer and keeps it as `SavedHost::egress`. A device without the grant is
refused with `not_granted`. Existing pairings have the grant off. Revoking the
device, or a pair-store edit that removes the grant, ends its forwarding sessions
within a second. A host forwards at most 64 sessions at once (`busy` beyond that).

Allowlist. The forwarder connects only to the provider hostnames the requester
declared from its provider config, only on port 443, and the caller cannot widen
that: the forwarder has no setting that adds hosts, and the declared list is the
only input. A hostname must be public DNS: IP literals in any notation,
`localhost`, single labels, `.local`, `.internal`, `.lan` and similar names are
refused, at most 16 names are accepted, and after resolution every address must
be public (loopback, private, link-local, carrier-grade NAT such as Tailscale's
100.64.0.0/10, documentation, benchmark and reserved ranges, and IPv4 addresses
embedded in IPv4-mapped, NAT64 and 6to4 IPv6 addresses are refused). The local
proxy applies the same check before it opens a session and serves nothing but
`CONNECT <declared host>:443`. It is not an open proxy on either side.

Errors. A failure names the device and the reason, for example `model traffic
through "Office PC" (<id>) was refused: this host has not allowed this device to
send model traffic through it`, or `... failed: it is offline, not sharing or
unreachable (...)`. Refusal codes: `not_granted`, `not_allowed`, `unreachable`,
`busy`, `protocol`, `unpaired`. Refusals other than `unreachable` and `busy` are
fatal for the provider call (nothing retries them away); an offline device is
transient. There is never a silent fallback to a direct connection: if the proxy
cannot start, the client points at a closed port. `EgressError` and
`EgressErrorKind` carry the same data (`NotPaired`, `Offline`, `Refused`,
`Local`).

`via` applies to `codex`, `responses`, `chat-completions` and `anthropic-messages`
providers with an `https://` base URL on port 443; decision providers and other
kinds reject it at validation. The provider-setup editor does not edit `via`; saving a provider there keeps the `via` its file already holds. `providers.<name>.via` is live (read at the next
registration). No Desktop UI exists for it yet. Tests
(`crates/bubblepaw-runner/tests/remote_egress.rs`) use a fake HTTPS provider on
loopback, an in-process relay and a forwarding host with a test dial; no network.
Live model acceptance has not run for this change.

## Host settings APIs (v0.13.0)

Desktop can call `remote.cloudflare.sign_out`, `remote.cloudflared.info`,
`remote.cloudflared.set` and `remote.share.state` through the same host API
locally and remotely; the executable setter is explicitly local-only.
The [SDK settings methods](sdk.md#desktop-settings-apis-v0130) also work on a
host-less remote client.

`Client::cloudflare_sign_out()` removes only the resolved `cert.pem` and reports
`changed`. The token, named tunnel credentials and other tunnel state remain.
The existing `remote::forget_cloudflare_token` remains independent.
`Client::set_cloudflared(Some(path))` validates `--version` under a short timeout,
stores the selection in `remote.cloudflared` and does not move or copy the file;
`None` clears it. Info reports `chosen` and `managed`. Restart sharing for a
changed selection to run. Every remote profile is refused, including when the
remote server forwards calls through a locally attached host, and generic
`config.set` cannot bypass this boundary.
The forwarding server requires an explicit `remote: true` acknowledgment from
the local host before it forwards requests; an older host must be restarted
with the new binary so it cannot silently ignore the restriction.

`Client::share_state()` reads embedded or separately running sharing state.
Beside `lastError`, `failure` provides the transport, optional child exit code
and actual scheduled retry delay in seconds. Both Cloudflare and relay retries
use the existing exponential backoff (1 to 60 seconds); clients must display
the returned delay, not assume a constant 30 seconds. A recovered connection
clears its failure detail.

Provider key changes require `Full`; `Read` and `Edit` may read source/presence
but cannot set or remove keys. Stored keys are host-owned plaintext files with
the same permission policy as the Cloudflare token; see [configuration](config.md).

## Threat notes and verification

The relay sees metadata only: device IDs/public keys, who talks to whom and when,
connection IPs, message sizes and timing. It can deny service, disconnect or delay
sessions. It cannot read or alter authenticated application plaintext. Reusing a
device identity across products intentionally makes that identity linkable.

The six-digit comparison guards pairing against a MITM relay; compare it on the
two physical devices before approving. This requires an honest display and user
confirmation. The code is a secret and must not be logged. Pairing and key-file
passphrases have separate roles. A stolen unlocked key or compromised endpoint
can impersonate that device; revoke it and replace its identity. The runner
enforces the stored profile on every host-API call over the tunnel.

Focused scripted and loopback transport tests cover key-format vectors, key
conversion, encrypted byte/WebSocket streams, trust rejection, pairing outcomes,
stores and multiplexed relay sessions/limits. On 2026-10-29, all 67 tests passed
with `cargo test -p bubblepaw-remote -j2 -- --test-threads=1`.
This includes list replacement/revocation, synthetic account-isolated mounted
routes, blocked-socket shutdown, post-rename sync failure and stalled pairing.
OAuth login itself is embedder-owned and was not exercised against a live hub.
Source formatting was applied on Linux; full workspace formatting, strict Clippy and
existing suite results are checked by CI's `rust` job.
No live model acceptance or VPS deployment is claimed. CI formats and lints the
crate and runs `cargo test -p bubblepaw-remote`.

Runner wiring (2026-11-02) is covered by local-transport tests with an
in-process relay and the scripted demo provider: `bubblepaw-sdk` `tests/remote.rs`
(a paired device lists sessions and runs a turn with notifications; a wrong code
stores nothing; revoke closes the live tunnel and the next connect is refused; a
`read` device gets -32003 for `config.set` while a `full` device succeeds), and
the host-API parity test's remote phase, compared with stdio.

Direct and Cloudflare reach (2026-11-02), local transport with fakes only (no
Cloudflare account, network service or live model): `bubblepaw-sdk`
`tests/remote.rs` pairs and runs a demo turn over `ws://127.0.0.1`;
`bubblepaw-runner` `tests/remote_tunnel.rs` runs a fake `cloudflared`
(`bubblepaw-fake-cloudflared`) for quick-tunnel address parsing and restart, and
a local fake Cloudflare API for domain setup (created once, reused, foreign DNS
records left alone, the `cloudflared` login path); `tests/remote_install.rs`
installs from a fake release and refuses a bad checksum; `tests/remote_cli.rs`
pairs from a second `bubblepaw remote pair --json` process while `bubblepaw
remote serve` runs, connects and runs a turn without a restart, and revokes from
a third process.
