//! Display languages for item text.
//!
//! Deliberately independent of [`Region`](crate::market::Region), because the
//! upstream is: an unlocalised static-item response carries *every* language
//! Blizzard publishes, whichever region's host answered it. Verified against
//! `static-eu`, `static-us`, `static-kr` and `static-tw`, which return byte-
//! identical locale sets.
//!
//! That matters for two reasons. Someone can read Korean prices in German.
//! And the tooltip cache is keyed by language alone, so the four regions share
//! one copy of the text rather than four.
//!
//! The list below is exactly what the API returns -- no more (`pt_PT` is not
//! published, and asking for it used to fall back to a different language
//! without saying so) and no less (`zh_CN` is in every payload even though
//! mainland China is a separate host we do not price).

use core::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locale {
    EnGb,
    EnUs,
    DeDe,
    EsEs,
    EsMx,
    FrFr,
    ItIt,
    PtBr,
    RuRu,
    KoKr,
    ZhTw,
    ZhCn,
}

/// Every language the item endpoint returns, in picker order: English first,
/// then the rest by rough player count.
pub const ALL_LOCALES: [Locale; 12] = [
    Locale::EnGb,
    Locale::EnUs,
    Locale::DeDe,
    Locale::EsEs,
    Locale::EsMx,
    Locale::FrFr,
    Locale::ItIt,
    Locale::PtBr,
    Locale::RuRu,
    Locale::KoKr,
    Locale::ZhTw,
    Locale::ZhCn,
];

/// What a visitor gets before choosing anything and before `Accept-Language`
/// has been consulted.
pub const DEFAULT_LOCALE: Locale = Locale::EnGb;

impl Locale {
    /// Blizzard's own code, and the key their localised strings arrive under.
    pub const fn code(self) -> &'static str {
        match self {
            Locale::EnGb => "en_GB",
            Locale::EnUs => "en_US",
            Locale::DeDe => "de_DE",
            Locale::EsEs => "es_ES",
            Locale::EsMx => "es_MX",
            Locale::FrFr => "fr_FR",
            Locale::ItIt => "it_IT",
            Locale::PtBr => "pt_BR",
            Locale::RuRu => "ru_RU",
            Locale::KoKr => "ko_KR",
            Locale::ZhTw => "zh_TW",
            Locale::ZhCn => "zh_CN",
        }
    }

    /// The same thing as an HTML `lang` attribute wants it.
    ///
    /// Worth getting right: a tooltip in German inside an English page is
    /// exactly the case `lang` exists for, and screen readers switch voice on
    /// it.
    pub const fn bcp47(self) -> &'static str {
        match self {
            Locale::EnGb => "en-GB",
            Locale::EnUs => "en-US",
            Locale::DeDe => "de-DE",
            Locale::EsEs => "es-ES",
            Locale::EsMx => "es-MX",
            Locale::FrFr => "fr-FR",
            Locale::ItIt => "it-IT",
            Locale::PtBr => "pt-BR",
            Locale::RuRu => "ru-RU",
            Locale::KoKr => "ko-KR",
            Locale::ZhTw => "zh-TW",
            Locale::ZhCn => "zh-CN",
        }
    }

    /// The language's name in that language -- someone looking for Deutsch is
    /// not looking for "German". Variants of one language carry their country,
    /// because all twelve sit in the same list.
    pub const fn label(self) -> &'static str {
        match self {
            Locale::EnGb => "English (GB)",
            Locale::EnUs => "English (US)",
            Locale::DeDe => "Deutsch",
            Locale::EsEs => "Español (ES)",
            Locale::EsMx => "Español (MX)",
            Locale::FrFr => "Français",
            Locale::ItIt => "Italiano",
            Locale::PtBr => "Português (BR)",
            Locale::RuRu => "Русский",
            Locale::KoKr => "한국어",
            Locale::ZhTw => "繁體中文",
            Locale::ZhCn => "简体中文",
        }
    }

    /// The two-letter language. Used for matching a browser that asked for
    /// `de` without saying which German, and for picking an interface
    /// catalogue: `es_ES` and `es_MX` share one Spanish interface.
    pub const fn language(self) -> &'static str {
        match self {
            Locale::EnGb | Locale::EnUs => "en",
            Locale::DeDe => "de",
            Locale::EsEs | Locale::EsMx => "es",
            Locale::FrFr => "fr",
            Locale::ItIt => "it",
            Locale::PtBr => "pt",
            Locale::RuRu => "ru",
            Locale::KoKr => "ko",
            Locale::ZhTw | Locale::ZhCn => "zh",
        }
    }

    /// Accepts `de_DE` and `de-DE` alike: one comes from our cookie, the other
    /// from a browser.
    pub fn parse(value: &str) -> Option<Locale> {
        let value = value.trim().replace('-', "_");
        ALL_LOCALES
            .into_iter()
            .find(|l| l.code().eq_ignore_ascii_case(&value))
    }

    /// Best match for one `Accept-Language` header.
    ///
    /// Honours the order the browser gave and ignores the q-values: they only
    /// matter for re-ordering, and browsers already send their list in
    /// preference order.
    pub fn from_accept_language(header: &str) -> Option<Locale> {
        for tag in header.split(',') {
            let tag = tag.split(';').next().unwrap_or("").trim();
            if tag.is_empty() || tag == "*" {
                continue;
            }
            if let Some(exact) = Locale::parse(tag) {
                return Some(exact);
            }
            // `de` on its own, or a variant we do not carry such as `de-AT`.
            let language = tag.split(['-', '_']).next().unwrap_or("").to_lowercase();
            if let Some(hit) = ALL_LOCALES.into_iter().find(|l| l.language() == language) {
                return Some(hit);
            }
        }
        None
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_separators() {
        assert_eq!(Locale::parse("de_DE"), Some(Locale::DeDe));
        assert_eq!(Locale::parse("de-DE"), Some(Locale::DeDe));
        assert_eq!(Locale::parse("klingon"), None);
        // Not published by the item endpoint, so not on offer.
        assert_eq!(Locale::parse("pt_PT"), None);
    }

    #[test]
    fn reads_a_browsers_language_list() {
        assert_eq!(
            Locale::from_accept_language("de-DE,de;q=0.9,en;q=0.8"),
            Some(Locale::DeDe)
        );
        // A variant we do not carry falls back to the same language.
        assert_eq!(
            Locale::from_accept_language("de-AT,en;q=0.5"),
            Some(Locale::DeDe)
        );
        // First understood entry wins, skipping ones we do not have.
        assert_eq!(
            Locale::from_accept_language("cy-GB,ko-KR;q=0.7"),
            Some(Locale::KoKr)
        );
        assert_eq!(Locale::from_accept_language("*"), None);
    }
}
