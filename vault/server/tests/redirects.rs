use vault_server::redirects::safe_redirect;

#[test]
fn safe_redirect_preserves_valid_origin_relative_spelling() {
    for value in [
        "/",
        "/Project",
        "/Project/Plan?tab=activity&filter=a%20b#history",
        "/caf%C3%A9",
        "/discount/100%25",
        "/search?q=https://example.com/a&email=user@example.com",
        "/@example.com",
        "/dots/../stay-local",
    ] {
        assert_eq!(safe_redirect(Some(value)), value, "{value}");
    }
}

#[test]
fn safe_redirect_rejects_external_and_ambiguous_forms() {
    for value in [
        "",
        "Project",
        "https://evil.example.com",
        "http://vault.invalid@evil.example.com",
        "//evil.example.com",
        "///evil.example.com",
        "//user:password@evil.example.com/path",
        "\\\\evil.example.com",
        "/\\evil.example.com",
        "/folder\\file",
    ] {
        assert_eq!(safe_redirect(Some(value)), "/", "{value:?}");
    }
    assert_eq!(safe_redirect(None), "/");
}

#[test]
fn safe_redirect_rejects_raw_and_percent_encoded_controls_or_separators() {
    for value in [
        "/line\nfeed",
        "/carriage\rreturn",
        "/nul\0byte",
        "/delete\u{7f}",
        "/%00",
        "/%09tab",
        "/%0D%0ASet-Cookie:bad",
        "/%1f",
        "/%7F",
        "/%C0%AF",
        "/%E0%80%AF",
        "/%FF",
        "/%2fevil.example.com",
        "/%2Fevil.example.com",
        "/%5cevil.example.com",
        "/%5Cevil.example.com",
        "/path?next=%2F%2Fevil.example.com",
    ] {
        assert_eq!(safe_redirect(Some(value)), "/", "{value:?}");
    }
}

#[test]
fn safe_redirect_rejects_nested_encoded_controls_and_separators() {
    for value in [
        "/%252fevil.example.com",
        "/%25252fevil.example.com",
        "/%2525252525252525252fevil.example.com",
        "/%255cevil.example.com",
        "/%250d%250aLocation:%252f%252fevil.example.com",
        "/%25%32%66evil.example.com",
        "/%25%35%43evil.example.com",
        "/%25%30%30",
    ] {
        assert_eq!(safe_redirect(Some(value)), "/", "{value}");
    }
}

#[test]
fn safe_redirect_rejects_malformed_percent_triplets() {
    for value in ["/%", "/%2", "/%GG", "/path?value=%Q0", "/trailing%"] {
        assert_eq!(safe_redirect(Some(value)), "/", "{value}");
    }
}
