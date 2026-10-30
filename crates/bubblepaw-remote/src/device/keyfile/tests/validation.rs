use super::*;

#[test]
fn refuses_unknown_format_version_and_type() {
    let file = plain_file();
    assert!(matches!(
        parse_error(&with_field(&file, "format", json!("other-format"))),
        DeviceKeyError::UnsupportedFormat
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "version", json!(2))),
        DeviceKeyError::UnsupportedVersion
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "version", json!("1"))),
        DeviceKeyError::InvalidField("version")
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "type", json!("rsa"))),
        DeviceKeyError::UnsupportedType
    ));
    assert!(matches!(
        parse_error(&without_field(&file, "format")),
        DeviceKeyError::MissingField("format")
    ));
}

#[test]
fn refuses_invalid_lengths_and_encodings() {
    let file = plain_file();
    let public = str_of(&file["public"]).to_owned();
    let private = str_of(&file["private"]).to_owned();
    assert!(matches!(
        parse_error(&with_field(&file, "public", json!(b64(&[0u8; 31])))),
        DeviceKeyError::InvalidLength("public")
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "public", json!(format!("{public}=")))),
        DeviceKeyError::InvalidEncoding("public")
    ));
    assert!(matches!(
        parse_error(&with_field(
            &file,
            "public",
            json!(format!("+{}", &public[1..]))
        )),
        DeviceKeyError::InvalidEncoding("public")
    ));
    // Last character of 32 bytes carries two unused bits: changing bit 0 keeps the decoded bytes.
    let alphabet = std::str::from_utf8(BASE64URL).expect("ascii");
    let last = public.chars().last().expect("non-empty");
    let index = alphabet.find(last).expect("alphabet");
    let mut non_canonical = public.clone();
    non_canonical.pop();
    non_canonical.push(alphabet.as_bytes()[index ^ 1] as char);
    assert!(matches!(
        parse_error(&with_field(&file, "public", json!(non_canonical))),
        DeviceKeyError::InvalidEncoding("public")
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "private", json!(b64(&[7u8; 31])))),
        DeviceKeyError::InvalidLength("private")
    ));
    assert!(matches!(
        parse_error(&with_field(&file, "private", json!(b64(&[7u8; 48])))),
        DeviceKeyError::InvalidLength("private")
    ));
    assert!(!private.is_empty());

    let protected = protected_file();
    assert!(matches!(
        parse_error(&with_field(&protected, "private", json!(b64(&[7u8; 32])))),
        DeviceKeyError::InvalidLength("private")
    ));
    let mut short_salt = protected.clone();
    short_salt["kdf"]["salt"] = json!(b64(&[1u8; 15]));
    assert!(matches!(
        parse_error(&short_salt.to_string()),
        DeviceKeyError::InvalidLength("kdf.salt")
    ));
    let mut short_nonce = protected.clone();
    short_nonce["aead"]["nonce"] = json!(b64(&[1u8; 11]));
    assert!(matches!(
        parse_error(&short_nonce.to_string()),
        DeviceKeyError::InvalidLength("aead.nonce")
    ));
    let mut padded_salt = protected.clone();
    padded_salt["kdf"]["salt"] = json!(format!("{}==", str_of(&protected["kdf"]["salt"])));
    assert!(matches!(
        parse_error(&padded_salt.to_string()),
        DeviceKeyError::InvalidEncoding("kdf.salt")
    ));
}

#[test]
fn imported_id_spelling_is_preserved_for_authentication() {
    let plain = plain_file();
    let alternate = str_of(&plain["id"]).to_lowercase().replace('-', "");
    let parsed = KeyFile::parse(&with_field(&plain, "id", json!(alternate))).expect("ID aliases");
    assert_eq!(parsed.id().to_string(), str_of(&plain["id"]));
    let serialized: Value = serde_json::from_str(&parsed.to_json().unwrap()).unwrap();
    assert_eq!(serialized["id"], alternate);
    let protected = protected_file();
    let tampered = KeyFile::parse(&with_field(
        &protected,
        "id",
        json!(str_of(&protected["id"]).to_lowercase()),
    ))
    .unwrap();
    assert!(matches!(
        tampered.unlock(Some(PASSPHRASE)),
        Err(DeviceKeyError::AuthenticationFailed)
    ));
    assert!(matches!(
        parse_error(&with_field(&plain, "id", json!("bad"))),
        DeviceKeyError::InvalidId
    ));
}

