//! Auction-house consumable tracker.
//!
//! One page per expansion. The active one shows live prices and alerts; an
//! archived one is the same page with the collection stopped -- history kept,
//! never added to.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::materialise::MarketWindow;
use app_core::market::window::Window;
use app_core::market::{ALL_AUDIENCES_LABELS, Catalog, CommodityProvider, ItemId, ItemKind};
use app_core::repo::{ReadModelRepository, Store};
use app_core::timing::{self, Stage};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, Redirect};
use cluster_core::Millis;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::format;
use crate::i18n::filters;
use crate::prefs::BASELINE_CHOICES;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{
    AuctionCategory, AuctionsView, BaselineOption, CardGroup, CatalogLink, Layout, MarketPicker,
    MarketView, PatchCell, PatchColumn, PatchRow, PatchesView,
};

/// The window the "vs usual" figure compares against is a visitor preference,

#[derive(Template)]
#[template(path = "auctions.html")]
struct AuctionsPage {
    layout: Layout,
    auctions: AuctionsView,
}

/// `GET /wow/auctions` -> the index of tracking categories.
///
/// One tab, many categories. A new category is a row here plus its own page,
/// never another entry in the nav bar.
pub async fn index<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<ConsumablesQuery>,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let id = query.expansion.filter(|id| !id.is_empty());
    let Some(catalog) = select(&env, id.as_deref()) else {
        return Err(app_core::AppError::NotFound.into());
    };
    let region = prefs.region;

    let count = |kind: ItemKind| catalog.of_kind(kind).count();
    let live = env.catalog_state(catalog).is_collected();

    // Carry the choice into the category pages, so picking here once is the
    // whole of the choice. Region rides along in the cookie the prefs
    // middleware writes, so only the expansion has to travel in the link.
    let onward = |path: &str| format!("{path}?expansion={}", catalog.id);
    let categories = vec![
        AuctionCategory {
            href: onward("/wow/consumables"),
            name: "Consumables",
            summary: "Flasks, potions, food and runes -- what a raid night costs.",
            scope: "Region-wide market",
            tracked_items: count(ItemKind::Consumable),
            live,
            icon: "flask",
        },
        AuctionCategory {
            href: onward("/wow/auctions/reagents"),
            name: "Reagents",
            summary: "Every crafting material of the current expansion, by profession.",
            scope: "Region-wide market",
            tracked_items: count(ItemKind::Reagent),
            live,
            icon: "ore",
        },
        AuctionCategory {
            href: onward("/wow/auctions/enchants"),
            name: "Enchants",
            summary: "Every enchantment on the auction house, by the slot it applies to.",
            scope: "Region-wide market",
            tracked_items: count(ItemKind::Enchant),
            live,
            icon: "sparkles",
        },
        AuctionCategory {
            href: onward("/wow/auctions/gems"),
            name: "Gems",
            summary: "The rare-quality cuts -- what a raider actually sockets.",
            scope: "Region-wide market",
            tracked_items: count(ItemKind::Gem),
            live,
            icon: "gems",
        },
        AuctionCategory {
            href: onward("/wow/auctions/gear"),
            name: "Bind-on-equip gear",
            summary: "Raid BoEs, with a price on every realm and an upgrade ladder.",
            // The one category the expansion-and-region picker above does not
            // govern: gear has its own realm choice, on its own page.
            scope: "Per connected realm",
            tracked_items: count(ItemKind::Boe),
            live,
            icon: "armor",
        },
        AuctionCategory {
            href: onward("/wow/auctions/recipes"),
            name: "Recipes",
            summary: "Every recipe trading this expansion, by the profession that reads it.",
            scope: "Per connected realm",
            tracked_items: count(ItemKind::Recipe),
            live,
            icon: "scroll",
        },
    ];

    // The same three figures the category pages show, for the whole expansion
    // rather than one category of it. One query, from the published version:
    // the index is a shell and has to paint before anything else arrives.
    let (markets, last_observed) = env.store().read_model().commodity_summary(region).await?;
    let token = super::wow_token::summary(&env, prefs).await?;
    let samples_held = markets as usize;
    let now = env.now();

    let auctions = AuctionsView {
        picker: MarketPicker::new("/wow/auctions".to_string(), &env.market().regions, region)
            .with_expansions(
                // Public order, so a `draft_ptr` catalogue is not in the
                // picker. §8: administrator-only, and it lists no prices.
                env.public_catalogs()
                    .into_iter()
                    .map(|c| CatalogLink {
                        id: c.id.clone(),
                        label: c.expansion.clone(),
                        collecting: env.catalog_state(c).is_collected(),
                        selected: c.id == catalog.id,
                    })
                    .collect(),
            ),
        expansion: catalog.expansion.clone(),
        region: region.to_string().to_uppercase(),
        archived: !env.catalog_state(catalog).is_collected(),
        tracked_items: catalog.tracked_ids().len(),
        samples_held,
        last_observed: observed(prefs, now, last_observed),
        baseline_days: prefs.baseline_days,
        baselines: BASELINE_CHOICES
            .into_iter()
            .map(|(days, label)| BaselineOption {
                days,
                label,
                selected: days == prefs.baseline_days,
            })
            .collect(),
        token,
        categories,
    };

    page(
        &AuctionsPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Auction House",
                "/wow/auctions",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            auctions,
        },
        prefs.locale,
    )
}

