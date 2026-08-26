//! Which items we track, how they are grouped, and which expansion they belong
//! to.
//!
//! The catalog is data, not code: it lives in `catalogs.json` and is embedded
//! at build time. A new raid tier is a data edit, not a recompile of logic.
//!
//! **Catalogs are archival.** There is one per expansion. Exactly one is
//! `Active` and is the only one ever collected; the rest are `Archived` --
//! still browsable, permanently frozen. When an expansion ends you flip its
//! status and add the next one, and the old prices stay queryable forever
//! rather than being deleted.
//!
//! Patch boundaries are recorded so history can be segmented: "what did a
//! flask cost during 12.0 versus 12.1" is the question this exists to answer.

use std::collections::BTreeMap;

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

use super::ItemId;

/// Who the consumable is for.
///
/// Most Midnight consumables are genuinely universal -- flasks are keyed on a
/// secondary stat rather than on primary stat -- but weapon enhancements are
/// not: weightstones and whetstones are attack power, oils are spell damage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// Tanks and melee DPS.
    Melee,
    /// Caster DPS and healers.
    Caster,
    /// Useful to both.
    Common,
}

pub const ALL_AUDIENCES: [Audience; 3] = [Audience::Melee, Audience::Caster, Audience::Common];

/// Display order for the UI: the shared bucket first, since most Midnight
/// consumables land there, then the two role-specific ones.
pub const ALL_AUDIENCES_LABELS: [(Audience, &str); 3] = [
    (Audience::Common, "Everyone"),
    (Audience::Melee, "Melee — tanks and melee DPS"),
    (Audience::Caster, "Caster — caster DPS and healers"),
];

impl Audience {
    pub const fn as_str(self) -> &'static str {
        match self {
            Audience::Melee => "melee",
            Audience::Caster => "caster",
            Audience::Common => "common",
        }
    }

    pub fn parse(s: &str) -> Option<Audience> {
        ALL_AUDIENCES.into_iter().find(|a| a.as_str() == s)
    }
}

/// Secondary metadata: which stat the consumable grants. Free to carry and
/// makes "show me the haste options" possible without a second catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stat {
    Haste,
    Crit,
    Mastery,
    Versatility,
    Primary,
    Stamina,
    /// Healing, mana, utility -- no stat.
    None,
}

impl Stat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Stat::Haste => "haste",
            Stat::Crit => "crit",
            Stat::Mastery => "mastery",
            Stat::Versatility => "versatility",
            Stat::Primary => "primary",
            Stat::Stamina => "stamina",
            Stat::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Flask,
    CombatPotion,
    HealingPotion,
    ManaPotion,
    Food,
    Feast,
    WeaponOil,
    WeaponStone,
    AugmentRune,
    Utility,
}

impl Category {
    pub const fn as_str(self) -> &'static str {
        match self {
            Category::Flask => "flask",
            Category::CombatPotion => "combat_potion",
            Category::HealingPotion => "healing_potion",
            Category::ManaPotion => "mana_potion",
            Category::Food => "food",
            Category::Feast => "feast",
            Category::WeaponOil => "weapon_oil",
            Category::WeaponStone => "weapon_stone",
            Category::AugmentRune => "augment_rune",
            Category::Utility => "utility",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Category::Flask => "Flasks",
            Category::CombatPotion => "Combat potions",
            Category::HealingPotion => "Healing potions",
            Category::ManaPotion => "Mana potions",
            Category::Food => "Food",
            Category::Feast => "Feasts",
            Category::WeaponOil => "Weapon oils",
            Category::WeaponStone => "Weapon stones",
            Category::AugmentRune => "Augment runes",
            Category::Utility => "Utility",
        }
    }
}

/// One quality rank of a consumable.
///
/// Crafted consumables come in ranks, and **each rank is a separate item id
/// and therefore a separate market**. R2 of a flask can be half the price of
/// R3 with most of the benefit, so tracking them as one item would hide the
/// thing you actually want to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRank {
    pub rank: u8,
    pub item_id: ItemId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub name: String,
    pub category: Category,
    pub audience: Audience,
    #[serde(default = "default_stat")]
    pub stat: Stat,
    /// Every known quality rank. May hold one entry if the item has no ranks.
    pub ranks: Vec<ItemRank>,
    /// Optional hard floor, in copper per unit, used for alerting before there
    /// is enough history for a percentile to mean anything.
    #[serde(default)]
    pub floor_copper: Option<u64>,
    /// Blizzard icon filename, e.g. `7548904.jpg`. Only the slug is stored:
    /// the host and size belong to the template, not the catalog.
    #[serde(default)]
    pub icon: Option<String>,
}

fn default_stat() -> Stat {
    Stat::None
}

impl CatalogItem {
    /// Full icon URL at the given square size. Blizzard serves 56px icons;
    /// larger requests fall back to the same asset, so 56 is what we ask for.
    pub fn icon_url(&self) -> Option<String> {
        self.icon
            .as_ref()
            .map(|slug| format!("https://render.worldofwarcraft.com/eu/icons/56/{slug}"))
    }

    pub fn item_ids(&self) -> impl Iterator<Item = ItemId> + '_ {
        self.ranks.iter().map(|r| r.item_id)
    }

    pub fn rank_of(&self, item: ItemId) -> Option<u8> {
        self.ranks
            .iter()
            .find(|r| r.item_id == item)
            .map(|r| r.rank)
    }

    /// Display name including the rank, when the item has more than one.
    pub fn display_name(&self, item: ItemId) -> String {
        match (self.ranks.len(), self.rank_of(item)) {
            (n, Some(rank)) if n > 1 => format!("{} (R{rank})", self.name),
            _ => self.name.clone(),
        }
    }
}

