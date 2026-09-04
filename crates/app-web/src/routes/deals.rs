//! Evidence-gated cross-realm deals.

use std::collections::BTreeMap;

use app_core::Ports;
use app_core::market::deals;
use app_core::market::{Deal, ItemKind, RealmId};
use app_core::repo::{ReadModelRepository, RealmPriceRepository, Store};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{DealRow, DealsView, Layout, PanelHead};

#[derive(Template)]
#[template(path = "deals.html")]
struct DealsPage {
    layout: Layout,
    deals: DealsView,
}

/// `GET /wow/deals`
pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    csrf: Extension<Csrf>,
    prefs: Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    // One published cross-realm scan, shared with §9's row shape. The domain
    // selector owns every price and evidence rule below this line.
    let public = env.public_index();
    let rows: Vec<_> = env
        .store()
        .read_model()
        .deal_rollups(prefs.region)
        .await?
        .into_iter()
        // Apply the public catalogue gate before selecting both visible and
        // suppressed markets: a count must not leak a draft item either.
        .filter(|row| public.contains_key(&row.item))
        .collect();
    let realms: BTreeMap<RealmId, String> = env
        .store()
        .realm_prices()
        .realms()
        .await?
        .into_iter()
        .filter(|realm| realm.region == prefs.region)
        .map(|realm| (realm.id, realm.name))
        .collect();
    let now = env.now();
    let selection = visible_deals(&rows);
    let mut visible: Vec<DealRow> = selection
        .deals
        .into_iter()
        // The public index is the catalogue gate: stale rows never reveal a
        // draft item's name just because the read model still has history.
        .filter_map(|deal| {
            let (_catalog, entry) = public.get(&deal.item)?;
            (entry.kind == deal.kind).then(|| DealRow {
                name: entry.display_name(deal.item),
                kind: deal.kind.label(),
                realm: realms
                    .get(&deal.realm)
                    .cloned()
                    .unwrap_or_else(|| format!("Realm {}", deal.realm.get())),
                price: deal.price.to_string(),
                threshold: deal.threshold.to_string(),
                saving_percent: deal.saving_percent,
                coverage: format!(
                    "{} of {} realms listing",
                    deal.realms_listing, deal.realms_collected
                ),
                href: deal_href(&deal),
            })
        })
        .collect();
    visible.sort_by(|a, b| {
        b.saving_percent
            .cmp(&a.saving_percent)
            .then_with(|| a.price.cmp(&b.price))
            .then_with(|| a.name.cmp(&b.name))
    });
    let observed = rows.iter().filter_map(|row| row.observed_at).max();
    let count = visible.len();
    let user = current_user(&env, &headers).await?;

    page(
        &DealsPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Deals",
                "/wow/auctions",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            deals: DealsView {
                panel: PanelHead {
                    question: "Which cross-realm listings are genuinely cheap?",
                    window: "published history and latest realm snapshots".into(),
                    units: "gold per item",
                    coverage: Some(format!("{count} deals after evidence gates")),
                    freshness: observed.map(|at| crate::format::ago(prefs.locale, now.since(at))),
                },
                rows: visible,
                suppressed: selection.suppressed,
            },
        },
        prefs.locale,
    )
}

fn visible_deals(rows: &[app_core::market::MarketRollup]) -> app_core::market::DealSelection {
    deals::select(rows)
}

fn deal_href(deal: &Deal) -> String {
    match (deal.kind, deal.track) {
        (ItemKind::Recipe, _) => format!("/wow/arbitrage/{}", deal.item),
        (_, Some(track)) => format!("/wow/arbitrage/{}/{}", deal.item, track.slug()),
        // An unclassified historic BoE has no safe track URL. Its item page
        // still gives the reader a public, catalogue-gated landing point.
        _ => format!("/wow/item/{}", deal.item),
    }
}

