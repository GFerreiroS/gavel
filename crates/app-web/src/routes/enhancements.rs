//! Gear enhancements: enchants and cut gems.
//!
//! Two categories, one module, because they are the same page twice over --
//! a grid of market cards for a set of items that is *generated* rather than
//! curated (see `scripts/catalog-sync.py`). Splitting them into two route
//! modules would be two copies of the same twenty lines, drifting apart the
//! first time either page grew a column.
//!
//! What differs is only the grouping: enchants divide by the equipment slot
//! they apply to, which is the choice a buyer is actually making. Gems do
//! not divide at all -- there are sixteen and a heading per stat would be
//! taller than the list beneath it.

use app_core::Ports;
use app_core::market::{ALL_SLOTS, Catalog, CatalogItem, ItemKind};
use app_core::repo::{PriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Query, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{CardGroup, EnhancementsView, ItemCard, Layout};

use super::reagents::SearchParams;

#[derive(Template)]
#[template(path = "enhancements.html")]
struct EnhancementsPage {
    layout: Layout,
    view: EnhancementsView,
}

#[derive(Template)]
#[template(path = "partials/enhancements.html")]
pub struct EnhancementsFragment {
    pub view: EnhancementsView,
}

/// `GET /wow/auctions/enchants`
pub async fn enchants_page<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    uri: OriginalUri,
    params: Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(ItemKind::Enchant, state, csrf, prefs, uri, params, headers).await
}

/// `GET /wow/auctions/gems`
pub async fn gems_page<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    uri: OriginalUri,
    params: Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(ItemKind::Gem, state, csrf, prefs, uri, params, headers).await
}

/// `GET /partials/enchants` -- the grid alone, for the search box.
pub async fn enchants_fragment<E: Ports>(
    state: State<E>,
    prefs: Extension<MarketPrefs>,
    params: Query<SearchParams>,
) -> WebResult<Html<String>> {
    fragment(ItemKind::Enchant, state, prefs, params).await
}

/// `GET /partials/gems`
pub async fn gems_fragment<E: Ports>(
    state: State<E>,
    prefs: Extension<MarketPrefs>,
    params: Query<SearchParams>,
) -> WebResult<Html<String>> {
    fragment(ItemKind::Gem, state, prefs, params).await
}

async fn render<E: Ports>(
    kind: ItemKind,
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    Query(params): Query<SearchParams>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let view = build(
        &env,
        kind,
        prefs,
        params.q.as_deref(),
        params.expansion.as_deref(),
        super::gear::Detail::Shell,
    )
    .await?;
    let user = current_user(&env, &headers).await?;
    page(
        &EnhancementsPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                view.title,
                "/wow/auctions",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            view,
        },
        prefs.locale,
    )
}

async fn fragment<E: Ports>(
    kind: ItemKind,
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Query(params): Query<SearchParams>,
) -> WebResult<Html<String>> {
    let view = build(
        &env,
        kind,
        prefs,
        params.q.as_deref(),
        params.expansion.as_deref(),
        super::gear::Detail::Full,
    )
    .await?;

    if let Some(wanted) = params.group.as_deref() {
        let baseline_days = view.baseline_days;
        let Some(group) = crate::groups::only(view.groups, wanted) else {
            return Err(app_core::AppError::NotFound.into());
        };
        return page(
            &crate::groups::CardGroupFragment {
                group,
                baseline_days,
                note: String::new(),
                heading_id: "slot",
            },
            prefs.locale,
        );
    }

    page(&EnhancementsFragment { view }, prefs.locale)
}

