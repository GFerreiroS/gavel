//! The realm picker's list.
//!
//! A `<datalist>` was the first attempt: native, no script, and the browser
//! draws it wherever it likes -- which on a wide window is a column floating
//! off to the right of the box it belongs to. There is no styling that fixes
//! that; the popup is the browser's, not the page's.
//!
//! So the list is ours: a `<details>` that drops open under its summary, a
//! search box that filters it here on the server, and one link per realm.
//! Links, not a `<select>`, because choosing a realm *is* going somewhere --
//! nothing to press afterwards, and it still works with scripting off.

use app_core::Ports;
use app_core::market::Realm;
use app_core::repo::{RealmPriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::response::Html;
use serde::Deserialize;

use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::{MarketPrefs, RealmChoice, slug};
use crate::render::page;
use crate::views::RealmOption;

/// Most realms to put in the list at once.
///
/// A region has a few hundred, and every one of them is a link with a name in
/// it. Past this the list is not a list any more, it is a page -- and the
/// search box is right there.
const MAX_SHOWN: usize = 60;

#[derive(Debug, Deserialize)]
pub struct RealmQuery {
    /// What was typed into the picker's search box.
    #[serde(default)]
    pub q: String,
    /// Which per-realm page the links should lead to. Anything else is gear:
    /// the value names one of two pages, and it must never become a path.
    #[serde(default)]
    pub kind: String,
    /// Carried through so a chosen realm keeps the expansion in view.
    #[serde(default)]
    pub expansion: String,
}

#[derive(Template)]
#[template(path = "partials/realms.html")]
pub struct RealmListFragment {
    pub realms: Vec<RealmOption>,
    /// Where each link goes, minus the realm.
    pub base: String,
    /// The link that clears the choice.
    pub all_href: String,
}

/// Lowercase, and without the accents.
///
/// Realm names carry them -- "Aggra (Português)", "Chants éternels",
/// "Nagrand'" -- and nobody reaches for a dead key to find their own realm in
/// a list. Only the Latin-1 letters are folded, which is all the realm names
/// that have accents use; a Cyrillic or Korean name is left as it is and
/// still matches itself.
fn fold(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'ý' | 'ÿ' => 'y',
            other => other,
        })
        .collect()
}

/// `GET /partials/realms`
pub async fn fragment<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(chosen): Extension<RealmChoice>,
    Query(params): Query<RealmQuery>,
) -> WebResult<Html<String>> {
    let realms: Vec<Realm> = env
        .store()
        .realm_prices()
        .realms()
        .await?
        .into_iter()
        .filter(|r| r.region == prefs.region)
        .collect();

    page(
        &build(&realms, &params, prefs, chosen.0.as_deref()),
        prefs.locale,
    )
}

pub(crate) fn build(
    realms: &[Realm],
    params: &RealmQuery,
    prefs: MarketPrefs,
    chosen: Option<&str>,
) -> RealmListFragment {
    let path = match params.kind.as_str() {
        "recipes" => "/wow/auctions/recipes",
        _ => "/wow/auctions/gear",
    };
    let base = format!(
        "{path}?expansion={}&region={}",
        super::gear::query_value(&params.expansion),
        prefs.region.as_str(),
    );

    let needle = super::reagents::normalise(Some(&params.q)).map(|q| fold(&q));
    let mut matches: Vec<RealmOption> = super::gear::realm_options(realms)
        .into_iter()
        .filter(|option| match &needle {
            None => true,
            Some(needle) => fold(&option.name).contains(needle.as_str()),
        })
        .collect();

    // The current choice first, so it is never scrolled off by a long list.
    matches.sort_by_key(|option| (slug(&option.name).as_deref() != chosen, option.name.clone()));

    // Cut without announcing it. Extra explanatory copy under a search box,
    // next to a scrollbar, only repeats what those controls already convey.
    matches.truncate(MAX_SHOWN);

    RealmListFragment {
        realms: matches,
        base: base.clone(),
        all_href: format!("{base}&realm="),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::market::{RealmId, Region};

    fn realm(id: u32, members: &[&str]) -> Realm {
        Realm {
            id: RealmId(id),
            region: Region::Eu,
            name: members.join(", "),
            members: members.iter().map(|m| (*m).to_string()).collect(),
            locale: "enGB".into(),
            enabled: true,
        }
    }

    fn prefs() -> MarketPrefs {
        MarketPrefs {
            region: Region::Eu,
            locale: app_core::locale::DEFAULT_LOCALE,
            baseline_days: 7,
        }
    }

    fn query(q: &str, kind: &str) -> RealmQuery {
        RealmQuery {
            q: q.into(),
            kind: kind.into(),
            expansion: "midnight".into(),
        }
    }

    fn names(fragment: &RealmListFragment) -> Vec<&str> {
        fragment.realms.iter().map(|r| r.name.as_str()).collect()
    }

    /// A connected realm is several realms, and each is findable by its own
    /// name -- which is the name a player knows their market by.
    #[test]
    fn every_realm_in_a_shared_house_is_its_own_entry() {
        let realms = [realm(509, &["Garona", "Sargeras", "Ner'zhul"])];
        let all = build(&realms, &query("", "gear"), prefs(), None);
        assert_eq!(names(&all), ["Garona", "Ner'zhul", "Sargeras"]);

        let one = build(&realms, &query("sarg", "gear"), prefs(), None);
        assert_eq!(names(&one), ["Sargeras"]);
    }

    /// Typed without the diacritic, because that is how it gets typed.
    #[test]
    fn search_ignores_accents_and_case() {
        let realms = [realm(1, &["Aggra (Português)"]), realm(2, &["Alleria"])];
        let found = build(&realms, &query("PORTUGUES", "gear"), prefs(), None);
        assert_eq!(names(&found), ["Aggra (Português)"]);
    }

    /// The realm you are looking at must not be scrolled off its own list.
    #[test]
    fn the_current_realm_leads() {
        let realms = [realm(1, &["Zul'jin"]), realm(2, &["Aegwynn"])];
        let list = build(&realms, &query("", "gear"), prefs(), Some("zul-jin"));
        assert_eq!(names(&list), ["Zul'jin", "Aegwynn"]);
    }

    /// `kind` names one of two pages and must never become a path of its own.
    #[test]
    fn the_link_target_is_one_of_two_pages() {
        let realms = [realm(1, &["Aegwynn"])];
        for (kind, expected) in [
            ("gear", "/wow/auctions/gear"),
            ("recipes", "/wow/auctions/recipes"),
            ("", "/wow/auctions/gear"),
            ("../../admin", "/wow/auctions/gear"),
            ("https://evil.example", "/wow/auctions/gear"),
        ] {
            let list = build(&realms, &query("", kind), prefs(), None);
            assert!(
                list.base.starts_with(expected),
                "{kind:?} led to {}",
                list.base
            );
        }
    }

    /// A long list is cut rather than rendered in full: a region has a few
    /// hundred realms and every one of them is a link with a name in it.
    #[test]
    fn a_long_list_is_cut() {
        let owned: Vec<String> = (0..MAX_SHOWN + 10)
            .map(|i| format!("Realm{i:03}"))
            .collect();
        let realms: Vec<Realm> = owned
            .iter()
            .enumerate()
            .map(|(i, name)| realm(i as u32, &[name.as_str()]))
            .collect();

        let list = build(&realms, &query("", "gear"), prefs(), None);
        assert_eq!(list.realms.len(), MAX_SHOWN);

        let narrowed = build(&realms, &query("realm00", "gear"), prefs(), None);
        assert_eq!(
            narrowed.realms.len(),
            10,
            "a search that fits keeps every match"
        );
    }
}