#[test]
fn refuses_incomplete_and_unsupported_protection() {
    let protected = protected_file();
    let mut kdf_only = protected.clone();
    kdf_only["aead"] = Value::Null;
    assert!(matches!(
        parse_error(&kdf_only.to_string()),
        DeviceKeyError::IncompleteProtection
    ));
    let mut aead_only = protected.clone();
    aead_only["kdf"] = Value::Null;
    assert!(matches!(
        parse_error(&aead_only.to_string()),
        DeviceKeyError::IncompleteProtection
    ));
    let mut missing_aead = protected.clone();
    missing_aead.as_object_mut().expect("object").remove("aead");
    assert!(matches!(
        parse_error(&missing_aead.to_string()),
        DeviceKeyError::MissingField("aead")
    ));

    let mut argon2i = protected.clone();
    argon2i["kdf"]["alg"] = json!("argon2i");
    assert!(matches!(
        parse_error(&argon2i.to_string()),
        DeviceKeyError::UnsupportedKdf
    ));
    let mut weaker = protected.clone();
    weaker["kdf"]["m"] = json!(32768);
    assert!(matches!(
        parse_error(&weaker.to_string()),
        DeviceKeyError::UnsupportedKdf
    ));
    let mut fewer_passes = protected.clone();
    fewer_passes["kdf"]["t"] = json!(2);
    assert!(matches!(
        parse_error(&fewer_passes.to_string()),
        DeviceKeyError::UnsupportedKdf
    ));
    let mut more_lanes = protected.clone();
    more_lanes["kdf"]["p"] = json!(2);
    assert!(matches!(
        parse_error(&more_lanes.to_string()),
        DeviceKeyError::UnsupportedKdf
    ));
    let mut aes128 = protected.clone();
    aes128["aead"]["alg"] = json!("aes-128-gcm");
    assert!(matches!(
        parse_error(&aes128.to_string()),
        DeviceKeyError::UnsupportedAead
    ));
    let mut no_salt = protected.clone();
    no_salt["kdf"]
        .as_object_mut()
        .expect("object")
        .remove("salt");
    assert!(matches!(
        parse_error(&no_salt.to_string()),
        DeviceKeyError::MissingField("kdf.salt")
    ));
    let mut extra_kdf = protected.clone();
    extra_kdf["kdf"]["extra"] = json!(1);
    assert!(
        matches!(parse_error(&extra_kdf.to_string()), DeviceKeyError::UnknownField(ref f) if f == "kdf.extra")
    );
    assert!(matches!(
        parse_error(&with_field(&protected, "extra", json!("x"))),
        DeviceKeyError::UnknownField(ref f) if f == "extra"
    ));
}

