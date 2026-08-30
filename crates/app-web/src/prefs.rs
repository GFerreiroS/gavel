//! Per-visitor market preferences: which region's prices, in which language,
//! and what "usual" means when a price is called cheap or dear.
//!
//! Resolution order, highest first:
//!
//! 1. `?region=` / `?lang=` / `?baseline=` on the request -- shareable, and
//!    what the picker submits.
//! 2. The `wow_tracker_market` cookie, so the choice survives the next visit.
//! 3. For the language only, the browser's `Accept-Language`, so a German
//!    browser gets German without touching the picker.
//! 4. The server's defaults: the first collected region, and English.
//!
//! Region and language are independent. Item text comes back in every
//! language from every regional host, so reading Korean prices in German is a
//! perfectly ordinary thing to want.
//!
//! A middleware rather than an extractor because the cookie has to be written
//! on the way *out*, and every page wants the same answer without repeating
//! the resolution.

use app_core::Ports;
use app_core::locale::{DEFAULT_LOCALE, Locale};
use app_core::market::Region;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::session::cookie_value;

pub const MARKET_COOKIE: &str = "wow_tracker_market";
/// A year: this is a display preference, not a session.
const COOKIE_MAX_AGE: u64 = 31_536_000;

/// The windows the "vs usual" percentage may compare against, in days, with
/// the label each carries in the picker.
///
/// A closed set rather than any number: the figure only means something next
/// to a window a visitor can name, and an arbitrary `?baseline=` is an
/// unbounded scan of the price history for anyone who asks for one.
pub const BASELINE_CHOICES: [(u64, &str); 5] = [
    (1, "Last 24 hours"),
    (3, "Last 3 days"),
    (7, "Last 7 days"),
    (14, "Last 14 days"),
    (30, "Last 30 days"),
];

/// A week: long enough to average over a full raid week, short enough that a
/// price which has moved since launch does not still read as normal.
pub const DEFAULT_BASELINE_DAYS: u64 = 7;

/// The resolved choice for one request, injected as a request extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketPrefs {
    pub region: Region,
    pub locale: Locale,
    /// How far back "usual" reaches, for the +/- percentage on every price.
    pub baseline_days: u64,
}

/// Which connected realm's gear prices to show, as its slug.
///
/// `None` is a real answer, not a missing one: it means the cross-realm view,
/// which is what the gear pages open on.
///
/// A *name* rather than an id, because it goes in a URL a person reads --
/// `?region=eu&realm=draenor`. The previous `eu:1403` form was worse than
/// ugly: a browser percent-encodes the colon, this hand-rolled parser did not
/// decode it, and every realm choice silently fell back to "all realms".
///
/// Its own extension rather than a field on [`MarketPrefs`], which is `Copy`
/// and passed by value through every handler; one `String` would have made
/// clones of it the most common operation in the web layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RealmChoice(pub Option<String>);

impl MarketPrefs {
    fn cookie_value(&self, realm: &RealmChoice) -> String {
        format!(
            "{}|{}|{}|{}",
            self.region.as_str(),
            self.locale.code(),
            self.baseline_days,
            realm.0.as_deref().unwrap_or_default()
        )
    }

    /// Region, language, baseline, gear realm. Older cookies carry fewer
    /// fields, and a missing field is "not chosen" rather than an error --
    /// nobody should be logged out of their language by a deploy.
    fn parse_cookie(raw: &str) -> Choice {
        let mut fields = raw.split('|');
        Choice {
            region: fields.next().and_then(Region::parse),
            locale: fields.next().and_then(Locale::parse),
            baseline: fields.next().and_then(parse_baseline),
            // An empty field is a remembered "all realms", which is different
            // from never having chosen.
            realm: fields.next().map(slug),
        }
    }
}

/// What one source (the query, the cookie) had to say. Every field optional:
/// each is resolved independently, so a cookie that only knows the region
/// still contributes it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Choice {
    region: Option<Region>,
    locale: Option<Locale>,
    baseline: Option<u64>,
    /// Three states, and all three are needed: absent means "not mentioned",
    /// `Some(None)` means "explicitly all realms", `Some(Some(..))` is a
    /// choice. Without the middle one there would be no way to go back to the
    /// cross-realm view once a realm had been picked.
    realm: Option<Option<String>>,
}

