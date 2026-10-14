//! English presentation helpers. Retained names, paths and diagnostics stay exact.
//!
//! Existing call sites keep these small helpers to avoid touching data handling.
//! There is no catalog, locale state, template matching, or worker translation.
use gp200_domain::LanguagePreference;
use std::borrow::Cow;

pub fn text(source: &str) -> &str {
    source
}

/// Kept for dialog-worker call sites and the one-English preference compatibility
/// field. Former preference tags deserialize to English before reaching workers.
pub fn text_for(_language: LanguagePreference, source: &str) -> &str {
    source
}

pub fn description<'a>(_module: &str, _code: u32, source: &'a str) -> &'a str {
    source
}

pub fn choice(source: &str) -> &str {
    source
}

pub fn value(source: &str) -> Cow<'_, str> {
    Cow::Borrowed(source)
}

pub fn style(
    style: gp200_domain::NativeStyleId,
    globals: Option<&gp200_domain::RawTransfer>,
) -> String {
    style.label(globals)
}

pub fn message(source: &str) -> Cow<'_, str> {
    Cow::Borrowed(source)
}
