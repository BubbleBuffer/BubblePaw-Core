//! Deterministic fixture maintenance using the implementation under test.
//!
//! Run only on the authorized build host:
//! cargo test -p bubblepaw-remote --lib regenerate_device_key_contract -- --ignored
//! Then recompile ordinary tests so include_str! sees the regenerated fixture.

use super::*;
use serde_json::Value;

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/device-keys-contract.json"
);

fn string(value: &Value) -> &str {
    value.as_str().expect("fixture string")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Replace complete JSON strings, preserving field order, whitespace and unrelated vectors.
fn replace_string(text: String, previous: &str, next: &str) -> String {
    let previous = serde_json::to_string(previous).expect("encode previous string");
    let next = serde_json::to_string(next).expect("encode replacement string");
    assert!(text.contains(&previous), "fixture string to replace exists");
    text.replace(&previous, &next)
}

fn regenerated_fixture(original: &str) -> String {
    let fixture: Value = serde_json::from_str(original).expect("fixture JSON");
    let vector = &fixture["keyFiles"]["passphrase"];
    let previous_format = string(&vector["file"]["format"]);
    // Parsing checks the unchanged identity and protection parameters; authentication is
    // intentionally performed below against the recomputed AAD and known test seed.
    let mut file = vector["file"].clone();
    file["format"] = Value::String(FORMAT.to_owned());
    let parsed = KeyFile::parse(&file.to_string()).expect("fixture structure");
    let Protection::Protected { salt, nonce, .. } = &parsed.protection else {
        panic!("protected fixture required");
    };
    let seed_hex = string(&vector["seedHex"]);
    assert_eq!(seed_hex.len(), SEED_BYTES * 2);
    let seed = Zeroizing::new(std::array::from_fn(|i| {
        u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16).expect("seed hex")
    }));
    let kdf_key = derive_kdf_key(string(&vector["passphrase"]), salt).expect("derive key");
    assert_eq!(hex(&kdf_key[..]), string(&vector["kdfKeyHex"]));
    let aad = parsed.aad_for_contract().expect("protected AAD");
    let sealed = seal_seed(&kdf_key, nonce, &aad, &seed).expect("seal known seed");
    let sealed = URL_SAFE_NO_PAD.encode(sealed);
    let previous_sealed = string(&vector["file"]["private"]);
    let mut output = replace_string(
        original.to_owned(),
        string(&vector["aadUtf8"]),
        std::str::from_utf8(&aad).expect("UTF-8 AAD"),
    );
    output = replace_string(output, string(&vector["aadHex"]), &hex(&aad));
    output = replace_string(output, previous_sealed, &sealed);
    output = replace_string(output, previous_format, FORMAT);

    // The two failure vectors retain their deliberate input differences and reuse
    // the valid vector's newly sealed bytes. No handshake/relay inputs may change.
    let mut expected = fixture.clone();
    expected["keyFiles"]["plain"]["file"]["format"] = FORMAT.into();
    let protected = &mut expected["keyFiles"]["passphrase"];
    protected["file"]["format"] = FORMAT.into();
    protected["file"]["private"] = sealed.clone().into();
    protected["aadUtf8"] = std::str::from_utf8(&aad).unwrap().into();
    protected["aadHex"] = hex(&aad).into();
    for failure in expected["keyFiles"]["failures"].as_array_mut().unwrap() {
        assert_eq!(string(&failure["file"]["format"]), previous_format);
        assert_eq!(string(&failure["file"]["private"]), previous_sealed);
        failure["file"]["format"] = FORMAT.into();
        failure["file"]["private"] = sealed.clone().into();
    }
    let actual: Value = serde_json::from_str(&output).expect("regenerated fixture JSON");
    assert_eq!(actual, expected, "only format and derived bytes change");
    let valid = KeyFile::parse(&actual["keyFiles"]["passphrase"]["file"].to_string()).unwrap();
    assert_eq!(
        valid
            .unlock(Some(string(&vector["passphrase"])))
            .unwrap()
            .to_bytes(),
        *seed
    );
    for failure in actual["keyFiles"]["failures"].as_array().unwrap() {
        let parsed = KeyFile::parse(&failure["file"].to_string()).unwrap();
        assert!(matches!(
            parsed.unlock(Some(string(&failure["passphrase"]))),
            Err(DeviceKeyError::AuthenticationFailed)
        ));
    }
    output
}

#[test]
fn checked_in_device_key_contract_is_reproducible() {
    let original = include_str!("../../../tests/fixtures/device-keys-contract.json");
    assert_eq!(regenerated_fixture(original), original);
}

#[test]
#[ignore = "explicit fixture maintenance: rewrites the checked-in contract"]
fn regenerate_device_key_contract() {
    let original = std::fs::read_to_string(FIXTURE_PATH).expect("read fixture");
    let generated = regenerated_fixture(&original);
    assert_eq!(
        regenerated_fixture(&generated),
        generated,
        "deterministic regeneration"
    );
    std::fs::write(FIXTURE_PATH, generated).expect("write fixture");
}