#[cfg(test)]
mod tests {
    use app_core::WebConfig;
    use app_core::locale::Locale;
    use app_core::market::engine::{Anomaly, Distribution, Position};
    use app_core::market::{Copper, ItemId, MarketRollup, Region, Scope, Track, Window};
    use axum::http::Uri;

    use super::*;

    fn healthy() -> Position {
        Position {
            rank: Some(50),
            valuation: None,
            insufficient: None,
            from_median_percent: None,
            anomaly: Anomaly::Ordinary,
        }
    }

    fn row(scope: Scope, price: Option<Copper>, listings: u32) -> MarketRollup {
        let mut row = MarketRollup::empty(
            Region::Eu,
            ItemId(271_438),
            ItemKind::Boe,
            Some(Track::Champion),
        );
        row.scope = scope;
        row.window = Window::Days(30);
        row.position = Some(healthy());
        row.distribution = Some(Distribution {
            p05: Copper(1_500_000),
            p25: Copper(2_000_000),
            median: Copper(3_000_000),
            p75: Copper(4_000_000),
            p95: Copper(5_000_000),
            iqr: Copper(2_000_000),
            mad: Copper(1_000_000),
            buckets: 72,
        });
        row.realms_listing = 3;
        row.realms_collected = 3;
        row.cheapest_now = price;
        row.listings_now = listings;
        row
    }

    fn market(price: Option<Copper>, listings: u32) -> Vec<MarketRollup> {
        vec![
            row(Scope::Region, price, listings),
            row(Scope::Realm(RealmId(1403)), price, listings),
            row(Scope::Realm(RealmId(1404)), Some(Copper(3_000_000)), 1),
            row(Scope::Realm(RealmId(1405)), Some(Copper(3_200_000)), 1),
        ]
    }

    fn rendered_empty(suppressed: usize) -> String {
        DealsPage {
            layout: Layout::new(
                &WebConfig::default(),
                Locale::EnUs,
                "Deals",
                "/wow/auctions",
                &Uri::from_static("/wow/deals"),
                None,
                String::new(),
            ),
            deals: DealsView {
                panel: PanelHead {
                    question: "Which cross-realm listings are genuinely cheap?",
                    window: "published history and latest realm snapshots".into(),
                    units: "gold per item",
                    coverage: Some("0 deals after evidence gates".into()),
                    freshness: None,
                },
                rows: Vec::new(),
                suppressed,
            },
        }
        .render()
        .expect("deals template renders")
    }

    #[test]
    fn the_route_surfaces_a_genuine_deal() {
        let deals = visible_deals(&market(Some(Copper(2_000_000)), 1));

        assert_eq!(deals.deals.len(), 1);
        assert_eq!(deals.deals[0].price, Copper(2_000_000));
    }

    #[test]
    fn the_route_excludes_a_thin_market() {
        let mut rows = market(Some(Copper(2_000_000)), 1);
        rows[0].realms_listing = 1;
        rows[0].realms_collected = 3;

        assert!(visible_deals(&rows).deals.is_empty());
    }

    #[test]
    fn the_route_never_ranks_a_zero_listing_market() {
        assert!(
            visible_deals(&market(Some(Copper::ZERO), 0))
                .deals
                .is_empty()
        );
    }

    #[test]
    fn suppressed_candidates_do_not_render_as_no_deals() {
        let mut thin = market(Some(Copper(2_000_000)), 1);
        thin[0].realms_listing = 1;
        thin[0].realms_collected = 3;
        let suppressed = visible_deals(&thin);

        assert!(suppressed.deals.is_empty());
        assert_eq!(suppressed.suppressed, 1);
        let withheld = rendered_empty(suppressed.suppressed);
        let no_deals = rendered_empty(0);
        assert!(
            withheld.contains("1 market had candidate prices but too little evidence to rank.")
        );
        assert!(!withheld.contains("No current listings clear the deal threshold"));
        assert!(no_deals.contains("No current listings clear the deal threshold"));
    }
}
