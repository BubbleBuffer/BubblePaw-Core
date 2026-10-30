# Remote transport

Status: implemented library, relay and pluggable trust, 2026-10-29; 67 focused
Linux tests pass. Host
dispatcher, runner commands, deployment and application integration are deferred.

`bubblepaw-remote` carries opaque host-protocol messages between paired devices.
It supplies identities, key files, bounded encrypted streams, pairing stores and
an authenticated WebSocket rendezvous relay. It does not interpret host requests
or enforce permission profiles; the host dispatcher must authorize each request.

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
closed under overload. Behind Caddy the source IP is the proxy's IP, so this
limit becomes shared unless separate trusted proxy handling is added later.
State is transient: online registrations and the live routing/limit bookkeeping
needed to forward sessions; nothing is persisted. Logs never include payloads.

## Threat notes and verification

The relay sees metadata only: device IDs/public keys, who talks to whom and when,
connection IPs, message sizes and timing. It can deny service, disconnect or delay
sessions. It cannot read or alter authenticated application plaintext. Reusing a
device identity across products intentionally makes that identity linkable.

The six-digit comparison guards pairing against a MITM relay; compare it on the
two physical devices before approving. This requires an honest display and user
confirmation. The code is a secret and must not be logged. Pairing and key-file
passphrases have separate roles. A stolen unlocked key or compromised endpoint
can impersonate that device; revoke it and replace its identity. Permission
profiles are metadata until the future dispatcher enforces them.

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
