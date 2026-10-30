use super::*;

use std::num::NonZeroU32;

use serde_json::{Value, json};

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/device-keys-contract.json"
));
const PASSPHRASE: &str = "correct horse battery staple";
const CREATED: &str = "2026-10-13T09:30:00Z";

/// Random entropy that fails on purpose: `fail_after` successful fills (zeros), then errors.
struct FailAfter(usize);

impl RngCore for FailAfter {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.try_fill_bytes(dest).expect("test RNG failed");
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        if self.0 == 0 {
            return Err(rand_core::Error::from(
                NonZeroU32::new(0x8000_0001).unwrap(),
            ));
        }
        self.0 -= 1;
        dest.fill(0);
        Ok(())
    }
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture parses")
}

fn unhex(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn seed_of(seed_hex: &str) -> [u8; 32] {
    unhex(seed_hex).try_into().expect("32-byte seed")
}

fn str_of(value: &Value) -> &str {
    value.as_str().expect("string")
}

/// Fixture file with one top-level field replaced.
fn with_field(file: &Value, key: &str, value: Value) -> String {
    let mut file = file.clone();
    file[key] = value;
    file.to_string()
}

fn without_field(file: &Value, key: &str) -> String {
    let mut file = file.clone();
    file.as_object_mut().expect("object").remove(key);
    file.to_string()
}

fn parse_error(text: &str) -> DeviceKeyError {
    match KeyFile::parse(text) {
        Ok(_) => panic!("parse should fail"),
        Err(error) => error,
    }
}

fn plain_file() -> Value {
    fixture()["keyFiles"]["plain"]["file"].clone()
}

fn protected_file() -> Value {
    fixture()["keyFiles"]["passphrase"]["file"].clone()
}

#[test]
fn plain_fixture_parses_unlocks_and_round_trips_exactly() {
    let file = plain_file();
    let key_file = KeyFile::parse(&file.to_string()).expect("plain fixture");
    assert!(!key_file.is_protected());
    assert_eq!(key_file.id().to_string(), str_of(&file["id"]));
    assert_eq!(key_file.label(), "PC test");
    assert_eq!(key_file.created(), CREATED);
    let seed = fixture()["keyFiles"]["plain"]["seedHex"].clone();
    let signing = key_file.unlock(None).expect("plain unlock");
    assert_eq!(signing.to_bytes(), seed_of(str_of(&seed)));
    let json = key_file.to_json().expect("serialize");
    assert_eq!(serde_json::from_str::<Value>(&json).expect("json"), file);
    assert!(json.starts_with("{\n  \"format\": \"bubblebuffer-device-key\",\n  \"version\": 1,\n"));
    assert!(!json.ends_with('\n'));
    let again = KeyFile::parse(&json).expect("reparse");
    assert_eq!(again.to_json().expect("serialize again"), json);
    assert!(matches!(
        key_file.unlock(Some(PASSPHRASE)),
        Err(DeviceKeyError::PassphraseNotAccepted)
    ));
}

#[test]
fn unsupported_format_is_rejected() {
    let file = plain_file();
    assert!(matches!(
        parse_error(&with_field(&file, "format", json!("other-device-key"))),
        DeviceKeyError::UnsupportedFormat
    ));
}

#[test]
fn plain_file_checks_seed_public_and_id_consistency() {
    let file = plain_file();
    let viewer_public = fixture()["ids"][0]["public"].clone();
    let viewer_id = fixture()["ids"][0]["id"].clone();
    assert!(matches!(
        parse_error(&with_field(&file, "public", viewer_public)),
        DeviceKeyError::PublicKeyMismatch
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "id", viewer_id)),
        DeviceKeyError::IdMismatch
    ));
}

