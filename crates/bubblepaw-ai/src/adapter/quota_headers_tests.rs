use super::*;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-20T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn headers(values: &[(&str, &str)]) -> HeaderMap {
    values
        .iter()
        .map(|(name, value)| {
            (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            )
        })
        .collect()
}
#[test]
fn codex_official_cli_fixture_and_older_relative_reset() {
    // https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/rate_limits.rs
    let fields = headers(&[
        ("x-codex-primary-used-percent", "12.5"),
        ("x-codex-primary-window-minutes", "60"),
        ("x-codex-primary-reset-at", "1704069000"),
        ("x-codex-primary-reset-after-seconds", "999"),
        ("x-codex-secondary-used-percent", "100"),
        ("x-codex-secondary-window-minutes", "10080"),
        ("x-codex-secondary-reset-after-seconds", "120"),
    ]);
    let readings = parse_codex_headers("codex", "personal", &fields, now());
    assert_eq!(readings.len(), 2);
    assert_eq!(readings[0].window, "primary");
    assert_eq!(readings[0].used_percent, 12.5);
    assert_eq!(
        readings[0].resets_at.as_deref(),
        Some("2024-01-01T00:30:00+00:00")
    );
    assert_eq!(
        readings[1].resets_at.as_deref(),
        Some("2026-10-20T12:02:00+00:00")
    );
    assert_eq!(readings[1].source, QuotaSource::ResponseHeaders);
}
#[test]
fn malformed_headers_are_unknown_and_zero_is_a_real_reading() {
    for value in ["NaN", "inf", "-1", "101", "bad"] {
        assert!(
            parse_codex_headers(
                "codex",
                "a",
                &headers(&[("x-codex-primary-used-percent", value)]),
                now()
            )
            .is_empty()
        );
    }
    assert_eq!(
        parse_codex_headers(
            "codex",
            "a",
            &headers(&[("x-codex-primary-used-percent", "0")]),
            now()
        )[0]
        .used_percent,
        0.0
    );
    assert!(parse_codex_headers("codex", "a", &HeaderMap::new(), now()).is_empty());
}
fn codex_primary(extra: &[(&str, &str)]) -> QuotaReading {
    let mut fields = vec![("x-codex-primary-used-percent", "40")];
    fields.extend_from_slice(extra);
    let mut readings = parse_codex_headers("codex", "a", &headers(&fields), now());
    assert_eq!(readings.len(), 1);
    let reading = readings.remove(0);
    assert_eq!(reading.observed_at, now().to_rfc3339());
    reading
}
#[test]
fn codex_missing_or_unusable_relative_reset_is_unknown_not_observed() {
    let cases: &[&[(&str, &str)]] = &[
        &[],
        &[("x-codex-primary-reset-after-seconds", "")],
        &[("x-codex-primary-reset-after-seconds", "0")],
        &[("x-codex-primary-reset-after-seconds", "-5")],
        &[("x-codex-primary-reset-after-seconds", "soon")],
        &[
            ("x-codex-primary-reset-at", ""),
            ("x-codex-primary-reset-after-seconds", ""),
        ],
        &[("x-codex-primary-reset-at", "soon")],
    ];
    for case in cases {
        assert_eq!(codex_primary(case).resets_at, None, "{case:?}");
    }
}
#[test]
fn codex_zero_relative_reset_does_not_borrow_secondary_or_observation_time() {
    let readings = parse_codex_headers(
        "codex",
        "a",
        &headers(&[
            ("x-codex-primary-used-percent", "10"),
            ("x-codex-primary-reset-after-seconds", "0"),
            ("x-codex-secondary-used-percent", "20"),
            ("x-codex-secondary-reset-after-seconds", "120"),
        ]),
        now(),
    );
    assert_eq!(readings.len(), 2);
    assert_eq!(readings[0].window, "primary");
    assert_eq!(readings[0].resets_at, None);
    assert_eq!(readings[1].window, "secondary");
    assert_eq!(
        readings[1].resets_at.as_deref(),
        Some("2026-10-20T12:02:00+00:00")
    );
}
#[test]
fn codex_positive_relative_reset_is_fallback_when_absolute_is_unusable() {
    for absolute in ["", "soon"] {
        let reading = codex_primary(&[
            ("x-codex-primary-reset-at", absolute),
            ("x-codex-primary-reset-after-seconds", "120"),
        ]);
        assert_eq!(
            reading.resets_at.as_deref(),
            Some("2026-10-20T12:02:00+00:00"),
            "reset-at {absolute:?}"
        );
    }
}
#[test]
fn codex_absolute_reset_takes_precedence_over_relative() {
    for relative in ["120", "0", "", "soon"] {
        let reading = codex_primary(&[
            ("x-codex-primary-reset-at", "1704069000"),
            ("x-codex-primary-reset-after-seconds", relative),
        ]);
        assert_eq!(
            reading.resets_at.as_deref(),
            Some("2024-01-01T00:30:00+00:00"),
            "reset-after-seconds {relative:?}"
        );
    }
    let alone = codex_primary(&[("x-codex-primary-reset-at", "1704069000")]);
    assert_eq!(
        alone.resets_at.as_deref(),
        Some("2024-01-01T00:30:00+00:00")
    );
    // Absolute epoch seconds retain their existing semantics, including
    // elapsed resets. The zero fix is specific to the relative fallback.
    assert_eq!(
        codex_primary(&[("x-codex-primary-reset-at", "0")])
            .resets_at
            .as_deref(),
        Some("1970-01-01T00:00:00+00:00")
    );
}
fn codex_credits(extra: &[(&str, &str)]) -> Vec<QuotaReading> {
    parse_codex_headers("codex", "a", &headers(extra), now())
}
#[test]
fn codex_credits_available_and_unavailable_use_has_credits() {
    let available = codex_credits(&[
        ("x-codex-credits-has-credits", "true"),
        ("x-codex-credits-unlimited", "false"),
        ("x-codex-credits-balance", "12.50"),
    ]);
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].window, "credits");
    assert_eq!(available[0].used_percent, 0.0);
    assert_eq!(available[0].resets_at, None);
    assert_eq!(available[0].balance.as_deref(), Some("12.50"));
    assert_eq!(available[0].source, QuotaSource::ResponseHeaders);
    assert_eq!(available[0].observed_at, now().to_rfc3339());
    let unavailable = codex_credits(&[
        ("x-codex-credits-has-credits", "false"),
        ("x-codex-credits-unlimited", "0"),
        ("x-codex-credits-balance", "0.00"),
    ]);
    assert_eq!(unavailable.len(), 1);
    assert_eq!(unavailable[0].used_percent, 100.0);
    assert_eq!(unavailable[0].resets_at, None);
    assert_eq!(unavailable[0].balance.as_deref(), Some("0.00"));
    let verbatim = codex_credits(&[
        ("x-codex-credits-has-credits", "true"),
        ("x-codex-credits-unlimited", "false"),
        ("x-codex-credits-balance", " 0012.500 "),
    ]);
    assert_eq!(verbatim[0].balance.as_deref(), Some(" 0012.500 "));
}
#[test]
fn codex_credits_unlimited_reports_available_with_unlimited_balance() {
    let unlimited = codex_credits(&[
        ("x-codex-credits-has-credits", "FALSE"),
        ("x-codex-credits-unlimited", "True"),
        ("x-codex-credits-balance", "7"),
    ]);
    assert_eq!(unlimited[0].used_percent, 0.0);
    assert_eq!(unlimited[0].balance.as_deref(), Some("unlimited"));
    let both = codex_credits(&[
        ("x-codex-credits-has-credits", "1"),
        ("x-codex-credits-unlimited", "1"),
        ("x-codex-credits-balance", "5"),
    ]);
    assert_eq!(both[0].used_percent, 0.0);
    assert_eq!(both[0].balance.as_deref(), Some("unlimited"));
    let without_balance = codex_credits(&[
        ("x-codex-credits-has-credits", "0"),
        ("x-codex-credits-unlimited", "1"),
    ]);
    assert_eq!(without_balance[0].used_percent, 0.0);
    assert_eq!(without_balance[0].balance.as_deref(), Some("unlimited"));
}
#[test]
fn codex_credits_balance_is_absent_when_header_is_missing_or_empty() {
    for balance in [None, Some("")] {
        let mut fields = vec![
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
        ];
        if let Some(value) = balance {
            fields.push(("x-codex-credits-balance", value));
        }
        let readings = codex_credits(&fields);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].balance, None, "{balance:?}");
        assert_eq!(readings[0].used_percent, 0.0);
    }
}
#[test]
fn codex_credits_partial_or_malformed_pairs_establish_no_reading() {
    for fields in [
        &[("x-codex-credits-has-credits", "false")][..],
        &[
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", "0"),
        ][..],
        &[
            ("x-codex-credits-has-credits", "yes"),
            ("x-codex-credits-unlimited", "false"),
        ][..],
        &[
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "maybe"),
        ][..],
        &[
            ("x-codex-credits-has-credits", ""),
            ("x-codex-credits-unlimited", "false"),
        ][..],
        &[
            ("x-codex-credits-has-credits", "2"),
            ("x-codex-credits-unlimited", "0"),
        ][..],
    ] {
        assert!(codex_credits(fields).is_empty(), "{fields:?}");
    }
}
#[test]
fn codex_credits_append_after_windows_and_never_remove_them() {
    let readings = codex_credits(&[
        ("x-codex-primary-used-percent", "40"),
        ("x-codex-credits-has-credits", "true"),
        ("x-codex-credits-unlimited", "false"),
        ("x-codex-credits-balance", "3"),
    ]);
    assert_eq!(
        readings
            .iter()
            .map(|reading| reading.window.as_str())
            .collect::<Vec<_>>(),
        ["primary", "credits"]
    );
    assert_eq!(readings[0].balance, None);
    assert_eq!(readings[1].balance.as_deref(), Some("3"));
    let partial = codex_credits(&[
        ("x-codex-primary-used-percent", "40"),
        ("x-codex-credits-has-credits", "maybe"),
        ("x-codex-credits-unlimited", "false"),
    ]);
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].window, "primary");
}
#[test]
fn anthropic_independent_buckets_validate_pairs() {
    let mut fields = HeaderMap::new();
    for bucket in ["requests", "tokens", "input-tokens", "output-tokens"] {
        fields.extend(headers(&[
            (&format!("anthropic-ratelimit-{bucket}-limit"), "200"),
            (&format!("anthropic-ratelimit-{bucket}-remaining"), "50"),
            (
                &format!("anthropic-ratelimit-{bucket}-reset"),
                "2026-10-20T12:01:00Z",
            ),
        ]));
    }
    let readings = parse_anthropic_headers("anthropic", "a", &fields, now());
    assert_eq!(readings.len(), 4);
    assert_eq!(
        readings
            .iter()
            .map(|reading| reading.window.as_str())
            .collect::<Vec<_>>(),
        ["requests", "tokens", "input_tokens", "output_tokens"]
    );
    assert!(
        readings
            .iter()
            .all(|reading| reading.used_percent == 75.0 && reading.validate().is_ok())
    );
    for (limit, remaining) in [("0", "0"), ("NaN", "1"), ("10", "11"), ("10", "-1")] {
        assert!(
            parse_anthropic_headers(
                "anthropic",
                "a",
                &headers(&[
                    ("anthropic-ratelimit-tokens-limit", limit),
                    ("anthropic-ratelimit-tokens-remaining", remaining)
                ]),
                now()
            )
            .is_empty()
        );
    }
}
