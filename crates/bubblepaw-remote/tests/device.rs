use bubblepaw_remote::device::{DeviceId, DeviceKey, DeviceKeyError, KeyFile, noise_public_key};
use curve25519_dalek::{
    constants::EIGHT_TORSION, edwards::CompressedEdwardsY, montgomery::MontgomeryPoint,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::Value;

fn bytes<const N: usize>(hex: &str) -> [u8; N] {
    assert_eq!(hex.len(), N * 2);
    std::array::from_fn(|index| {
        u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("hex")
    })
}

#[test]
fn rfc8032_identity_and_libsodium_noise_conversion_vector() {
    // RFC 8032 test 1 seed/public/signature. X25519 expected values pin the
    // libsodium conversion (SHA-512 + clamp; u=(1+y)/(1-y)).
    let key = DeviceKey::from_seed(&bytes(
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    ));
    assert_eq!(
        key.public_key(),
        bytes("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",)
    );
    assert_eq!(key.id().to_string(), "47Z3-3QX1-AJH6-2RKB");
    assert_eq!(
        key.fingerprint(),
        "21fe31dfa154a261626bf854046fd2271b7bed4b6abe45aa58877ef47f9721b9"
    );
    assert_eq!(
        key.sign(b""),
        bytes(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        )
    );
    let private = key.noise_private_key();
    assert_eq!(
        *private,
        bytes("307c83864f2833cb427a2ef1c00a013cfdff2768d980c0a3a520f006904de94f",)
    );
    let public = noise_public_key(&key.public_key()).expect("valid prime-order key");
    assert_eq!(
        public,
        bytes("d85e07ec22b0ad881537c2f44d662d1a143cf830c57aca4305d85c7a90f6b62e",)
    );
    assert_eq!(
        MontgomeryPoint::mul_base_clamped(*private).to_bytes(),
        public
    );
}

#[test]
fn noise_conversion_rejects_malformed_small_order_and_torsion_keys() {
    for torsion in EIGHT_TORSION {
        assert!(matches!(
            noise_public_key(&torsion.compress().to_bytes()),
            Err(DeviceKeyError::InvalidPublicKey)
        ));
    }
    // Noncanonical y=p+1 represents the identity if reduced by a permissive decoder.
    let mut noncanonical = [0xff; 32];
    noncanonical[0] = 0xee;
    noncanonical[31] = 0x7f;
    assert!(noise_public_key(&noncanonical).is_err());
    // y=2 has no Edwards point.
    let mut invalid = [0; 32];
    invalid[0] = 2;
    assert!(CompressedEdwardsY(invalid).decompress().is_none());
    assert!(noise_public_key(&invalid).is_err());
    let public = DeviceKey::from_seed(&[7; 32]).public_key();
    let point = CompressedEdwardsY(public)
        .decompress()
        .expect("Ed25519 key");
    for torsion in &EIGHT_TORSION[1..] {
        let mixed = point + torsion;
        assert!(!mixed.is_small_order());
        assert!(!mixed.is_torsion_free());
        assert!(noise_public_key(&mixed.compress().to_bytes()).is_err());
    }
}

#[test]
fn generated_keys_sign_and_convert_without_exposing_the_seed_in_debug() {
    let first = DeviceKey::generate().expect("OS randomness");
    let second = DeviceKey::generate().expect("OS randomness");
    assert_ne!(first.public_key(), second.public_key());
    let message = b"BubblePaw device proof";
    VerifyingKey::from_bytes(&first.public_key())
        .expect("public")
        .verify_strict(message, &Signature::from_bytes(&first.sign(message)))
        .expect("signature");
    assert_eq!(
        MontgomeryPoint::mul_base_clamped(*first.noise_private_key()).to_bytes(),
        noise_public_key(&first.public_key()).expect("conversion"),
    );
    assert_eq!(DeviceId::from_public_key(&first.public_key()), first.id());
    assert_eq!(
        format!("{first:?}"),
        format!("DeviceKey {{ id: {:?}, .. }}", first.id())
    );
}

#[test]
fn existing_identity_keyfiles_round_trip_plain_and_protected() {
    let key = DeviceKey::from_seed(&[23; 32]);
    for passphrase in [None, Some("test passphrase")] {
        let file = KeyFile::from_device(&key, "Laptop", "2026-10-29T00:00:00Z", passphrase)
            .expect("wrap identity");
        let json = file.to_json().expect("serialize");
        let parsed = KeyFile::parse(&json).expect("parse");
        let unlocked = parsed.unlock_device(passphrase).expect("unlock device");
        assert_eq!(unlocked.public_key(), key.public_key());
        assert_eq!(unlocked.sign(b"proof"), key.sign(b"proof"));
        assert_eq!(parsed.to_json().expect("stable encoding"), json);
    }
}

#[test]
fn upstream_plain_and_encrypted_contract_files_preserve_exact_bytes() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/device-keys-contract.json")).expect("fixture");
    for name in ["plain", "passphrase"] {
        let vector = &fixture["keyFiles"][name];
        let parsed = KeyFile::parse(&vector["file"].to_string()).expect("upstream key file");
        let seed = bytes::<32>(vector["seedHex"].as_str().expect("seed"));
        let device = parsed
            .unlock_device(vector["passphrase"].as_str())
            .expect("unlock");
        assert_eq!(
            device.public_key(),
            DeviceKey::from_seed(&seed).public_key()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&parsed.to_json().expect("serialize")).expect("JSON"),
            vector["file"]
        );
        if name == "passphrase" {
            assert_eq!(
                parsed.aad_for_contract().expect("protected AAD"),
                vector["aadUtf8"].as_str().expect("AAD").as_bytes()
            );
        }
    }
}
