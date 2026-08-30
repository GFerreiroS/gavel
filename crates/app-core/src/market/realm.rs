//! The per-realm auction house: gear, not commodities.
//!
//! Everything in [`super`] assumes a commodity — stackable, region-wide, one
//! price. Bind-on-equip gear is none of those. It is auctioned one item at a
//! time on each connected realm, and the same item id trades at several item
//! levels under different *bonus ids*, at prices an order of magnitude apart:
//! on one realm a Temple Delver's Mystic Helm was listed at 9,000g and at
//! 330,000g on the same afternoon, and both were honest prices for what they
//! were.
//!
//! **What Blizzard does not give us is the item level.** A listing carries
//! `bonus_lists` and nothing else, and there is no published table mapping a
//! bonus id to an item level. So this module deliberately does not try: it
//! records the bonus list verbatim as the market's identity, and leaves the
//! grouping of those variants into named tiers to the layer that displays
//! them. A patch that renumbers bonus ids then breaks a display rule, which
//! is cheap to fix, instead of the collection, which would cost history that
//! cannot be re-fetched.

use std::collections::BTreeMap;

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

use super::{Copper, ItemId, Ladder, Listing, Region};

/// A connected realm: several realms sharing one auction house.
///
/// Unique only within a region -- EU 1403 and US 1403 are different places --
/// so nothing here is ever keyed by realm alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RealmId(pub u32);

impl RealmId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for RealmId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One realm's auction house, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realm {
    pub id: RealmId,
    pub region: Region,
    /// "Dentarg, Tarren Mill" -- a connected realm can be several.
    pub name: String,
    /// The individual realms sharing this auction house: `["Dun Modr",
    /// "C'Thun"]`. A player looks for one of these, not for the joined name.
    pub members: Vec<String>,
    /// The language it is played in, as Blizzard's tag: `enGB`, `deDE`, `ruRU`.
    ///
    /// EU shares one region between seven languages, and a reader looking for
    /// their own realm among ninety-two is looking for their own *language*
    /// first. Empty when unknown.
    pub locale: String,
    /// Whether prices are collected from it. A realm switched off keeps every
    /// sample it already has and simply stops gaining more.
    pub enabled: bool,
}

/// A single gear auction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GearListing {
    pub item: ItemId,
    /// What it costs to take it now. An auction with only a bid is carried at
    /// the bid: it is still a price someone can pay, and dropping it would
    /// silently thin out the cheap end of the market.
    pub price: Copper,
    /// Sorted and deduplicated, so it can be compared and used as a key.
    pub bonus_ids: Vec<u32>,
}

impl GearListing {
    /// The market this listing belongs to: everything that distinguishes one
    /// version of an item from another, as a stable string.
    ///
    /// The bonus list *is* the identity. Item level, sockets and tertiaries
    /// are all functions of it, so grouping on it can never merge two things
    /// that are genuinely different -- only split things that turn out to be
    /// the same, which the display layer can undo.
    pub fn variant(&self) -> String {
        let mut out = String::new();
        for (i, id) in self.bonus_ids.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(itoa(*id).as_str());
        }
        out
    }
}

fn itoa(value: u32) -> String {
    value.to_string()
}

/// One variant of one item on one realm at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealmSample {
    pub item: ItemId,
    pub region: Region,
    pub realm: RealmId,
    /// The bonus list, comma separated. Opaque here on purpose.
    pub variant: String,
    pub observed_at: Millis,
    /// The cheapest way to own one right now. This -- not an average -- is
    /// the number a buyer acts on, and with a handful of listings per variant
    /// a percentile would be noise dressed as precision.
    pub min_price: Copper,
    pub median_price: Copper,
    /// The dearest listing. On a single realm this is the only spread there
    /// is: with no other realm to compare to, "cheapest and highest here" is
    /// what a buyer can act on.
    pub max_price: Copper,
    pub listings: u32,
}

/// Reduce a realm's listings to one sample per (item, variant).
///
/// Listings arrive already filtered to the tracked items: the payload is 20 MB
/// and 100,000 auctions, of which a few hundred are ours.
pub fn summarise_realm(
    listings: Vec<GearListing>,
    region: Region,
    realm: RealmId,
    observed_at: Millis,
) -> (Vec<RealmSample>, Vec<(ItemId, String, Ladder)>) {
    let mut grouped: BTreeMap<(ItemId, String), Vec<Copper>> = BTreeMap::new();
    for listing in listings {
        grouped
            .entry((listing.item, listing.variant()))
            .or_default()
            .push(listing.price);
    }

    let mut samples = Vec::with_capacity(grouped.len());
    let mut ladders = Vec::with_capacity(grouped.len());
    for ((item, variant), mut prices) in grouped {
        prices.sort_unstable();
        let (Some(min), Some(max)) = (prices.first().copied(), prices.last().copied()) else {
            continue;
        };

        // **The sparse ladder.** A gear auction is one item, so every rung
        // here is a quantity of one unless two sellers happen to have listed
        // at exactly the same copper -- which is why `Ladder::is_sparse` exists
        // and why the depth metrics that assume a distribution decline on
        // these. It is still worth storing: "four for sale, at 25k, 31k, 31k
        // and 300k" is the whole of what a buyer wants to know about a BoE,
        // and no summary of min/median/max says it.
        ladders.push((
            item,
            variant.clone(),
            Ladder::of(
                &prices
                    .iter()
                    .map(|price| Listing {
                        item,
                        unit_price: *price,
                        quantity: 1,
                    })
                    .collect::<Vec<_>>(),
            ),
        ));

        samples.push(RealmSample {
            item,
            region,
            realm,
            variant,
            observed_at,
            min_price: min,
            median_price: prices[prices.len() / 2],
            max_price: max,
            listings: prices.len() as u32,
        });
    }
    (samples, ladders)
}

