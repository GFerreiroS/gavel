//! Cross-realm prices for one non-commodity item.
//!
//! The read model already reduced the regional median and each realm's market.
//! This route only selects and formats those published rows; it never derives
//! a price, a spread, or a coverage claim during a request.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::materialise::{MarketRollup, Scope};
use app_core::market::{Copper, ItemId, ItemKind, RealmId, Track};
use app_core::repo::{ReadModelRepository, RealmPriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{ArbitrageRealmRow, ArbitrageView, Layout, PanelHead};

#[derive(Template)]
#[template(path = "arbitrage.html")]
struct ArbitragePage {
    layout: Layout,
    arbitrage: ArbitrageView,
}

pub async fn gear<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    uri: OriginalUri,
    Path((item_id, track)): Path<(u32, String)>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(
        state,
        csrf,
        prefs,
        uri,
        headers,
        ItemId(item_id),
        Track::parse(&track),
    )
    .await
}

pub async fn recipe<E: Ports>(
    state: State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    uri: OriginalUri,
    Path(item_id): Path<u32>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    render(state, csrf, prefs, uri, headers, ItemId(item_id), None).await
}

#[allow(clippy::too_many_arguments)]
async fn render<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    item: ItemId,
    track: Option<Track>,
) -> WebResult<Html<String>> {
    let Some((catalog, entry)) = env.public_item(item) else {
        return Err(app_core::AppError::NotFound.into());
    };
    if entry.kind.is_commodity() || (entry.kind == ItemKind::Recipe) != track.is_none() {
        return Err(app_core::AppError::NotFound.into());
    }

    let rows = env
        .store()
        .read_model()
        .item_rollups(prefs.region, item)
        .await?;
    let regional = rows
        .iter()
        .find(|row| row.track == track && row.scope == Scope::Region);
    let names: BTreeMap<RealmId, String> = env
        .store()
        .realm_prices()
        .realms()
        .await?
        .into_iter()
        .filter(|realm| realm.region == prefs.region)
        .map(|realm| (realm.id, realm.name))
        .collect();
    let now = env.now();
    let regional = regional
        .cloned()
        .unwrap_or_else(|| MarketRollup::empty(prefs.region, item, entry.kind, track));
    let coverage = format!(
        "{} of {} realms listing",
        regional.realms_listing, regional.realms_collected
    );
    let freshness = regional
        .observed_at
        .map(|at| crate::format::ago(prefs.locale, now.since(at)));
    let panel = |question| PanelHead {
        question,
        window: "latest snapshot on each collected realm".into(),
        units: "gold per item",
        coverage: Some(coverage.clone()),
        freshness: freshness.clone(),
    };
    let price = |value: Option<Copper>| value.map(|p| p.to_string()).unwrap_or_else(|| "—".into());
    let realm_name = |id: Option<RealmId>| {
        id.and_then(|id| names.get(&id))
            .cloned()
            .unwrap_or_else(|| "—".into())
    };
    let mut realms: Vec<ArbitrageRealmRow> = rows
        .iter()
        .filter(|row| row.track == track)
        .filter_map(|row| match row.scope {
            Scope::Region => None,
            Scope::Realm(id) => Some((id, row)),
        })
        .map(|(id, row)| {
            let insufficient = match row.position.and_then(|position| position.insufficient) {
                Some(app_core::market::Insufficient::NotEnoughHistory { .. }) | None => {
                    Some("Not enough history")
                }
                Some(app_core::market::Insufficient::TooManyGaps { .. }) => Some("Too many gaps"),
            };
            ArbitrageRealmRow {
                realm: names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("Realm {id}")),
                price: price(row.cheapest_now),
                listings: row.listings_now,
                observed: row
                    .observed_at
                    .map(|at| crate::format::ago(prefs.locale, now.since(at)))
                    .unwrap_or_else(|| "—".into()),
                insufficient,
            }
        })
        .collect();
    realms.sort_by(|a, b| a.realm.cmp(&b.realm));

    let user = current_user(&env, &headers).await?;
    page(
        &ArbitragePage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Realm arbitrage",
                "/wow/auctions",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            arbitrage: ArbitrageView {
                name: entry.display_name(item),
                track: track.map(Track::as_str).unwrap_or("").into(),
                section_href: format!(
                    "/wow/auctions/{}?expansion={}",
                    if entry.kind == ItemKind::Recipe {
                        "recipes"
                    } else {
                        "gear"
                    },
                    catalog.id
                ),
                summary_panel: panel("Where is this item cheapest right now?"),
                table_panel: panel("How does each realm compare?"),
                has_data: regional.cheapest_now.is_some(),
                cheapest: price(regional.cheapest_now),
                typical: price(regional.median_realm_now),
                dearest: price(regional.dearest_realm_now),
                cheapest_realm: realm_name(regional.cheapest_realm),
                dearest_realm: realm_name(regional.dearest_realm),
                realms,
            },
        },
        prefs.locale,
    )
}