#[derive(Template)]
#[template(path = "consumables.html")]
struct ConsumablesPage {
    layout: Layout,
    market: MarketView,
}

#[derive(Template)]
#[template(path = "partials/consumables.html")]
pub struct ConsumablesFragment {
    pub market: MarketView,
}

#[derive(Template)]
#[template(path = "partials/patches.html")]
pub struct PatchesFragment {
    pub patches: PatchesView,
}

/// What the picker form submits. Both fields are optional so the bare URL
/// still means "the expansion currently being collected, in my usual region".
#[derive(Debug, Default, serde::Deserialize)]
pub struct ConsumablesQuery {
    /// A catalog id. `region` is absent here on purpose: it is resolved for
    /// every page by the `MarketPrefs` middleware, which also remembers it.
    expansion: Option<String>,
}

pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<ConsumablesQuery>,
) -> WebResult<Html<String>> {
    // An empty `expansion=` -- which is what a select with no choice submits
    // -- means "whatever is live", not "a catalog with an empty id".
    let id = query.expansion.filter(|id| !id.is_empty());
    render_page(env, csrf, prefs, headers, &uri, id).await
}

/// Compatibility for old expansion bookmarks. An expansion is not itself a
/// category, so the neutral destination is the Auction House index.
pub async fn archived_page<E: Ports>(
    State(env): State<E>,
    Path(id): Path<String>,
) -> WebResult<Redirect> {
    if env.public_catalog(&id).is_none() {
        return Err(app_core::AppError::NotFound.into());
    }
    Ok(Redirect::permanent(&format!(
        "/wow/auctions?expansion={id}"
    )))
}

async fn render_page<E: Ports>(
    env: E,
    csrf: Csrf,
    prefs: MarketPrefs,
    headers: HeaderMap,
    uri: &axum::http::Uri,
    id: Option<String>,
) -> WebResult<Html<String>> {
    let market = build(&env, prefs, id.as_deref(), super::gear::Detail::Shell).await?;
    let user = current_user(&env, &headers).await?;
    page(
        &ConsumablesPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Consumables",
                "/wow/auctions",
                uri,
                user.as_ref(),
                csrf.masked(),
            ),
            market,
        },
        prefs.locale,
    )
}

/// What the patch table is asked for.
///
/// `patch` narrows it to one column, which is what the archive's patch page
/// fetches. A second table would have been the obvious way to give that page
/// prices, and it would have been the fork §16's Phase 9 forbids -- so this
/// one grew a filter instead.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PatchesQuery {
    expansion: Option<String>,
    patch: Option<String>,
}

