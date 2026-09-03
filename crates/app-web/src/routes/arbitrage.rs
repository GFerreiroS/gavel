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
            realm_view(
                row,
                names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("Realm {id}")),
                row.observed_at
                    .map(|at| crate::format::ago(prefs.locale, now.since(at)))
                    .unwrap_or_else(|| "—".into()),
            )
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

/// Turn one published realm roll-up into the exact row the template renders.
///
/// A missing position is not a healthy position: it means there is no
/// collected evidence for this row. Conversely, a position with no
/// insufficiency is explicitly healthy and must leave its current price
/// visible.
fn realm_view(row: &MarketRollup, realm: String, observed: String) -> ArbitrageRealmRow {
    let insufficient = match row.position {
        None => Some("No observations yet."),
        Some(position) => match position.insufficient {
            Some(app_core::market::Insufficient::NotEnoughHistory { .. }) => {
                Some("Not enough history")
            }
            Some(app_core::market::Insufficient::TooManyGaps { .. }) => Some("Too many gaps"),
            None => None,
        },
    };

    ArbitrageRealmRow {
        realm,
        // A snapshot with no listings has no current price. Guard against a
        // malformed stored zero here rather than rendering a free item.
        price: (row.listings_now > 0)
            .then_some(row.cheapest_now)
            .flatten()
            .map(|price| price.to_string())
            .unwrap_or_else(|| "—".into()),
        listings: row.listings_now,
        observed,
        insufficient,
    }
}

#[cfg(test)]
mod tests {
    use app_core::WebConfig;
    use app_core::locale::Locale;
    use app_core::market::engine::{Anomaly, Insufficient, Position};
    use axum::http::Uri;

    use super::*;

    fn healthy_position() -> Position {
        Position {
            rank: Some(50),
            valuation: None,
            insufficient: None,
            from_median_percent: Some(0),
            anomaly: Anomaly::Ordinary,
        }
    }

    fn rollup(position: Option<Position>, listings: u32, price: Option<Copper>) -> MarketRollup {
        let mut row = MarketRollup::empty(
            app_core::market::Region::Eu,
            ItemId(271_438),
            ItemKind::Boe,
            Some(Track::Champion),
        );
        row.scope = Scope::Realm(RealmId(1403));
        row.position = position;
        row.listings_now = listings;
        row.cheapest_now = price;
        row
    }

    fn panel() -> PanelHead {
        PanelHead {
            question: "How does each realm compare?",
            window: "latest snapshot".into(),
            units: "gold per item",
            coverage: Some("1 of 1 realms listing".into()),
            freshness: Some("just now".into()),
        }
    }

    fn rendered_realm(row: MarketRollup) -> String {
        let page = ArbitragePage {
            layout: Layout::new(
                &WebConfig::default(),
                Locale::EnUs,
                "Realm arbitrage",
                "/wow/auctions",
                &Uri::from_static("/wow/arbitrage/271438/champion"),
                None,
                String::new(),
            ),
            arbitrage: ArbitrageView {
                name: "Test item".into(),
                track: "Champion".into(),
                section_href: "/wow/auctions/gear".into(),
                summary_panel: panel(),
                table_panel: panel(),
                has_data: false,
                cheapest: "—".into(),
                typical: "—".into(),
                dearest: "—".into(),
                cheapest_realm: "—".into(),
                dearest_realm: "—".into(),
                realms: vec![realm_view(&row, "Test realm".into(), "just now".into())],
            },
        };
        page.render().expect("arbitrage template renders")
    }

    #[test]
    fn a_healthy_realm_renders_its_price() {
        let html = rendered_realm(rollup(Some(healthy_position()), 1, Some(Copper(123_456))));

        assert!(html.contains("<td class=\"number\">12g 34s</td>"));
        assert!(!html.contains("No observations yet."));
    }

    #[test]
    fn a_realm_without_enough_history_withholds_its_price() {
        let html = rendered_realm(rollup(
            Some(Position {
                insufficient: Some(Insufficient::NotEnoughHistory { have: 4, need: 72 }),
                ..healthy_position()
            }),
            1,
            Some(Copper(123_456)),
        ));

        assert!(html.contains("Not enough history"));
        assert!(!html.contains("12g 34s"));
    }

    #[test]
    fn a_realm_with_too_many_gaps_withholds_its_price() {
        let html = rendered_realm(rollup(
            Some(Position {
                insufficient: Some(Insufficient::TooManyGaps {
                    coverage: 20,
                    need: 80,
                }),
                ..healthy_position()
            }),
            1,
            Some(Copper(123_456)),
        ));

        assert!(html.contains("Too many gaps"));
        assert!(!html.contains("12g 34s"));
    }

    #[test]
    fn an_unobserved_realm_withholds_its_price() {
        let html = rendered_realm(rollup(None, 1, Some(Copper(123_456))));

        assert!(html.contains("No observations yet."));
        assert!(!html.contains("12g 34s"));
    }

    #[test]
    fn a_realm_with_zero_listings_never_looks_free() {
        let html = rendered_realm(rollup(Some(healthy_position()), 0, Some(Copper::ZERO)));

        assert!(html.contains("<td class=\"number\">—</td>"));
        assert!(!html.contains(">0c<"));
    }
}
