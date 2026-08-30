//! Building a market card.
//!
//! Shared by the consumables and reagents pages: they group cards differently
//! -- by raid role, by profession -- but compute them identically, and the
//! figures on a card are the part worth having exactly one copy of.

use std::collections::BTreeMap;

use app_core::locale::Locale;
use app_core::market::engine::Insufficient;
use app_core::market::materialise::{MarketSummary, MarketWindow};
use app_core::market::{CatalogItem, ItemId, ItemKind};
use cluster_core::Millis;

use crate::views::{ItemCard, RankColumn, TooltipView};

/// What every column on a page needs and no card can work out for itself.
///
/// `newest` is the page's own snapshot, which is what makes a card's freshness
/// worth printing: every commodity market in a region is priced by the same
/// collection cycle, so an age is only news when it is *older* than the page's.
/// Passing the page's newest observation down is what lets a column say that
/// about itself rather than making the reader compare two timestamps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CardContext {
    pub locale: Locale,
    pub now: Millis,
    pub newest: Option<Millis>,
}

/// One consumable as a card, with a column per quality rank.
pub(crate) fn card(
    entry: &CatalogItem,
    latest: &BTreeMap<ItemId, MarketSummary>,
    recent: &BTreeMap<ItemId, MarketWindow>,
    tooltips: &BTreeMap<u32, TooltipView>,
    ctx: CardContext,
) -> ItemCard {
    let multi_rank = entry.ranks.len() > 1;
    let mut ranks: Vec<&app_core::market::ItemRank> = entry.ranks.iter().collect();
    ranks.sort_by_key(|r| r.rank);

    let columns: Vec<RankColumn> = ranks
        .iter()
        .map(|rank| {
            column(
                rank.item_id,
                // "R1"/"R2" are not words and stay as they are; "Price" is,
                // and reaches the template through `|t` like every other
                // label. It was the one string on this card that did not, and
                // it read "Price" on an otherwise Spanish page.
                if multi_rank {
                    format!("R{}", rank.rank)
                } else {
                    "Price".to_string()
                },
                latest.get(&rank.item_id),
                recent.get(&rank.item_id),
                ctx,
            )
        })
        .collect();

    // `ranks` is sorted above, so the last is the highest.
    let tooltip_item_id = ranks.last().map(|r| r.item_id.get()).unwrap_or_default();

    let tooltip = tooltips.get(&tooltip_item_id);

    ItemCard {
        // A reagent's category label would only ever read "Reagents", and an
        // enchant's "Enchants". Blizzard's own subclass is the useful line --
        // the material type, the equipment slot, the gem's stat -- and it
        // arrives localised.
        material: match entry.kind {
            ItemKind::Reagent
            | ItemKind::Enchant
            | ItemKind::Gem
            | ItemKind::Boe
            | ItemKind::Recipe => tooltip.and_then(|t| t.material.clone()),
            ItemKind::Consumable => None,
        },
        // The localised name when we have it, the catalog's English otherwise.
        name: tooltips
            .get(&tooltip_item_id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| entry.name.clone()),
        icon: entry.icon_url(),
        tooltip_item_id,
        tooltip: tooltips.get(&tooltip_item_id).cloned(),
        category: entry.category.label(),
        stat: entry.stat.as_str(),
        rarity: tooltip.map(|t| t.rarity).unwrap_or_default(),
        sort_name: entry.name.clone(),
        any_data: columns.iter().any(|c| c.has_data),
        columns,
    }
}

/// The order cards appear in within a group: rarest first, then by name.
///
/// One rule for every grid, because a reader who learns the order on one page
/// should not have to learn it again on the next. Rarity comes from the
/// tooltip cache; an item whose tooltip has not been fetched yet sorts as if
/// it were common, which is a cold-cache page settling down rather than a
/// wrong answer.
pub(crate) fn by_rarity(a: &ItemCard, b: &ItemCard) -> std::cmp::Ordering {
    b.rarity.cmp(&a.rarity).then_with(|| a.name.cmp(&b.name))
}

/// The item's name in the visitor's language, with the rank suffix the
/// catalog would have added.
///
/// The game has one name per item whatever the rank, so the suffix stays ours
/// and stays untranslated -- "R2" is not a word.
pub(crate) fn display_name(
    tooltips: &BTreeMap<u32, TooltipView>,
    entry: &CatalogItem,
    item: ItemId,
) -> String {
    let base = tooltips
        .get(&item.get())
        .map(|t| t.name.clone())
        .unwrap_or_else(|| entry.name.clone());
    match (entry.ranks.len(), entry.rank_of(item)) {
        (total, Some(rank)) if total > 1 => format!("{base} (R{rank})"),
        _ => base,
    }
}