/// `GET /partials/patches`
///
/// Fetched when the reader scrolls to it, and answers on its own for a reader
/// with scripting off.
pub async fn patches<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(cache): Extension<std::sync::Arc<crate::FragmentCache>>,
    Query(query): Query<PatchesQuery>,
    headers: HeaderMap,
) -> WebResult<axum::response::Response> {
    let id = query.expansion.filter(|id| !id.is_empty());
    let only = query.patch.filter(|p| !p.is_empty());
    let key = crate::fragment_cache::FragmentKey::new(
        "patches",
        env.store()
            .read_model()
            .published()
            .await?
            .map(|v| v.version),
        id.as_deref().unwrap_or(""),
        prefs.region.as_str(),
        // The patch table is not measured against a comparison window, so the
        // window is not part of what it says and must not split its cache.
        0,
        // Which patch, in the slot a category page's group occupies. Leaving
        // it out would serve one patch's column under another's name -- the
        // rule §11b states as "anything a cached fragment shows must be in
        // its key".
        only.as_deref().unwrap_or(""),
        prefs.locale.code(),
        None,
    );
    crate::fragment_cache::respond(&cache, &headers, key, async {
        Ok(page(
            &PatchesFragment {
                patches: build_patches(&env, prefs, id.as_deref(), only.as_deref()).await?,
            },
            prefs.locale,
        )?
        .0)
    })
    .await
}

pub async fn fragment<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    Extension(cache): Extension<std::sync::Arc<crate::FragmentCache>>,
    headers: HeaderMap,
) -> WebResult<axum::response::Response> {
    let key = crate::fragment_cache::FragmentKey::new(
        "consumables",
        env.store()
            .read_model()
            .published()
            .await?
            .map(|v| v.version),
        "",
        prefs.region.as_str(),
        prefs.baseline_days,
        "",
        prefs.locale.code(),
        None,
    );
    crate::fragment_cache::respond(&cache, &headers, key, async {
        Ok(page(
            &ConsumablesFragment {
                market: build(&env, prefs, None, super::gear::Detail::Full).await?,
            },
            prefs.locale,
        )?
        .0)
    })
    .await
}

/// Resolve which catalog the page is about: an explicit id, else the active
/// one, else the most recent archive so the page is never blank.
fn select<'a, E: Ports>(env: &'a E, id: Option<&str>) -> Option<&'a Catalog> {
    match id {
        // `public_catalog`, so a bookmarked or guessed PTR id is a 404 rather
        // than a page. A visitor must not be able to learn one exists.
        Some(id) => env.public_catalog(id),
        None => env
            .active_catalog()
            .or_else(|| env.public_catalogs().first().copied()),
    }
}

