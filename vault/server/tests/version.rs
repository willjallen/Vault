use vault_server::version::{app_version, changelog_release_notes, parse_changelog_release_notes};

#[test]
fn changelog_parser_returns_only_numbered_release_sections() {
    /*
     * Feeds the parser unreleased text, two numbered releases, and an entry outside any release.
     * It checks that only versioned sections survive, labeled entries keep their kinds, and
     * plain release text becomes a note.
     */
    let changelog = r"
========

Unreleased

feat: Not shipped yet.

========

v2.1.0

feat: Add a focused release view.
fix: Keep acknowledgements synced.

========

v2.0.0

Initial release

========

feat: A new feature.
";

    let sections = parse_changelog_release_notes(changelog);

    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].version, "2.1.0");
    assert_eq!(sections[0].entries[0].kind, "feat");
    assert_eq!(sections[0].entries[0].text, "Add a focused release view.");
    assert_eq!(sections[0].entries[1].kind, "fix");
    assert_eq!(sections[1].version, "2.0.0");
    assert_eq!(sections[1].entries[0].kind, "note");
    assert_eq!(sections[1].entries[0].text, "Initial release");
}

#[test]
fn bundled_changelog_contains_the_current_app_version() {
    /*
     * Loads the release notes embedded in the server and the version reported by the
     * application. It requires a matching changelog section so a build cannot advertise a
     * version with no corresponding release notes.
     */
    let sections = changelog_release_notes();

    assert!(
        sections
            .iter()
            .any(|section| section.version == app_version()),
        "CHANGELOG.txt must contain a v{} section",
        app_version()
    );
}
