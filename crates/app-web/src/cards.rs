//! Building a market card.
//!
//! Shared by the consumables and reagents pages: they group cards differently
//! -- by raid role, by profession -- but compute them identically, and the
//! figures on a card are the part worth having exactly one copy of.

use std::collections::BTreeMap;

use app_core::market::materialise::{MarketSummary, MarketWindow};
use app_core::market::{CatalogItem, ItemId, ItemKind};

use crate::views::{ItemCard, RankColumn, TooltipView};

/// One consumable as a card, with a column per quality rank.
pub(crate) fn card(
    entry: &CatalogItem,
    latest: &BTreeMap<ItemId, MarketSummary>,
    recent: &BTreeMap<ItemId, MarketWindow>,
    all_time: &BTreeMap<ItemId, MarketWindow>,
    tooltips: &BTreeMap<u32, TooltipView>,
) -> ItemCard {
    let multi_rank = entry.ranks.len() > 1;
    let mut ranks: Vec<&app_core::market::ItemRank> = entry.ranks.iter().collect();
    ranks.sort_by_key(|r| r.rank);

    let columns: Vec<RankColumn> = ranks
        .iter()
        .map(|rank| {
            column(
                rank.item_id,
                if multi_rank {
                    format!("R{}", rank.rank)
                } else {
                    "Price".to_string()
                },
                latest.get(&rank.item_id),
                recent.get(&rank.item_id),
                all_time.get(&rank.item_id),
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

fn column(
    id: ItemId,
    label: String,
    sample: Option<&MarketSummary>,
    recent: Option<&MarketWindow>,
    all_time: Option<&MarketWindow>,
) -> RankColumn {
    let base = RankColumn {
        item_id: id.get(),
        label,
        has_data: false,
        current: "\u{2014}".into(),
        mean: "\u{2014}".into(),
        low: "\u{2014}".into(),
        low_when: "\u{2014}".into(),
        high: "\u{2014}".into(),
        high_when: "\u{2014}".into(),
        quantity: 0,
        delta_percent: 0,
        cheap: false,
        dear: false,
    };

    let Some(sample) = sample else {
        // Tracked but never seen: collection has not run, or this rank has no
        // listings at all.
        return base;
    };

    // "vs usual" compares against the recent window, not all time: a price
    // that is normal for this month should not read as cheap because it was
    // cheaper at launch.
    let delta = match recent {
        Some(w) if w.samples > 1 && w.mean.get() > 0 => {
            let current = sample.price.get() as i128;
            let mean = w.mean.get() as i128;
            ((current - mean) * 100 / mean) as i32
        }
        _ => 0,
    };
    let dated = all_time.filter(|w| w.samples > 0);

    RankColumn {
        has_data: true,
        current: sample.price.to_string(),
        mean: dated
            .map(|w| w.mean.to_string())
            .unwrap_or_else(|| base.mean.clone()),
        low: dated
            .map(|w| w.low.to_string())
            .unwrap_or_else(|| base.low.clone()),
        low_when: dated
            .map(|w| w.low_at.to_date_string())
            .unwrap_or_else(|| base.low_when.clone()),
        high: dated
            .map(|w| w.high.to_string())
            .unwrap_or_else(|| base.high.clone()),
        high_when: dated
            .map(|w| w.high_at.to_date_string())
            .unwrap_or_else(|| base.high_when.clone()),
        quantity: sample.quantity,
        delta_percent: delta,
        cheap: delta <= -15,
        dear: delta >= 15,
        ..base
    }
}