async fn build<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    id: Option<&str>,
    detail: super::gear::Detail,
) -> WebResult<MarketView> {
    let Some(catalog) = select(env, id) else {
        return Err(app_core::AppError::NotFound.into());
    };
    // One region per page: commodity markets share nothing, so mixing them in
    // a single table would be meaningless. Which one is the visitor's choice.
    let region = prefs.region;

    let now = env.now();

    // The shell asks for no prices: the heading, the archived notice and the
    // expansion wording are all that paint first.
    let shell = detail == super::gear::Detail::Shell;

    // Three sets of stored rows. Extremes are all-time, not windowed:
    // "cheapest ever, and when" only means something across the whole history,
    // and it is a row rather than a scan of one.
    let page = if shell {
        Default::default()
    } else {
        crate::read_model::commodity_page(env, region, prefs.baseline_days).await?
    };
    let (latest, recent) = (page.current, page.recent);
    // The page's own snapshot: what a card's freshness is news against.
    let ctx = crate::cards::CardContext {
        locale: prefs.locale,
        now,
        newest: latest.values().filter_map(|s| s.observed_at).max(),
    };

    // Tooltips that are already cached go straight into the page, so hovering
    // an icon costs no request at all (see `routes::tooltip`).
    let tooltips = if shell {
        Default::default()
    } else {
        super::tooltip::cached_all(env, prefs, catalog, now).await
    };

    // --- one card per market, grouped the way the raid is grouped ----------
    let mut groups: Vec<CardGroup> = Vec::new();
    // Charged to the analysis stage for the same reason the gear page's
    // grouping is: it is the read model being assembled inside a request.
    let cards_timing = timing::start(Stage::Analysis);
    for (audience, label) in ALL_AUDIENCES_LABELS {
        if shell {
            break;
        }
        let mut cards = Vec::new();
        for entry in catalog.by_audience(audience) {
            cards.push(crate::cards::card(entry, &latest, &recent, &tooltips, ctx));
        }
        // Category first -- flasks together, potions together -- and the
        // rarer item first within each, as everywhere else.
        cards.sort_by(|a, b| {
            a.category
                .cmp(b.category)
                .then_with(|| crate::cards::by_rarity(a, b))
        });
        groups.push(CardGroup {
            // Filled in by `groups::defer` once the page's size is known.
            deferred: false,
            href: String::new(),
            audience: audience.as_str(),
            label,
            cards,
        });
    }
    drop(cards_timing);

    Ok(MarketView {
        expansion: catalog.expansion.clone(),
        season: catalog.season_label(),
        archived: !env.catalog_state(catalog).is_collected(),
        configured: env.commodities().is_configured(),
        groups,
        // One snapshot priced every card on the page, so the age is the
        // page's rather than each card's.
        observed: observed(
            prefs,
            now,
            env.store().read_model().commodity_summary(region).await?.1,
        ),
        baseline_days: prefs.baseline_days,
    })
}

/// The expansion's price history, patch by patch.
///
/// Its own build and its own request, because it is 659 rows of every item at
/// every rank -- 85% of what the consumables fragment used to weigh -- and
/// most visits never scroll to it. One query per patch column plus one for the
/// expansion; all of them stored windows, none of them a reduction.
async fn build_patches<E: Ports>(
    env: &E,
    prefs: MarketPrefs,
    id: Option<&str>,
    only: Option<&str>,
) -> WebResult<PatchesView> {
    let Some(catalog) = select(env, id) else {
        return Err(app_core::AppError::NotFound.into());
    };
    // A patch this catalogue does not have is a 404, not an empty table. The
    // archive validates the expansion and then the patch inside it (§16), and
    // this is the same rule one layer down -- otherwise a guessed key would
    // render a table of dashes and look like an answer.
    if let Some(want) = only
        && !catalog.patches.iter().any(|p| p.patch == want)
    {
        return Err(app_core::AppError::NotFound.into());
    }
    let region = prefs.region;
    let now = env.now();
    let model = env.store().read_model();

    let mut columns = Vec::new();
    let mut per_patch: Vec<BTreeMap<ItemId, MarketWindow>> = Vec::new();
    for (patch, _from, _until) in catalog.patch_windows() {
        if only.is_some_and(|want| want != patch.patch) {
            continue;
        }
        columns.push(PatchColumn {
            patch: patch.patch.clone(),
            label: patch.label(),
            started: patch.started.clone(),
        });
        per_patch.push(windows_by_item(
            model
                .commodity_windows(region, &Window::Patch(patch.patch.clone()))
                .await?,
        ));
    }
    let overall = windows_by_item(model.commodity_windows(region, &Window::Expansion).await?);

    // Names come from the tooltip cache so a reader sees the same word here as
    // on the card above.
    let tooltips = super::tooltip::cached_all(env, prefs, catalog, now).await;

    let mut rows = Vec::new();
    for item in &catalog.items {
        for rank in &item.ranks {
            rows.push(PatchRow {
                name: crate::cards::display_name(&tooltips, item, rank.item_id),
                audience: item.audience.as_str(),
                category: item.category.label(),
                cells: per_patch
                    .iter()
                    .map(|stats| cell(stats.get(&rank.item_id)))
                    .collect(),
                overall: cell(overall.get(&rank.item_id)),
            });
        }
    }
    rows.sort_by(|a, b| a.category.cmp(b.category).then(a.name.cmp(&b.name)));
    // One patch on its own is a different question -- "what was listed while
    // 12.1 was live" -- and a row with nothing in that column is not an answer
    // to it. Across the whole expansion every row says something, so nothing
    // is dropped there.
    if only.is_some() {
        rows.retain(|row| row.cells.iter().any(|cell| cell.has_data));
    }

    Ok(PatchesView {
        expansion: catalog.expansion.clone(),
        only: only.map(str::to_string),
        patches: columns,
        rows,
    })
}

