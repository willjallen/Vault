const VERSION_TEXT: &str = include_str!("../../../VERSION");

#[must_use]
pub fn app_version() -> &'static str {
    VERSION_TEXT.trim()
}