/// Whether a catalog is still being collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogStatus {
    /// The current expansion. Collected every cycle.
    #[default]
    Active,
    /// A finished expansion. Readable, never written to again.
    Archived,
}

impl CatalogStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            CatalogStatus::Active => "active",
            CatalogStatus::Archived => "archived",
        }
    }

    pub const fn is_active(self) -> bool {
        matches!(self, CatalogStatus::Active)
    }
}

/// A patch within an expansion, used to segment the price history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// e.g. "12.1".
    pub patch: String,
    pub name: String,
    /// `YYYY-MM-DD`, the day the patch went live.
    pub started: String,
}

impl Patch {
    pub fn started_at(&self) -> Millis {
        parse_date(&self.started).unwrap_or(Millis::ZERO)
    }

    pub fn label(&self) -> String {
        format!("{} — {}", self.patch, self.name)
    }
}

/// `YYYY-MM-DD` to an instant. Returns `None` on anything malformed rather
/// than silently landing at the epoch.
fn parse_date(value: &str) -> Option<Millis> {
    let mut parts = value.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(Millis::from_utc_date(year, month, day))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// Stable slug, used in URLs: `/wow/consumables/midnight`.
    pub id: String,
    pub expansion: String,
    /// Which content this catalog is for, shown in the UI so a stale catalog
    /// is visible rather than silently wrong.
    pub season: String,
    #[serde(default)]
    pub status: CatalogStatus,
    /// Patch boundaries, oldest first.
    #[serde(default)]
    pub patches: Vec<Patch>,
    pub items: Vec<CatalogItem>,
}

impl Catalog {
    pub fn from_json(json: &str) -> Result<Catalog, String> {
        serde_json::from_str(json).map_err(|e| format!("catalog: {e}"))
    }

    pub fn is_active(&self) -> bool {
        self.status.is_active()
    }

    /// Patch windows as `(patch, from, until)`, the last one open-ended.
    ///
    /// Derived from the start dates rather than stored, so adding a patch is a
    /// one-line edit and every window stays contiguous by construction.
    pub fn patch_windows(&self) -> Vec<(&Patch, Millis, Option<Millis>)> {
        let mut sorted: Vec<&Patch> = self.patches.iter().collect();
        sorted.sort_by_key(|p| p.started_at());
        sorted
            .iter()
            .enumerate()
            .map(|(i, patch)| {
                let next = sorted.get(i + 1).map(|p| p.started_at());
                (*patch, patch.started_at(), next)
            })
            .collect()
    }

    /// The whole expansion's window: from its first patch to now, or to the
    /// start of whatever came after it.
    pub fn span_start(&self) -> Millis {
        self.patches
            .iter()
            .map(|p| p.started_at())
            .min()
            .unwrap_or(Millis::ZERO)
    }

    /// Every item id we care about, for filtering a snapshot.
    pub fn tracked_ids(&self) -> Vec<ItemId> {
        let mut ids: Vec<ItemId> = self.items.iter().flat_map(|i| i.item_ids()).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Reverse index from item id to its catalog entry.
    pub fn index(&self) -> BTreeMap<ItemId, &CatalogItem> {
        let mut map = BTreeMap::new();
        for item in &self.items {
            for id in item.item_ids() {
                map.insert(id, item);
            }
        }
        map
    }

    pub fn find(&self, item: ItemId) -> Option<&CatalogItem> {
        self.items.iter().find(|i| i.rank_of(item).is_some())
    }

    pub fn by_audience(&self, audience: Audience) -> impl Iterator<Item = &CatalogItem> {
        self.items.iter().filter(move |i| i.audience == audience)
    }
}

/// Every catalog the build knows about: one per expansion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSet {
    pub catalogs: Vec<Catalog>,
}

const EMBEDDED: &str = include_str!("catalogs.json");

impl CatalogSet {
    /// Parse the shipped catalogs. Panics only if the embedded file is
    /// invalid, which a test catches at build time.
    pub fn embedded() -> CatalogSet {
        serde_json::from_str(EMBEDDED).expect("embedded catalogs.json is malformed")
    }

    pub fn from_json(json: &str) -> Result<CatalogSet, String> {
        serde_json::from_str(json).map_err(|e| format!("catalogs: {e}"))
    }

    /// The expansion currently being collected.
    ///
    /// Exactly one catalog should be active. If several are, the first wins
    /// and the rest are effectively archived -- a validation test guards it so
    /// that cannot happen by accident.
    pub fn active(&self) -> Option<&Catalog> {
        self.catalogs.iter().find(|c| c.is_active())
    }

    pub fn by_id(&self, id: &str) -> Option<&Catalog> {
        self.catalogs.iter().find(|c| c.id == id)
    }

    /// Newest first, active before archived -- display order.
    pub fn ordered(&self) -> Vec<&Catalog> {
        let mut all: Vec<&Catalog> = self.catalogs.iter().collect();
        all.sort_by(|a, b| {
            b.is_active()
                .cmp(&a.is_active())
                .then(b.span_start().cmp(&a.span_start()))
        });
        all
    }

    /// Ids of every item across every catalog, for reverse lookups over
    /// archived history.
    pub fn index(&self) -> BTreeMap<ItemId, (&Catalog, &CatalogItem)> {
        let mut map = BTreeMap::new();
        for catalog in &self.catalogs {
            for item in &catalog.items {
                for id in item.item_ids() {
                    map.insert(id, (catalog, item));
                }
            }
        }
        map
    }
}