/// What a realm snapshot fetch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealmSnapshot {
    /// Unchanged since the timestamp we sent. Realms regenerate on their own
    /// schedules, so this is checked per realm rather than per region.
    NotModified,
    Fresh {
        /// `Last-Modified`: when Blizzard generated *this realm's* snapshot.
        generated_at: Millis,
        listings: Vec<GearListing>,
    },
}

/// Reads one connected realm's auction house.
///
/// Separate from [`super::CommodityProvider`] because it is a different
/// endpoint returning a different shape, and because a caller has to name a
/// realm: there is no such thing as "the" price of a piece of gear.
pub trait RealmAuctionProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &'static str;

    fn is_configured(&self) -> bool {
        true
    }

    /// Fetch one realm's auctions, keeping only `wanted` items.
    ///
    /// The payload is roughly 20 MB and 100,000 auctions of which a few
    /// hundred are ours, so filtering happens while parsing rather than
    /// after: the discarded 99% never becomes a `GearListing`.
    fn auctions(
        &self,
        region: Region,
        realm: RealmId,
        wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> impl std::future::Future<Output = crate::AppResult<RealmSnapshot>> + Send;

    /// Name connected realms.
    ///
    /// An empty `wanted` means *every* realm in the region, discovered from
    /// the index. Naming them costs one small request each, at startup only,
    /// and the names are stored so a later start can skip it.
    fn realms(
        &self,
        region: Region,
        wanted: &[RealmId],
    ) -> impl std::future::Future<Output = crate::AppResult<Vec<Realm>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(item: u32, price: u64, bonus: &[u32]) -> GearListing {
        let mut bonus_ids = bonus.to_vec();
        bonus_ids.sort_unstable();
        bonus_ids.dedup();
        GearListing {
            item: ItemId(item),
            price: Copper(price),
            bonus_ids,
        }
    }

    /// The same item at two upgrade levels is two markets. Averaging them
    /// would report a price nobody can buy at: the real listings were 9,000g
    /// and 330,000g.
    #[test]
    fn variants_of_one_item_are_separate_markets() {
        let (samples, ladders) = summarise_realm(
            vec![
                listing(271438, 90_000_000, &[6652, 10844, 12825, 13332]),
                listing(271438, 99_110_000, &[6652, 10844, 12825, 13332]),
                listing(271438, 3_300_000_000, &[6652, 10844, 12843, 13334]),
            ],
            Region::Eu,
            RealmId(1403),
            Millis(1_000),
        );

        assert_eq!(samples.len(), 2, "two upgrade levels, two markets");
        let cheap = &samples[0];
        assert_eq!(cheap.variant, "6652,10844,12825,13332");
        assert_eq!(cheap.listings, 2);
        assert_eq!(cheap.min_price, Copper(90_000_000));
        assert_eq!(cheap.max_price, Copper(99_110_000));

        // A ladder per market, and the sparse shape: one unit a rung, because
        // a gear auction is one item.
        assert_eq!(ladders.len(), 2, "a ladder per market, not per item");
        let (_, variant, ladder) = &ladders[0];
        assert_eq!(variant, "6652,10844,12825,13332");
        assert_eq!(ladder.levels(), 2);
        assert_eq!(ladder.total(), 2);
        assert_eq!(ladder.cheapest(), Some(Copper(90_000_000)));
        assert!(
            ladder.is_sparse(),
            "two auctions is not a distribution, and the depth metrics say so"
        );
        assert_eq!(ladder.supply_percentile(50), None);
    }

    /// The bonus list is written in a stable order whatever order it arrived
    /// in, or the same market would split in two between snapshots.
    #[test]
    fn a_variant_key_does_not_depend_on_arrival_order() {
        let a = listing(271438, 1, &[13332, 6652, 10844]);
        let b = listing(271438, 1, &[10844, 13332, 6652]);
        assert_eq!(a.variant(), b.variant());
        assert_eq!(a.variant(), "6652,10844,13332");
    }

    /// Gear with no bonus ids at all is the base version, and still a market.
    #[test]
    fn a_plain_item_is_its_own_variant() {
        let (samples, _) = summarise_realm(
            vec![listing(271434, 50_000, &[])],
            Region::Us,
            RealmId(60),
            Millis(1),
        );
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].variant, "");
        assert_eq!(samples[0].median_price, Copper(50_000));
    }
}
