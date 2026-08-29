//! Crafting reagents: every tracked material of the current expansion,
//! grouped by the profession that makes or gathers it.
//!
//! Two things make this page different from consumables. It is an order of
//! magnitude bigger -- 223 entries against 26 -- so it adds a search box to
//! the shared card grid. Its grouping is professional
//! rather than by raid role, which is why `CatalogItem` carries a profession
//! instead of the page inferring one from the item's material type.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::{ALL_PROFESSIONS, ItemId, ItemKind, PriceSample, WindowStats};
use app_core::repo::{PriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Query, State};
use axum::http::HeaderMap;
use axum::response::Html;
use cluster_core::Millis;
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{CardGroup, ItemCard, Layout, ReagentsView};

/// Longest search term accepted. Anything longer is a mistake or an attack,
/// and matching it would still be a linear scan of the catalogue.
const MAX_QUERY: usize = 64;

#[derive(Debug, Default, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: Option<String>,
    /// Which expansion, when arriving from the Auction House index. Absent
    /// means the one currently being collected.
    #[serde(default)]
    pub expansion: Option<String>,
}

#[derive(Template)]
#[template(path = "reagents.html")]
struct ReagentsPage {
    layout: Layout,
    reagents: ReagentsView,
}

#[derive(Template)]
#[template(path = "partials/reagents.html")]
pub struct ReagentsFragment {
    pub reagents: ReagentsView,
}

/// `GET /wow/auctions/reagents`
pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    Query(params): Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let reagents = build(
        &env,
        prefs,
        params.q.as_deref(),
        params.expansion.as_deref(),
    )
    .await?;
    let user = current_user(&env, &headers).await?;
    page(
        &ReagentsPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Reagents",
                "/wow/auctions",
                &uri,
                user.map(|u| u.username),
                csrf.0.clone(),
            ),
            reagents,
        },
        prefs.locale,
    )
}

/// `GET /partials/reagents` -- the table alone, for the search box.
pub async fn fragment<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Query(params): Query<SearchParams>,
) -> WebResult<Html<String>> {
    page(
        &ReagentsFragment {
            reagents: build(
                &env,
                prefs,
                params.q.as_deref(),
                params.expansion.as_deref(),
            )
            .await?,
        },
        prefs.locale,
    )
}

async fn build<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    query: Option<&str>,
    expansion: Option<&str>,
) -> WebResult<ReagentsView> {
    // An explicit choice from the index, else whatever is being collected.
    // An empty value is what a select with no choice submits and means the
    // latter, not "a catalog with an empty id".
    let catalog = match expansion.filter(|id| !id.is_empty()) {
        Some(id) => env.catalogs().by_id(id),
        None => env.active_catalog(),
    };
    let Some(catalog) = catalog else {
        return Err(app_core::AppError::NotFound.into());
    };
    let region = prefs.region;
    let prices = env.store().prices();
    let now = env.now();

    let latest: BTreeMap<ItemId, PriceSample> = prices
        .latest(region)
        .await?
        .into_iter()
        .map(|s| (s.item, s))
        .collect();
    // The "vs usual" window is the visitor's, chosen once on the Auction
    // House index and applied to every category under it.
    let recent = index_stats(
        prices
            .window_stats(region, prefs.baseline_since(now), None)
            .await?,
    );
    // Extremes are all-time, as on the consumables cards: "cheapest ever, and
    // when" only means anything across the whole history.
    let all_time = index_stats(prices.window_stats(region, Millis::ZERO, None).await?);

    // Localised names come from the tooltip cache, so a search matches what
    // the visitor can actually see on the page.
    let tooltips = super::tooltip::cached_all(env, prefs, catalog, now).await;

    let needle = normalise(query);
    let total = catalog.of_kind(ItemKind::Reagent).count();
    let mut groups = Vec::new();
    let mut matched = 0;

    for profession in ALL_PROFESSIONS {
        let mut cards: Vec<ItemCard> = catalog
            .by_profession(profession)
            .map(|entry| crate::cards::card(entry, &latest, &recent, &all_time, &tooltips))
            .filter(|card| matches(card, &needle))
            .collect();
        if cards.is_empty() {
            continue;
        }
        cards.sort_by(crate::cards::by_rarity);
        matched += cards.len();
        groups.push(CardGroup {
            audience: profession.as_str(),
            label: profession.label(),
            cards,
        });
    }

    Ok(ReagentsView {
        expansion: catalog.expansion.clone(),
        expansion_id: catalog.id.clone(),
        archived: !catalog.is_active(),
        query: needle.clone().unwrap_or_default(),
        total,
        matched,
        observed: super::market::observed(prefs, now, prices.last_observed(region).await?),
        baseline_days: prefs.baseline_days,
        groups,
    })
}

fn index_stats(stats: Vec<WindowStats>) -> BTreeMap<ItemId, WindowStats> {
    stats.into_iter().map(|w| (w.item, w)).collect()
}

/// Trim and lower-case the search term, and treat an empty one as absent.
///
/// Shared with `routes::enhancements`: every card grid with a search box
/// filters the same way, and two of them disagreeing about whitespace or case
/// would be a bug nobody could see.
pub(super) fn normalise(query: Option<&str>) -> Option<String> {
    let value = query?.trim();
    if value.is_empty() {
        return None;
    }
    Some(
        value
            .chars()
            .take(MAX_QUERY)
            .collect::<String>()
            .to_lowercase(),
    )
}

pub(super) fn matches(card: &ItemCard, needle: &Option<String>) -> bool {
    match needle {
        None => true,
        Some(needle) => card.name.to_lowercase().contains(needle.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card_named(name: &str) -> ItemCard {
        ItemCard {
            name: name.to_string(),
            icon: None,
            material: None,
            tooltip_item_id: 0,
            tooltip: None,
            category: "Herbalism",
            stat: "none",
            rarity: 0,
            sort_name: name.to_string(),
            any_data: false,
            columns: Vec::new(),
        }
    }

    #[test]
    fn an_empty_search_is_no_search() {
        assert_eq!(normalise(None), None);
        assert_eq!(normalise(Some("   ")), None);
        assert!(matches(&card_named("Peridot"), &normalise(None)));
    }

    #[test]
    fn search_ignores_case_and_surrounding_space() {
        let needle = normalise(Some("  PERIdot "));
        assert_eq!(needle.as_deref(), Some("peridot"));
        assert!(matches(&card_named("Flawless Quick Peridot"), &needle));
        assert!(!matches(&card_named("Masterful Amethyst"), &needle));
    }

    #[test]
    fn search_matches_the_localised_name() {
        // Rows carry the name the visitor can see, so a Spanish search term
        // has to match the Spanish name rather than the catalogue's English.
        let needle = normalise(Some("tinta"));
        assert!(matches(&card_named("Tinta de la Marea Negra"), &needle));
    }

    #[test]
    fn a_long_search_term_is_truncated_rather_than_rejected() {
        let needle = normalise(Some(&"a".repeat(500)));
        assert_eq!(needle.map(|n| n.len()), Some(MAX_QUERY));
    }
}
