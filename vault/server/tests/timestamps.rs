use vault_server::timestamps::{CANONICAL_LENGTH, canonicalize, is_canonical};

#[test]
fn canonicalizes_every_supported_historical_shape() {
    /*
     * Exercises every timestamp shape written by supported Vault releases plus an explicit
     * non-UTC offset. It verifies canonicalization preserves microseconds and converts the
     * represented instant to fixed-width UTC.
     */
    for (value, expected) in [
        ("2026-06-26 19:03:04", "2026-06-26T19:03:04.000000Z"),
        ("2026-06-26 19:03:04.123456", "2026-06-26T19:03:04.123456Z"),
        ("2026-06-26T19:03:04", "2026-06-26T19:03:04.000000Z"),
        (
            "2026-06-26T19:03:04.123456789",
            "2026-06-26T19:03:04.123456Z",
        ),
        (
            "2026-06-26T14:03:04.123456-05:00",
            "2026-06-26T19:03:04.123456Z",
        ),
        ("2026-06-26T19:03:04Z", "2026-06-26T19:03:04.000000Z"),
    ] {
        assert_eq!(canonicalize(value).expect("supported timestamp"), expected);
    }
}

#[test]
fn canonical_contract_is_fixed_width_and_strict() {
    /*
     * Checks that only the single persisted representation is considered canonical even when
     * another input is a valid RFC 3339 timestamp for the same UTC instant.
     */
    let canonical = "2026-06-26T19:03:04.123456Z";
    assert!(is_canonical(canonical));
    assert_eq!(canonical.len(), CANONICAL_LENGTH);
    for value in [
        "2026-06-26T19:03:04Z",
        "2026-06-26T19:03:04.123Z",
        "2026-06-26T19:03:04.123456+00:00",
        "2026-06-26 19:03:04.123456",
        "not-a-timestamp",
    ] {
        assert!(!is_canonical(value), "input {value}");
    }
}