#[test]
fn protected_fixture_kdf_aad_and_sealed_seed_match_exact_bytes() {
    let vector = fixture()["keyFiles"]["passphrase"].clone();
    let key_file = KeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
    assert!(key_file.is_protected());
    let Protection::Protected {
        salt,
        nonce,
        sealed,
    } = &key_file.protection
    else {
        panic!("expected protected file");
    };
    let kdf_key = derive_kdf_key(PASSPHRASE, salt).expect("argon2");
    assert_eq!(hex(&kdf_key[..]), str_of(&vector["kdfKeyHex"]));
    let aad = aad_bytes(
        &key_file.id_text,
        &key_file.label,
        &key_file.created,
        &key_file.public,
        salt,
    );
    assert_eq!(
        String::from_utf8(aad.clone()).expect("utf8"),
        str_of(&vector["aadUtf8"])
    );
    assert_eq!(hex(&aad), str_of(&vector["aadHex"]));
    // Recompute ciphertext and authentication tag from the known seed and the
    // fixture's fixed salt/nonce; serialization alone cannot prove compatibility.
    let seed = Zeroizing::new(seed_of(str_of(&vector["seedHex"])));
    let recomputed = seal_seed(&kdf_key, nonce, &aad, &seed).expect("deterministic seal");
    assert_eq!(&recomputed, sealed);
    assert_eq!(b64(&recomputed), str_of(&vector["file"]["private"]));
}

#[test]
fn protected_fixture_unlocks_to_the_viewer_seed() {
    let vector = fixture()["keyFiles"]["passphrase"].clone();
    let key_file = KeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
    let signing = key_file.unlock(Some(PASSPHRASE)).expect("unlock");
    assert_eq!(signing.to_bytes(), seed_of(str_of(&vector["seedHex"])));
    assert!(matches!(
        key_file.unlock(None),
        Err(DeviceKeyError::PassphraseRequired)
    ));
}

#[test]
fn protected_failure_vectors_are_authentication_failures() {
    let failures = fixture()["keyFiles"]["failures"].clone();
    assert_eq!(failures[0]["name"], "wrong-passphrase");
    assert_eq!(failures[1]["name"], "tampered-aad");
    let wrong = KeyFile::parse(&failures[0]["file"].to_string()).expect("structurally valid");
    assert!(matches!(
        wrong.unlock(Some(str_of(&failures[0]["passphrase"]))),
        Err(DeviceKeyError::AuthenticationFailed)
    ));
    let tampered = KeyFile::parse(&failures[1]["file"].to_string()).expect("structurally valid");
    assert!(matches!(
        tampered.unlock(Some(str_of(&failures[1]["passphrase"]))),
        Err(DeviceKeyError::AuthenticationFailed)
    ));
}

#[test]
fn unlock_rejects_a_seed_that_does_not_match_the_public_key() {
    let vector = fixture()["keyFiles"]["passphrase"].clone();
    let file = KeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
    let Protection::Protected { salt, nonce, .. } = &file.protection else {
        panic!("expected protected file");
    };
    // A correctly encrypted, authenticated seed that belongs to the PC key, not this file's key.
    let pc_seed = Zeroizing::new(seed_of(str_of(&fixture()["ids"][1]["seedHex"])));
    let kdf_key = derive_kdf_key(PASSPHRASE, salt).expect("argon2");
    let aad = aad_bytes(
        &file.id_text,
        &file.label,
        &file.created,
        &file.public,
        salt,
    );
    let sealed = seal_seed(&kdf_key, nonce, &aad, &pc_seed).expect("seal");
    let forged = KeyFile {
        id: file.id,
        id_text: file.id_text.clone(),
        label: file.label.clone(),
        created: file.created.clone(),
        public: file.public,
        protection: Protection::Protected {
            salt: *salt,
            nonce: *nonce,
            sealed,
        },
    };
    assert!(matches!(
        forged.unlock(Some(PASSPHRASE)),
        Err(DeviceKeyError::PublicKeyMismatch)
    ));
}

#[test]
fn create_round_trips_plain_and_protected_keys() {
    let plain = KeyFile::create("Pixel 8", CREATED, None).expect("create plain");
    assert!(!plain.is_protected());
    assert_eq!(plain.label(), "Pixel 8");
    let signing = plain.unlock(None).expect("unlock plain");
    assert_eq!(signing.verifying_key().to_bytes(), *plain.public_key());
    assert_eq!(plain.id(), DeviceId::from_public_key(plain.public_key()));
    let parsed = KeyFile::parse(&plain.to_json().expect("json")).expect("parse plain");
    assert_eq!(parsed.id(), plain.id());
    assert_eq!(parsed.created(), CREATED);
    assert_eq!(
        parsed.unlock(None).expect("unlock parsed").to_bytes(),
        signing.to_bytes()
    );

    let protected = KeyFile::create("Viewer", CREATED, Some(PASSPHRASE)).expect("create protected");
    assert!(protected.is_protected());
    let json = protected.to_json().expect("json");
    assert!(json.contains("\"alg\": \"argon2id\"") && json.contains("\"alg\": \"aes-256-gcm\""));
    let parsed = KeyFile::parse(&json).expect("parse protected");
    assert_eq!(parsed.id(), protected.id());
    let unlocked = parsed.unlock(Some(PASSPHRASE)).expect("unlock protected");
    assert_eq!(unlocked.verifying_key().to_bytes(), *protected.public_key());
    assert!(matches!(
        parsed.unlock(Some("wrong passphrase")),
        Err(DeviceKeyError::AuthenticationFailed)
    ));
    assert!(matches!(
        parsed.unlock(None),
        Err(DeviceKeyError::PassphraseRequired)
    ));
}