/// A realm slug: lower case, punctuation and spaces collapsed to hyphens.
/// "Dentarg, Tarren Mill" becomes `dentarg-tarren-mill`.
///
/// Empty is "all realms", which is a perfectly good answer and the default.
/// Anything unrecognised resolves to the same thing at the point of use, so a
/// stale bookmark shows every realm rather than an error.
pub fn slug(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// A baseline window, but only one that is on the menu.
fn parse_baseline(raw: &str) -> Option<u64> {
    let days: u64 = raw.parse().ok()?;
    BASELINE_CHOICES
        .iter()
        .any(|(offered, _)| *offered == days)
        .then_some(days)
}

pub async fn layer<E: Ports>(State(env): State<E>, mut request: Request, next: Next) -> Response {
    let query = from_query(request.uri().query());
    let cookie = cookie_value(request.headers(), MARKET_COOKIE)
        .map(|raw| MarketPrefs::parse_cookie(&raw))
        .unwrap_or_default();

    // Only offer regions this instance actually collects: a region with no
    // prices is a blank page, which is worse than not offering it.
    let collected = &env.market().regions;
    let allowed = |region: Option<Region>| region.filter(|r| collected.contains(r));

    let region = allowed(query.region)
        .or_else(|| allowed(cookie.region))
        .or_else(|| collected.first().copied())
        .unwrap_or(Region::Eu);

    let locale = query
        .locale
        .or(cookie.locale)
        .or_else(|| accept_language(request.headers()))
        .unwrap_or(DEFAULT_LOCALE);

    let baseline_days = query
        .baseline
        .or(cookie.baseline)
        .unwrap_or(DEFAULT_BASELINE_DAYS);

    // Not validated here: the middleware has no realm names to check against
    // -- they live in the store. An unknown slug becomes "all realms" where
    // it is resolved, which is the right answer for a stale bookmark anyway.
    let realm = RealmChoice(query.realm.clone().or(cookie.realm).flatten());

    let prefs = MarketPrefs {
        region,
        locale,
        baseline_days,
    };
    request.extensions_mut().insert(prefs);
    request.extensions_mut().insert(realm.clone());
    let mut response = next.run(request).await;

    // Remember an explicit choice, and only that: writing the cookie on every
    // request would pin the default the first time anyone loaded a page.
    if query != Choice::default()
        && let Ok(value) = HeaderValue::from_str(&format!(
            "{MARKET_COOKIE}={}; Path=/; SameSite=Lax; Max-Age={COOKIE_MAX_AGE}",
            prefs.cookie_value(&realm)
        ))
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

fn accept_language(headers: &HeaderMap) -> Option<Locale> {
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(Locale::from_accept_language)
}

/// Pull `region`, `lang` and `baseline` out of a raw query string.
///
/// Hand-rolled because this runs on every request and the alternative is
/// deserialising the whole query twice -- once here and once in the handler
/// that actually wanted it.
fn from_query(query: Option<&str>) -> Choice {
    let Some(query) = query else {
        return Choice::default();
    };
    let mut choice = Choice::default();
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("region", value)) => choice.region = Region::parse(value),
            Some(("lang", value)) => choice.locale = Locale::parse(value),
            Some(("baseline", value)) => choice.baseline = parse_baseline(value),
            // `?realm=` with nothing after it is how the page says "all
            // realms", so an empty value is a choice rather than a no-op.
            Some(("realm", value)) => choice.realm = Some(slug(value)),
            _ => {}
        }
    }
    choice
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_beats_a_cookie() {
        let choice = from_query(Some("region=kr&lang=ko_KR&baseline=30"));
        assert_eq!(choice.region, Some(Region::Kr));
        assert_eq!(choice.locale, Some(Locale::KoKr));
        assert_eq!(choice.baseline, Some(30));
    }

    #[test]
    fn an_unknown_value_is_ignored_rather_than_an_error() {
        assert_eq!(from_query(Some("region=mars&lang=tlh")), Choice::default());
    }

    /// The window is a menu, not a number: `?baseline=99999` would otherwise
    /// scan the whole price history on request.
    #[test]
    fn a_baseline_off_the_menu_is_refused() {
        assert_eq!(parse_baseline("7"), Some(7));
        assert_eq!(parse_baseline("9"), None);
        assert_eq!(parse_baseline("99999"), None);
        assert_eq!(parse_baseline("-1"), None);
        assert_eq!(parse_baseline("seven"), None);
    }

    #[test]
    fn a_cookie_round_trips() {
        let prefs = MarketPrefs {
            region: Region::Tw,
            locale: Locale::ZhTw,
            baseline_days: 14,
        };
        let realm = RealmChoice(Some("draenor".into()));
        let choice = MarketPrefs::parse_cookie(&prefs.cookie_value(&realm));
        assert_eq!(choice.region, Some(Region::Tw));
        assert_eq!(choice.locale, Some(Locale::ZhTw));
        assert_eq!(choice.baseline, Some(14));
        assert_eq!(choice.realm, Some(Some("draenor".into())));
    }

    /// The cross-realm view is a choice you can go back to, so it has to
    /// survive the cookie as something other than "never chose".
    #[test]
    fn all_realms_round_trips_as_a_choice() {
        let prefs = MarketPrefs {
            region: Region::Eu,
            locale: Locale::EnGb,
            baseline_days: 7,
        };
        let choice = MarketPrefs::parse_cookie(&prefs.cookie_value(&RealmChoice(None)));
        assert_eq!(choice.realm, Some(None), "remembered as all realms");
        assert_eq!(from_query(Some("realm=")).realm, Some(None));
    }

    /// The bug this scheme replaced: a browser percent-encodes a colon, the
    /// old `eu:1403` value never survived it, and every realm choice silently
    /// became "all realms". A slug has nothing to encode.
    #[test]
    fn a_realm_slug_survives_a_url() {
        assert_eq!(
            slug("Dentarg, Tarren Mill").as_deref(),
            Some("dentarg-tarren-mill")
        );
        assert_eq!(slug("Draenor").as_deref(), Some("draenor"));
        assert_eq!(slug("Zul'jin").as_deref(), Some("zul-jin"));
        assert_eq!(slug("  "), None);
        assert_eq!(
            from_query(Some("region=eu&realm=dentarg-tarren-mill")).realm,
            Some(Some("dentarg-tarren-mill".into()))
        );
    }

    /// Cookies written before the baseline existed have two fields. They must
    /// keep working, with the window falling back to the default.
    #[test]
    fn an_older_two_field_cookie_still_carries_its_choice() {
        let choice = MarketPrefs::parse_cookie("us|es_ES");
        assert_eq!(choice.region, Some(Region::Us));
        assert_eq!(choice.locale, Some(Locale::EsEs));
        assert_eq!(choice.baseline, None);
        assert_eq!(choice.realm, None, "never chose, rather than chose none");
    }

    /// The reader's comparison window used to be turned into a `since`
    /// timestamp and reduced during the request. It is now a stored row, which
    /// means the choice can only ever be one that was materialised -- so the
    /// list a cookie may hold and the list the materialiser writes have to be
    /// the same list.
    #[test]
    fn every_window_a_reader_can_choose_is_one_that_was_materialised() {
        for (days, _) in BASELINE_CHOICES {
            assert!(
                app_core::market::window::ROLLING_DAYS.contains(&days),
                "{days}d is offered but never materialised"
            );
        }
        // And nothing else is accepted from a cookie or a query string.
        for days in [0, 2, 5, 31, 365] {
            assert_eq!(
                parse_baseline(&days.to_string()),
                None,
                "{days}d should be refused"
            );
        }
    }

    #[test]
    fn a_browsers_language_seeds_the_default() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_LANGUAGE, "fr-FR,fr;q=0.9".parse().unwrap());
        assert_eq!(accept_language(&headers), Some(Locale::FrFr));

        assert_eq!(accept_language(&HeaderMap::new()), None);
    }
}