#[test]
fn refuses_duplicate_keys_and_non_canonical_json() {
    let text = plain_file().to_string();
    let duplicate_top = format!("{{\"label\":\"first\",{}", &text[1..]);
    assert!(matches!(
        parse_error(&duplicate_top),
        DeviceKeyError::Malformed("duplicate key")
    ));
    let protected = protected_file().to_string();
    let duplicate_nested = protected.replacen("\"m\":65536", "\"m\":65536,\"m\":65536", 1);
    assert_ne!(duplicate_nested, protected);
    assert!(matches!(
        parse_error(&duplicate_nested),
        DeviceKeyError::Malformed("duplicate key")
    ));
    assert!(matches!(
        parse_error(&format!("{text} x")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&format!("{text}{text}")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&with_field(&plain_file(), "extra", json!(true))),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&with_field(&plain_file(), "extra", json!([1]))),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&text.replace("\"version\":1", "\"version\":1.0")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&text.replace("\"version\":1", "\"version\":01")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&text.replace("\"version\":1", "\"version\":-0")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&text.replace("\"version\":1", "\"version\":1e0")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(parse_error("[1]"), DeviceKeyError::Malformed(_)));
    assert!(matches!(parse_error(""), DeviceKeyError::Malformed(_)));
    assert!(matches!(
        parse_error(&text.replace("\"PC test\"", "\"PC\\ud800test\"")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error(&text.replace("\"PC test\"", "\"PC\u{1}test\"")),
        DeviceKeyError::Malformed(_)
    ));
    assert!(matches!(
        parse_error("{\"a\":{\"b\":{\"c\":{\"d\":{\"e\":1}}}}}"),
        DeviceKeyError::Malformed(_)
    ));
    // Insignificant whitespace is fine; escapes that denote the same text are fine too.
    let spaced = format!("\n\t{} \r\n", text);
    assert!(KeyFile::parse(&spaced).is_ok());
    let escaped_label = text.replace("\"PC test\"", "\"\\u0050C test\"");
    assert_eq!(
        KeyFile::parse(&escaped_label).expect("escapes").label(),
        "PC test"
    );
    let surrogate_label = text.replace("\"PC test\"", "\"\\ud83d\\ude00\"");
    assert_eq!(
        KeyFile::parse(&surrogate_label).expect("pair").label(),
        "\u{1F600}"
    );
}

#[test]
fn json_canonical_form_sorts_utf16_and_escapes_strings() {
    let entries = vec![
        ("\u{FFFD}".to_owned(), Json::Int(5)),
        ("\u{1F600}".to_owned(), Json::Int(4)),
        ("é".to_owned(), Json::Int(3)),
        ("a".to_owned(), Json::Int(2)),
        ("B".to_owned(), Json::Int(1)),
    ];
    let expected = "{\"B\":1,\"a\":2,\"é\":3,\"\u{1F600}\":4,\"\u{FFFD}\":5}";
    assert_eq!(canonical(&Json::Object(entries.clone())), expected);
    let mut reversed = entries;
    reversed.reverse();
    assert_eq!(canonical(&Json::Object(reversed)), expected);

    let tricky = "\"\\\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{7f}é\u{1F600}/";
    let escaped = r#""\"\\\b\f\n\r\t\u0001\u001f"#.to_owned() + "\u{7f}é\u{1F600}/\"";
    assert_eq!(canonical(&jstr(tricky)), escaped);
    assert_eq!(canonical(&Json::Null), "null");
    assert_eq!(canonical(&Json::Int(-5)), "-5");
    assert_eq!(canonical(&Json::Int(0)), "0");
    assert_eq!(canonical(&Json::Object(Vec::new())), "{}");

    let parsed = parse_document(r#"{"b":"é","a":{"x":-7}}"#).expect("parse");
    assert_eq!(canonical(&parsed), "{\"a\":{\"x\":-7},\"b\":\"é\"}");
}

#[test]
fn aad_is_canonical_for_quotes_and_non_ascii_labels() {
    let vector = fixture()["keyFiles"]["passphrase"].clone();
    let file = KeyFile::parse(&vector["file"].to_string()).expect("protected fixture");
    assert_eq!(file.label(), "Viewer \"test\" café");
    let Protection::Protected { salt, .. } = &file.protection else {
        panic!("expected protected file");
    };
    let aad = aad_bytes(
        &file.id_text,
        &file.label,
        &file.created,
        &file.public,
        salt,
    );
    let text = String::from_utf8(aad).expect("utf8");
    assert!(text.contains(r#""label":"Viewer \"test\" café""#));
    assert!(text.starts_with(r#"{"aead":{"alg":"aes-256-gcm"},"created":"#));
}

#[test]
fn debug_output_and_stable_serialization_hide_nothing_they_should_not() {
    let key = KeyFile::parse(&plain_file().to_string()).expect("plain fixture");
    let seed = key.unlock(None).expect("unlock").to_bytes();
    let debug = format!("{key:?}");
    assert!(debug.contains(&key.id().to_string()));
    assert!(!debug.contains(&hex(&seed)));
    assert!(!debug.contains(&b64(&seed)));
    let protected = KeyFile::parse(&protected_file().to_string()).expect("protected fixture");
    let debug = format!("{protected:?}");
    assert!(debug.contains("protected: true"));
    assert!(!debug.contains("salt"));
    let json = protected.to_json().expect("json");
    assert_eq!(
        KeyFile::parse(&json)
            .expect("reparse")
            .to_json()
            .expect("json"),
        json
    );
}

#[test]
fn supplied_strings_round_trip_without_ui_policy() {
    let label = format!("{}\n\t\u{0}café \u{1f600}", "x".repeat(20_000));
    let plain = KeyFile::create(&label, "", None).unwrap();
    let parsed = KeyFile::parse(&plain.to_json().unwrap()).unwrap();
    assert_eq!(parsed.label(), label);
    assert_eq!(parsed.created(), "");
    for passphrase in ["", "päss\u{0}\u{1f600}"] {
        let protected = plain.rewrap(None, Some(passphrase)).unwrap();
        let parsed = KeyFile::parse(&protected.to_json().unwrap()).unwrap();
        assert!(parsed.is_protected());
        assert_eq!(parsed.label(), label);
        assert_eq!(
            parsed.unlock(Some(passphrase)).unwrap().to_bytes(),
            plain.unlock(None).unwrap().to_bytes()
        );
    }
}