#[test]
fn rewrap_takes_fresh_salt_and_nonce_and_keeps_identity() {
    let original = KeyFile::create("Viewer", CREATED, Some(PASSPHRASE)).expect("create");
    let protection = |file: &KeyFile| match &file.protection {
        Protection::Protected {
            salt,
            nonce,
            sealed,
        } => (*salt, *nonce, sealed.clone()),
        Protection::Plain { .. } => panic!("expected protected file"),
    };
    let (salt0, nonce0, sealed0) = protection(&original);
    let seed = original
        .unlock(Some(PASSPHRASE))
        .expect("unlock")
        .to_bytes();

    let first = original
        .rewrap(Some(PASSPHRASE), Some(PASSPHRASE))
        .expect("rewrap");
    let second = original
        .rewrap(Some(PASSPHRASE), Some(PASSPHRASE))
        .expect("rewrap");
    for rewrapped in [&first, &second] {
        assert_eq!(rewrapped.id(), original.id());
        assert_eq!(rewrapped.public_key(), original.public_key());
        assert_eq!(rewrapped.label(), "Viewer");
        assert_eq!(rewrapped.created(), CREATED);
        assert_eq!(
            rewrapped
                .unlock(Some(PASSPHRASE))
                .expect("unlock")
                .to_bytes(),
            seed
        );
    }
    let (salt1, nonce1, sealed1) = protection(&first);
    let (salt2, nonce2, sealed2) = protection(&second);
    assert_ne!(salt1, salt0);
    assert_ne!(nonce1, nonce0);
    assert_ne!(sealed1, sealed0);
    assert_ne!(salt1, salt2);
    assert_ne!(nonce1, nonce2);
    assert_ne!(sealed1, sealed2);

    let exported = original
        .rewrap(Some(PASSPHRASE), None)
        .expect("export plain");
    assert!(!exported.is_protected());
    assert_eq!(
        exported.unlock(None).expect("plain unlock").to_bytes(),
        seed
    );
    let reprotected = exported
        .rewrap(None, Some("second passphrase"))
        .expect("re-protect");
    assert!(reprotected.is_protected());
    assert_eq!(
        reprotected
            .unlock(Some("second passphrase"))
            .expect("unlock")
            .to_bytes(),
        seed
    );
    assert!(matches!(
        reprotected.unlock(Some(PASSPHRASE)),
        Err(DeviceKeyError::AuthenticationFailed)
    ));
    assert!(matches!(
        original.rewrap(Some("wrong passphrase"), None),
        Err(DeviceKeyError::AuthenticationFailed)
    ));
}

#[test]
fn randomness_failures_are_reported_not_ignored() {
    assert!(matches!(
        KeyFile::create_with_rng(&mut FailAfter(0), "Pixel", CREATED, None),
        Err(DeviceKeyError::Entropy)
    ));
    assert!(matches!(
        KeyFile::create_with_rng(&mut FailAfter(0), "Pixel", CREATED, Some(PASSPHRASE)),
        Err(DeviceKeyError::Entropy)
    ));
    // The seed succeeds, then the salt fails: the protected path must not swallow it.
    assert!(matches!(
        KeyFile::create_with_rng(&mut FailAfter(1), "Pixel", CREATED, Some(PASSPHRASE)),
        Err(DeviceKeyError::Entropy)
    ));
    let plain = KeyFile::create("Pixel", CREATED, None).expect("create");
    assert!(matches!(
        plain.rewrap_with_rng(&mut FailAfter(0), None, Some(PASSPHRASE)),
        Err(DeviceKeyError::Entropy)
    ));
}

mod validation;