/// Everything both pages show. `kind` decides the wording and the grouping;
/// every figure below it is computed exactly as the other market pages
/// compute theirs.
async fn build<E: Ports>(
    env: &E,
    kind: ItemKind,
    prefs: MarketPrefs,
    query: Option<&str>,
    expansion: Option<&str>,
    detail: super::gear::Detail,
) -> WebResult<EnhancementsView> {
    let catalog = match expansion.filter(|id| !id.is_empty()) {
        Some(id) => env.public_catalog(id),
        None => env.active_catalog(),
    };
    let Some(catalog) = catalog else {
        return Err(app_core::AppError::NotFound.into());
    };
    let region = prefs.region;
    let prices = env.store().prices();
    let now = env.now();

    // The shell asks for no prices: it is the title, the slot links and the
    // search box, and none of those need a market.
    let shell = detail == super::gear::Detail::Shell;
    // Three sets of already-reduced rows, or nothing at all for the shell.
    // Before Phase 2 this was three reductions over the region's archive,
    // inside the request that was trying to paint.
    let page = if shell {
        Default::default()
    } else {
        crate::read_model::commodity_page(env, region, prefs.baseline_days).await?
    };
    let (latest, recent, all_time) = (page.current, page.recent, page.all_time);
    let tooltips = if shell {
        Default::default()
    } else {
        super::tooltip::cached_all(env, prefs, catalog, now).await
    };

    let needle = super::reagents::normalise(query);
    let total = catalog.of_kind(kind).count();
    let mut matched = 0;
    let mut groups = Vec::new();
    for (anchor, label, entries) in sections(catalog, kind) {
        if shell {
            break;
        }
        let mut cards: Vec<ItemCard> = entries
            .into_iter()
            .map(|entry| crate::cards::card(entry, &latest, &recent, &all_time, &tooltips))
            .filter(|card| super::reagents::matches(card, &needle))
            .collect();
        if cards.is_empty() {
            continue;
        }
        // Gems are read as a grid rather than as a list: in catalog order
        // each stat family is a row -- Deadly (crit), then Masterful, Quick,
        // Versatile -- and the four stones line up in colour columns down the
        // page. Any other order breaks that, so this one sorts on the
        // catalog's English names, which is what encodes the family. The
        // localised name would rearrange the rows in every other language.
        cards.sort_by(|a, b| match kind {
            ItemKind::Gem => a.sort_name.cmp(&b.sort_name),
            _ => crate::cards::by_rarity(a, b),
        });
        matched += cards.len();
        groups.push(CardGroup {
            // Filled in by `groups::defer` once the page's size is known.
            deferred: false,
            href: String::new(),
            // `audience` is the anchor id on a grouped page and unused on a
            // flat one; the field is named for the consumables page, which
            // was the first thing to group cards.
            audience: anchor,
            label,
            cards,
        });
    }

    // Enchants are 57 cards across a dozen slots and gems are 16 in one grid,
    // so the threshold decides rather than the page: the gems page is under it
    // and is one response, the enchants page is over it and is not.
    let fragment = |group: &str| {
        format!(
            "{}?expansion={}&group={}",
            Text::of(kind).fragment_path,
            super::gear::query_value(&catalog.id),
            super::gear::query_value(group),
        )
    };
    // Only where the page has sections to defer to: gems are one flat grid,
    // and a grid cannot arrive in halves.
    if matches!(kind, ItemKind::Enchant) {
        crate::groups::defer(&mut groups, needle.is_some(), fragment);
    }

    let text = Text::of(kind);
    Ok(EnhancementsView {
        fragment_href: format!(
            "{}?expansion={}&q={}",
            Text::of(kind).fragment_path,
            super::gear::query_value(&catalog.id),
            super::gear::query_value(needle.as_deref().unwrap_or_default()),
        ),
        title: text.title,
        blurb: text.blurb,
        counted: text.counted,
        matched_of: text.matched_of,
        path: text.path,
        fragment_path: text.fragment_path,
        grouped: matches!(kind, ItemKind::Enchant),
        expansion: catalog.expansion.clone(),
        expansion_id: catalog.id.clone(),
        archived: !env.catalog_state(catalog).is_collected(),
        query: needle.unwrap_or_default(),
        total,
        matched,
        observed: super::market::observed(prefs, now, prices.last_observed(region).await?),
        baseline_days: prefs.baseline_days,
        groups,
    })
}

/// The page's cards in the order they are shown, as (anchor, heading, cards).
///
/// One nameless section is how an ungrouped page is expressed, rather than a
/// second code path that skips the loop.
type Section<'a> = (&'static str, &'static str, Vec<&'a CatalogItem>);

fn sections(catalog: &Catalog, kind: ItemKind) -> Vec<Section<'_>> {
    match kind {
        ItemKind::Enchant => ALL_SLOTS
            .into_iter()
            .map(|slot| (slot.as_str(), slot.label(), catalog.by_slot(slot).collect()))
            .collect(),
        _ => vec![("all", "", catalog.of_kind(kind).collect())],
    }
}

/// The wording that differs between the two pages.
///
/// Source strings, translated by the template: an English sentence is the
/// msgid, so these are extracted by `scripts/i18n-extract.py` from the list in
/// [`crate::i18n::EXTERNAL_STRINGS`] like every other label that lives in Rust.
pub(crate) struct Text {
    pub title: &'static str,
    pub blurb: &'static str,
    pub counted: &'static str,
    pub matched_of: &'static str,
    pub path: &'static str,
    pub fragment_path: &'static str,
}

impl Text {
    pub(crate) const ENCHANTS: Text = Text {
        title: "Enchants",
        blurb: "Every enchantment sold on the auction house in {}, by the slot it applies to. \
                Prices are per scroll; each quality rank is its own market.",
        counted: "{} enchants tracked.",
        matched_of: "{} of {} enchants match.",
        path: "/wow/auctions/enchants",
        fragment_path: "/partials/enchants",
    };

    pub(crate) const GEMS: Text = Text {
        title: "Gems",
        blurb: "Every rare-quality cut gem of {}. Uncommon cuts and the handful of epic gems \
                are not tracked; each quality rank is its own market.",
        counted: "{} gems tracked.",
        matched_of: "{} of {} gems match.",
        path: "/wow/auctions/gems",
        fragment_path: "/partials/gems",
    };

    const fn of(kind: ItemKind) -> Text {
        match kind {
            ItemKind::Gem => Text::GEMS,
            _ => Text::ENCHANTS,
        }
    }
}