/// One market's column of figures.
///
/// Everything here comes from a stored row: the band and the median are the
/// engine's, materialised under the published version, and the sparkline was
/// reduced to its slots when that version was built. §15's read path -- the
/// request draws, it does not reduce.
fn column(
    id: ItemId,
    label: String,
    sample: Option<&MarketSummary>,
    recent: Option<&MarketWindow>,
    ctx: CardContext,
) -> RankColumn {
    let base = RankColumn {
        item_id: id.get(),
        label,
        has_data: false,
        current: "\u{2014}".into(),
        median: "\u{2014}".into(),
        delta_percent: 0,
        cheap: false,
        dear: false,
        band: None,
        band_slug: "none",
        rank_percent: None,
        insufficient: None,
        insufficient_have: 0,
        insufficient_need: 0,
        quantity: 0,
        listings: 0,
        freshness: "\u{2014}".into(),
        stale: false,
        spark: String::new(),
    };

    let Some(sample) = sample else {
        // Tracked but never seen: collection has not run, or this rank has no
        // listings at all.
        return base;
    };

    // Everything below describes the reader's chosen comparison window, and
    // the same one throughout: the band, the median it is measured from, and
    // the line. A card whose shape covered a fortnight while its percentile
    // covered a month would be two answers to one question.
    let window = recent.filter(|w| w.samples > 0);

    // Against the window's *median*, not its mean. The mean is what this line
    // compared against before Phase 5, and it is the measure a single spike
    // moves: §5.4's whole argument for robust statistics, applied to the one
    // number a reader actually looks at.
    let delta = window
        .and_then(|w| w.position.from_median_percent)
        .unwrap_or(0);

    let position = window.map(|w| w.position);
    let insufficient = position.and_then(|p| p.insufficient);

    RankColumn {
        has_data: true,
        current: sample.price.to_string(),
        median: window
            .map(|w| w.distribution.median.to_string())
            .unwrap_or_else(|| base.median.clone()),
        delta_percent: delta,
        // The thresholds stay where they were: this is the same "noticeably
        // cheaper than usual" arrow, now measured from a robust centre.
        cheap: delta <= -15,
        dear: delta >= 15,
        band: position.and_then(|p| p.valuation).map(|v| v.as_str()),
        band_slug: position
            .and_then(|p| p.valuation)
            .map(|v| v.slug())
            .unwrap_or(base.band_slug),
        rank_percent: position.and_then(|p| p.rank),
        insufficient: insufficient.map(|reason| match reason {
            Insufficient::NotEnoughHistory { .. } => "Not enough history",
            Insufficient::TooManyGaps { .. } => "Too many gaps",
        }),
        insufficient_have: match insufficient {
            Some(Insufficient::NotEnoughHistory { have, .. }) => have,
            Some(Insufficient::TooManyGaps { coverage, .. }) => coverage,
            None => 0,
        },
        insufficient_need: match insufficient {
            Some(Insufficient::NotEnoughHistory { need, .. })
            | Some(Insufficient::TooManyGaps { need, .. }) => need,
            None => 0,
        },
        quantity: sample.quantity,
        listings: sample.listings,
        freshness: sample
            .observed_at
            .map(|at| crate::format::ago(ctx.locale, ctx.now.since(at)))
            .unwrap_or_else(|| base.freshness.clone()),
        // Older than the page's own snapshot by more than a collection cycle.
        // Every commodity market in a region is priced by the same cycle, so
        // an age is only news when this market missed one -- which is exactly
        // the case where the figures above describe a different moment from
        // the ones on the card beside it.
        stale: match (sample.observed_at, ctx.newest) {
            (Some(at), Some(newest)) => newest.since(at) > STALE_AFTER_MS,
            _ => false,
        },
        spark: window
            .map(|w| {
                crate::chart::sparkline(
                    &w.spark,
                    crate::i18n::translate(ctx.locale, "Price over the comparison window"),
                )
            })
            .unwrap_or_default(),
        ..base
    }
}

/// How far behind the page's snapshot a market may be before its card says so.
///
/// Snapshots are hourly and a collection cycle is thirty minutes, so an hour
/// and a half is one missed cycle with room for the clock. Tighter than that
/// and every page would be covered in warnings about markets that were merely
/// collected in a different order.
const STALE_AFTER_MS: u64 = 90 * 60 * 1000;
