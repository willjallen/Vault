use serde::Serialize;

const VERSION_TEXT: &str = include_str!("../../../VERSION");
const CHANGELOG_TEXT: &str = include_str!("../../../CHANGELOG.txt");

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseNoteEntry {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseNotesSection {
    pub version: String,
    pub entries: Vec<ReleaseNoteEntry>,
}

#[must_use]
pub fn app_version() -> &'static str {
    VERSION_TEXT.trim()
}

#[must_use]
pub fn changelog_release_notes() -> Vec<ReleaseNotesSection> {
    parse_changelog_release_notes(CHANGELOG_TEXT)
}

#[must_use]
pub fn parse_changelog_release_notes(changelog: &str) -> Vec<ReleaseNotesSection> {
    let mut sections = Vec::new();
    let mut block = Vec::new();

    for line in changelog.lines() {
        if is_section_separator(line) {
            parse_release_block(&block, &mut sections);
            block.clear();
        } else {
            block.push(line);
        }
    }
    parse_release_block(&block, &mut sections);

    sections
}

fn is_section_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 8 && trimmed.bytes().all(|byte| byte == b'=')
}

fn parse_release_block(lines: &[&str], sections: &mut Vec<ReleaseNotesSection>) {
    let mut content = lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());
    let Some(title) = content.next() else {
        return;
    };
    let Some(version) = release_version_from_title(title) else {
        return;
    };
    let entries = content
        .map(|line| {
            let (kind, text) = line
                .split_once(':')
                .filter(|(kind, text)| is_note_kind(kind) && !text.trim().is_empty())
                .map_or(("note", line), |(kind, text)| (kind, text.trim()));
            ReleaseNoteEntry {
                kind: kind.to_string(),
                text: text.to_string(),
            }
        })
        .collect::<Vec<_>>();
    if !entries.is_empty() {
        sections.push(ReleaseNotesSection {
            version: version.to_string(),
            entries,
        });
    }
}

fn release_version_from_title(title: &str) -> Option<&str> {
    let version = title.strip_prefix('v')?;
    let starts_with_number = version.as_bytes().first().is_some_and(u8::is_ascii_digit);
    let safe = version
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'));
    (starts_with_number && safe).then_some(version)
}

fn is_note_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 16
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
}