/// How long ago a snapshot was collected, or that none ever was.
pub(super) fn observed(prefs: MarketPrefs, now: Millis, at: Option<Millis>) -> String {
    match at {
        Some(at) => format::ago(prefs.locale, now.since(at)),
        None => "never".to_string(),
    }
}

fn windows_by_item(windows: Vec<MarketWindow>) -> BTreeMap<ItemId, MarketWindow> {
    windows.into_iter().map(|w| (w.key.item(), w)).collect()
}

fn cell(stats: Option<&MarketWindow>) -> PatchCell {
    match stats {
        Some(w) if w.samples > 0 => PatchCell {
            low: w.low.to_string(),
            mean: w.mean.to_string(),
            high: w.high.to_string(),
            samples: w.samples,
            has_data: true,
        },
        _ => PatchCell::empty(),
    }
}

#[cfg(test)]
mod tests {
    use askama::Template;
    use axum::http::Uri;

    use super::*;
    use crate::views::TokenSummaryView;

    fn token(has_price: bool) -> TokenSummaryView {
        TokenSummaryView {
            panel: crate::views::PanelHead {
                question: "What is the current WoW Token price for this region?",
                window: "latest collection".into(),
                units: "gold",
                coverage: Some(
                    if has_price {
                        "one regional token price"
                    } else {
                        "no token price collected"
                    }
                    .into(),
                ),
                freshness: Some(
                    if has_price {
                        "2m ago"
                    } else {
                        "not applicable"
                    }
                    .into(),
                ),
            },
            region: "EU".into(),
            has_price,
            current: if has_price {
                "275,000g".into()
            } else {
                String::new()
            },
            updated: if has_price {
                "2m ago"
            } else {
                "not applicable"
            }
            .into(),
            observations: usize::from(has_price),
            href: "/wow/token?region=eu".into(),
        }
    }

    fn page_with_token(has_price: bool) -> String {
        let uri: Uri = "/wow/auctions".parse().expect("valid URI");
        AuctionsPage {
            layout: Layout::new(
                &app_core::WebConfig::default(),
                app_core::locale::Locale::EnGb,
                "Auction House",
                "/wow/auctions",
                &uri,
                None,
                String::new(),
            ),
            auctions: AuctionsView {
                picker: MarketPicker::new(
                    "/wow/auctions".into(),
                    &[],
                    app_core::market::Region::Eu,
                ),
                expansion: "Midnight".into(),
                region: "EU".into(),
                archived: false,
                tracked_items: 0,
                samples_held: 0,
                last_observed: "never".into(),
                baseline_days: 7,
                baselines: Vec::new(),
                token: token(has_price),
                categories: Vec::new(),
            },
        }
        .render()
        .expect("auction-house template renders")
    }

    #[test]
    fn auctions_page_shows_the_token_panel_with_the_current_price() {
        let rendered = page_with_token(true);

        assert!(rendered.contains("What is the current WoW Token price for this region?"));
        assert!(rendered.contains("275,000g"));
        assert!(rendered.contains("/wow/token?region=eu"));
    }

    #[test]
    fn auctions_page_names_a_missing_token_price_without_inventing_zero() {
        let rendered = page_with_token(false);

        assert!(rendered.contains("No token price collected for this region yet."));
        assert!(!rendered.contains(">0g</span>"));
    }
}
