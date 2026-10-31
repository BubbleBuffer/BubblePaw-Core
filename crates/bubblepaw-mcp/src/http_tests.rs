use super::*;

#[test]
fn mirrored_headers_encode_controls_unicode_whitespace_and_literal_sentinel() {
    for value in ["hello", "", "42", "true", "a\tb"] {
        assert_eq!(encode_header(value).unwrap().to_str().unwrap(), value);
    }
    for value in [
        " padded ",
        "line\nnext",
        "世界",
        "=?base64?literal?=",
        "\ttab",
    ] {
        assert_eq!(
            encode_header(value).unwrap().to_str().unwrap(),
            format!("=?base64?{}?=", STANDARD.encode(value))
        );
    }
}

#[test]
fn annotation_validation_requires_unique_primitive_properties_paths() {
    let valid = json!({"type":"object","properties":{"nested":{"type":"object","properties":{
        "value":{"type":"integer","x-mcp-header":"Value"}}}}});
    let mut headers = Vec::new();
    inspect_schema(
        &valid,
        &mut Vec::new(),
        false,
        &mut BTreeSet::new(),
        &mut headers,
    )
    .unwrap();
    assert_eq!(headers[0].path, ["nested", "value"]);
    for invalid in [
        json!({"type":"string","x-mcp-header":"Root"}),
        json!({"properties":{"value":{"type":"number","x-mcp-header":"Value"}}}),
        json!({"properties":{"a":{"type":"string","x-mcp-header":"value"},"b":{"type":"string","x-mcp-header":"Value"}}}),
        json!({"items":{"properties":{"value":{"type":"string","x-mcp-header":"Value"}}}}),
        json!({"properties":{"value":{"type":"string","x-mcp-header":"bad\r\nname"}}}),
        json!({"$defs":{"value":{"type":"string","x-mcp-header":"Value"}}}),
    ] {
        assert!(
            inspect_schema(
                &invalid,
                &mut Vec::new(),
                false,
                &mut BTreeSet::new(),
                &mut Vec::new()
            )
            .is_err()
        );
    }
    assert!(primitive_value(&json!(SAFE_INTEGER + 1), Primitive::Integer).is_err());
    assert_eq!(
        primitive_value(&json!(42.0), Primitive::Integer).unwrap(),
        "42"
    );
}

#[test]
fn sse_parser_handles_split_crlf_multiline_comments_cursor_and_retry() {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for byte in b"\xef\xbb\xbf:keepalive\r\nid: cursor\r\nretry: 27\r\ndata:\r\n\r\ndata: {\r\ndata: \"ok\":true}\r\n\r\n" {
        if let Some(event) = parser.push(*byte).unwrap() { events.push(event); }
    }
    assert_eq!(parser.last_id.as_deref(), Some("cursor"));
    assert_eq!(parser.retry, Duration::from_millis(27));
    assert_eq!(events, [Vec::new(), b"{\n\"ok\":true}".to_vec()]);
    let mut parser = SseParser::default();
    for _ in 0..FRAME_LIMIT {
        parser.push(b'x').unwrap();
    }
    assert!(parser.push(b'x').is_err());
}
