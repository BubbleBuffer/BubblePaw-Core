# Device-key contract fixtures

`device-keys-contract.json` covers the BubbleBuffer device-key format, version 1.
Its private material is public test data.

The key-file vectors cover plain and protected files, fixed Argon2id v1.3
parameters, AES-256-GCM, authenticated metadata bytes and imported ID spelling.
The contract tests recompute the protected seed's complete ciphertext and tag
using its fixed salt and nonce, comparing the exact stored `private` bytes and
their base64url encoding. They also check wrong-passphrase and tampered-metadata
failures.

The device tests cover Ed25519 signing, a full SHA-256 public-key fingerprint,
and libsodium-compatible Ed25519-to-X25519 conversion. Public conversion rejects
noncanonical encodings, small-order points and points with a torsion component.

## Deterministic regeneration

The ignored `regenerate_device_key_contract` test uses the crate's own canonical
AAD, Argon2id and AES-256-GCM code. It preserves the existing seeds, passphrases,
salts, nonces and metadata, updating only format identifiers and their derived
AAD and authentication tags. Failure vectors retain their original invalid
passphrase or tampered metadata and reuse the regenerated protected bytes.

Run on the build host:

```sh
cargo test -p bubblepaw-remote --lib device::keyfile::contract_generator::regenerate_device_key_contract -- --ignored --exact
cargo test -p bubblepaw-remote
```

The second command recompiles the contract tests with the regenerated fixture.
Other fixture sections remain unchanged.
