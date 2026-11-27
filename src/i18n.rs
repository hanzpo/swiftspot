//! Interface strings. SwiftSpot is English-only: these helpers return the
//! English source text, choosing the singular or plural form by count, and
//! keep the call sites free of hand-built plurals.

use std::borrow::Cow;

/// The interface language. English is the only one.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Locale {
    #[default]
    English,
}

/// The interface text for `source`.
pub fn gettext(_locale: Locale, source: &'static str) -> Cow<'static, str> {
    Cow::Borrowed(source)
}

/// The interface text for a phrase whose meaning depends on `context`.
pub fn pgettext(
    _locale: Locale,
    _context: &'static str,
    source: &'static str,
) -> Cow<'static, str> {
    Cow::Borrowed(source)
}

/// The singular form when `count` is one, otherwise the plural form.
pub fn ngettext(
    _locale: Locale,
    singular: &'static str,
    plural: &'static str,
    count: u32,
) -> Cow<'static, str> {
    Cow::Borrowed(if count == 1 { singular } else { plural })
}
