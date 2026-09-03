//! Evidence-gated cross-realm buying opportunities.
//!
//! A deal is not a new market statistic. The published regional roll-up
//! already carries the history and its evidence gate; this module only applies
//! roadmap §10's conservative buying rule to those stored answers.

use std::collections::BTreeMap;

use super::{Copper, ItemId, ItemKind, MarketRollup, RealmId, Scope, Track};

/// Roadmap §10: below this, a cheap item is noise rather than an opportunity.
pub const MIN_DEAL_PRICE: Copper = Copper(1_500_000);
/// Do not make a cross-realm recommendation from one or two available realms.
pub const MIN_LISTING_REALMS: u32 = 3;
/// A current price must cover at least half of the realms that supplied history.
pub const MIN_LISTING_COVERAGE_PERCENT: u32 = 50;
/// Roadmap §10's gate before the current-offer percentile is meaningful.
pub const PERCENTILE_REALMS: usize = 15;

/// One item/track whose cheapest currently purchasable realm clears the deal
/// threshold. There is one result per market, not one per auction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deal {
    pub item: ItemId,
    pub kind: ItemKind,
    pub track: Option<Track>,
    pub realm: RealmId,
    pub price: Copper,
    pub threshold: Copper,
    pub saving_percent: u8,
    pub realms_listing: u32,
    pub realms_collected: u32,
}

/// Select the published markets that are safe enough to call deals.
///
/// The regional row supplies the historical median and its evidence. Realm
/// rows supply the currently purchasable prices for §10's 15-realm percentile
/// rule. No price history is reduced here.
pub fn find(rows: &[MarketRollup]) -> Vec<Deal> {
    let mut grouped: BTreeMap<(ItemId, ItemKind, Option<Track>), Vec<&MarketRollup>> =
        BTreeMap::new();
    for row in rows {
        if !row.kind.is_commodity() {
            grouped
                .entry((row.item, row.kind, row.track))
                .or_default()
                .push(row);
        }
    }

    let mut deals: Vec<Deal> = grouped.into_values().filter_map(deal).collect();
    deals.sort_by(|a, b| {
        b.saving_percent
            .cmp(&a.saving_percent)
            .then_with(|| a.price.cmp(&b.price))
            .then_with(|| a.item.cmp(&b.item))
            .then_with(|| a.track.cmp(&b.track))
    });
    deals
}

fn deal(rows: Vec<&MarketRollup>) -> Option<Deal> {
    let regional = rows
        .iter()
        .copied()
        .find(|row| row.scope == Scope::Region)?;
    let historical_median = regional.distribution?.median;

    // An absent position means no observations; an insufficiency means the
    // stored historical shape refused to make a claim. Neither supports one.
    if regional.position?.insufficient.is_some()
        || regional.realms_listing < MIN_LISTING_REALMS
        || regional.realms_collected == 0
        || regional.realms_listing.saturating_mul(100)
            < regional
                .realms_collected
                .saturating_mul(MIN_LISTING_COVERAGE_PERCENT)
    {
        return None;
    }

    let mut offered: Vec<(Copper, RealmId)> = rows
        .iter()
        .filter_map(|row| match row.scope {
            Scope::Region => None,
            Scope::Realm(realm) => {
                // §9's evidence rule applies to the purchase realm too. A
                // zero-listing snapshot has no price, even if a malformed row
                // carries a numeric value.
                (row.position
                    .is_some_and(|position| position.insufficient.is_none())
                    && row.listings_now > 0)
                    .then_some(row.cheapest_now)
                    .flatten()
                    .map(|price| (price, realm))
            }
        })
        .collect();
    offered.sort_unstable_by_key(|(price, realm)| (*price, *realm));
    let &(price, realm) = offered.first()?;

    // Exact port of §10: historic median normally, then the lower of that and
    // the current offered third once at least fifteen realms make that
    // percentile a real shape rather than a single unusual listing.
    let threshold = if offered.len() >= PERCENTILE_REALMS {
        historical_median.min(offered[offered.len() / 3].0)
    } else {
        historical_median
    };
    if price < MIN_DEAL_PRICE || price >= threshold {
        return None;
    }

    Some(Deal {
        item: regional.item,
        kind: regional.kind,
        track: regional.track,
        realm,
        price,
        threshold,
        saving_percent: (((threshold.get() - price.get()) * 100) / threshold.get()) as u8,
        realms_listing: regional.realms_listing,
        realms_collected: regional.realms_collected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::engine::{Anomaly, Insufficient, Position};
    use crate::market::{Distribution, Window};

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
            super::super::Region::Eu,
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

    #[test]
    fn a_genuine_deal_is_ranked() {
        let deals = find(&market(Some(Copper(2_000_000)), 1));

        assert_eq!(deals.len(), 1);
        assert_eq!(deals[0].realm, RealmId(1403));
        assert_eq!(deals[0].price, Copper(2_000_000));
        assert_eq!(deals[0].threshold, Copper(3_000_000));
    }

    #[test]
    fn thin_realm_coverage_is_excluded() {
        let mut rows = market(Some(Copper(2_000_000)), 1);
        rows[0].realms_listing = 1;
        rows[0].realms_collected = 3;

        assert!(find(&rows).is_empty());
    }

    #[test]
    fn an_insufficient_historical_market_is_excluded() {
        let mut rows = market(Some(Copper(2_000_000)), 1);
        rows[0]
            .position
            .as_mut()
            .expect("fixture position")
            .insufficient = Some(Insufficient::TooManyGaps {
            coverage: 20,
            need: 80,
        });

        assert!(find(&rows).is_empty());
    }

    #[test]
    fn a_zero_listing_price_never_ranks() {
        let deals = find(&market(Some(Copper::ZERO), 0));

        assert!(deals.is_empty());
    }

    #[test]
    fn fifteen_offered_realms_cap_the_threshold_at_the_current_third() {
        let mut rows = Vec::new();
        let mut regional = row(Scope::Region, Some(Copper(2_000_000)), 1);
        regional.realms_listing = PERCENTILE_REALMS as u32;
        regional.realms_collected = PERCENTILE_REALMS as u32;
        rows.push(regional);
        for offset in 0..PERCENTILE_REALMS {
            rows.push(row(
                Scope::Realm(RealmId(1403 + offset as u32)),
                Some(Copper(2_000_000 + offset as u64 * 100_000)),
                1,
            ));
        }

        let deals = find(&rows);

        assert_eq!(deals.len(), 1);
        assert_eq!(
            deals[0].threshold,
            Copper(2_500_000),
            "§10's offered[len / 3], not an invented dispersion percentile"
        );
    }
}
