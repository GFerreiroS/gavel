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

use super::key::MarketKey;
use super::realm::RealmSample;
use super::{ItemId, PriceSample};

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
    pub const ALL: [Stat; 7] = [
        Stat::Haste,
        Stat::Crit,
        Stat::Mastery,
        Stat::Versatility,
        Stat::Primary,
        Stat::Stamina,
        Stat::None,
    ];

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
    /// Bind-on-equip gear from the current raid.
    Boe,
    /// A profession recipe.
    Recipe,
    /// Bought once per raid tier, for one boss. Its own category because it
    /// is neither a flask nor a utility item: it is a raid-night purchase
    /// with a completely different cadence.
    VantusRune,
    Utility,
    /// Crafting reagents, which are grouped by profession rather than by the
    /// consumable categories above.
    Reagent,
    /// Enchantments sold as scrolls, kits and spellthreads, grouped by the
    /// equipment slot they apply to.
    Enchant,
    /// Cut gems, grouped by nothing: there are sixteen of them and a heading
    /// per stat would be longer than the list under it.
    Gem,
}

impl Category {
    pub const ALL: [Category; 16] = [
        Category::Flask,
        Category::CombatPotion,
        Category::HealingPotion,
        Category::ManaPotion,
        Category::Food,
        Category::Feast,
        Category::WeaponOil,
        Category::WeaponStone,
        Category::AugmentRune,
        Category::VantusRune,
        Category::Utility,
        Category::Reagent,
        Category::Enchant,
        Category::Gem,
        Category::Boe,
        Category::Recipe,
    ];

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
            Category::VantusRune => "vantus_rune",
            Category::Utility => "utility",
            Category::Reagent => "reagent",
            Category::Enchant => "enchant",
            Category::Gem => "gem",
            Category::Boe => "boe",
            Category::Recipe => "recipe",
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
            Category::VantusRune => "Vantus runes",
            Category::Utility => "Utility",
            Category::Reagent => "Reagents",
            Category::Enchant => "Enchants",
            Category::Gem => "Gems",
            Category::Boe => "Bind-on-equip gear",
            Category::Recipe => "Recipes",
        }
    }
}

/// Which profession a reagent belongs to.
///
/// Not the same thing as Blizzard's item subclass, which is a *material* type:
/// "Optional Reagents" is 72 of the current expansion's tradeskill items and
/// says nothing about who makes them. The mapping comes from the profession
/// recipe lists, with gathered materials falling back to the profession that
/// gathers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profession {
    Alchemy,
    Blacksmithing,
    Cooking,
    Enchanting,
    Engineering,
    Fishing,
    Herbalism,
    Inscription,
    Jewelcrafting,
    Leatherworking,
    Mining,
    Skinning,
    Tailoring,
    /// World drops, motes and finishing reagents that no single profession
    /// owns. An honest bucket beats attributing them to a profession at
    /// random.
    Shared,
}

pub const ALL_PROFESSIONS: [Profession; 14] = [
    Profession::Alchemy,
    Profession::Blacksmithing,
    Profession::Cooking,
    Profession::Enchanting,
    Profession::Engineering,
    Profession::Fishing,
    Profession::Herbalism,
    Profession::Inscription,
    Profession::Jewelcrafting,
    Profession::Leatherworking,
    Profession::Mining,
    Profession::Skinning,
    Profession::Tailoring,
    Profession::Shared,
];

impl Profession {
    pub const fn as_str(self) -> &'static str {
        match self {
            Profession::Alchemy => "alchemy",
            Profession::Blacksmithing => "blacksmithing",
            Profession::Cooking => "cooking",
            Profession::Enchanting => "enchanting",
            Profession::Engineering => "engineering",
            Profession::Fishing => "fishing",
            Profession::Herbalism => "herbalism",
            Profession::Inscription => "inscription",
            Profession::Jewelcrafting => "jewelcrafting",
            Profession::Leatherworking => "leatherworking",
            Profession::Mining => "mining",
            Profession::Skinning => "skinning",
            Profession::Tailoring => "tailoring",
            Profession::Shared => "shared",
        }
    }

    /// Display name, translated through the interface catalogue.
    pub const fn label(self) -> &'static str {
        match self {
            Profession::Alchemy => "Alchemy",
            Profession::Blacksmithing => "Blacksmithing",
            Profession::Cooking => "Cooking",
            Profession::Enchanting => "Enchanting",
            Profession::Engineering => "Engineering",
            Profession::Fishing => "Fishing",
            Profession::Herbalism => "Herbalism",
            Profession::Inscription => "Inscription",
            Profession::Jewelcrafting => "Jewelcrafting",
            Profession::Leatherworking => "Leatherworking",
            Profession::Mining => "Mining",
            Profession::Skinning => "Skinning",
            Profession::Tailoring => "Tailoring",
            Profession::Shared => "Shared reagents",
        }
    }
}

/// What kind of market an entry is, and therefore which page shows it and how
/// it is grouped: consumables by who drinks them, reagents by profession.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// Defaulted so every catalogue written before reagents existed still
    /// parses unchanged.
    #[default]
    Consumable,
    Reagent,
    Enchant,
    Gem,
    /// Bind-on-equip gear. Unlike every kind above it, this is **not** a
    /// commodity: it is auctioned per connected realm, one item at a time,
    /// with a different price on every realm.
    Boe,
    /// Profession recipes: patterns, designs, plans. Per realm, like gear,
    /// but with no upgrade levels -- a recipe is a recipe.
    Recipe,
}

impl ItemKind {
    pub const ALL: [ItemKind; 6] = [
        ItemKind::Consumable,
        ItemKind::Reagent,
        ItemKind::Enchant,
        ItemKind::Gem,
        ItemKind::Boe,
        ItemKind::Recipe,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ItemKind::Consumable => "consumable",
            ItemKind::Reagent => "reagent",
            ItemKind::Enchant => "enchant",
            ItemKind::Gem => "gem",
            ItemKind::Boe => "boe",
            ItemKind::Recipe => "recipe",
        }
    }

    /// What to call this kind in the interface.
    pub const fn label(self) -> &'static str {
        match self {
            ItemKind::Consumable => "Consumables",
            ItemKind::Reagent => "Reagents",
            ItemKind::Enchant => "Enchants",
            ItemKind::Gem => "Gems",
            ItemKind::Boe => "Bind-on-equip gear",
            ItemKind::Recipe => "Recipes",
        }
    }

    /// Whether this kind trades on the region-wide commodity market.
    ///
    /// The two markets share nothing: a different endpoint, a different
    /// payload, a different table, and a price that means something different.
    /// Everything that branches on the distinction asks here rather than
    /// listing the kinds again and forgetting one.
    pub const fn is_commodity(self) -> bool {
        !matches!(self, ItemKind::Boe | ItemKind::Recipe)
    }

    /// Whether `scripts/catalog-sync.py` may rewrite this kind's entries.
    ///
    /// Enchants and gems are grouped by data Blizzard publishes, so a rewrite
    /// loses nothing. Consumables and reagents carry judgements the API cannot
    /// make -- which audience a potion is for, which profession makes a
    /// material -- and are edited by hand.
    pub const fn is_generated(self) -> bool {
        matches!(self, ItemKind::Enchant | ItemKind::Gem)
    }
}

/// An equipment slot: which slot an enchantment applies to, or which slot a
/// piece of gear occupies.
///
/// Shared by both because it is the same idea and the same word. For enchants
/// it is Blizzard's item subclass ("Enchant Ring" is subclass `Finger`); for
/// gear it is the inventory type. Ordered as a character sheet is, not
/// alphabetically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Head,
    Neck,
    Shoulder,
    Cloak,
    Chest,
    Wrist,
    Hands,
    Waist,
    Legs,
    Feet,
    Finger,
    Weapon,
    TwoHandedWeapon,
}

pub const ALL_SLOTS: [Slot; 13] = [
    Slot::Head,
    Slot::Neck,
    Slot::Shoulder,
    Slot::Cloak,
    Slot::Chest,
    Slot::Wrist,
    Slot::Hands,
    Slot::Waist,
    Slot::Legs,
    Slot::Feet,
    Slot::Finger,
    Slot::Weapon,
    Slot::TwoHandedWeapon,
];

impl Slot {
    pub const fn as_str(self) -> &'static str {
        match self {
            Slot::Head => "head",
            Slot::Neck => "neck",
            Slot::Shoulder => "shoulder",
            Slot::Cloak => "cloak",
            Slot::Chest => "chest",
            Slot::Wrist => "wrist",
            Slot::Hands => "hands",
            Slot::Waist => "waist",
            Slot::Legs => "legs",
            Slot::Feet => "feet",
            Slot::Finger => "finger",
            Slot::Weapon => "weapon",
            Slot::TwoHandedWeapon => "two_handed_weapon",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Slot::Head => "Head",
            Slot::Neck => "Neck",
            Slot::Shoulder => "Shoulder",
            Slot::Cloak => "Cloak",
            Slot::Chest => "Chest",
            Slot::Wrist => "Wrist",
            Slot::Hands => "Hands",
            Slot::Waist => "Waist",
            Slot::Legs => "Legs",
            Slot::Feet => "Feet",
            Slot::Finger => "Finger",
            Slot::Weapon => "Weapon",
            Slot::TwoHandedWeapon => "Two-handed weapon",
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
    /// Defaulted, so a catalogue written before reagents existed still parses.
    #[serde(default)]
    pub kind: ItemKind,
    /// Set on reagents; `None` on everything else.
    #[serde(default)]
    pub profession: Option<Profession>,
    /// Set on enchants; `None` on everything else.
    #[serde(default)]
    pub slot: Option<Slot>,
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
/// An upgrade track: the axis that actually separates gear prices.
///
/// A listing carries one track bonus (13332, 13333, 13334 on Midnight) and one
/// rank inside it (12825, 12826, …). The **track** is the market -- a Hero
/// piece and a Veteran piece are different things a buyer chooses between --
/// and the rank inside it is a range, not four markets nobody could price
/// separately. That is the same judgement CLAUDE.md §8 already recorded; this
/// type is what finally makes the pages agree with it.
///
/// Ordered weakest to strongest, which is the order a card lists them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Track {
    Veteran,
    Champion,
    Hero,
    Myth,
}

impl Track {
    /// Every track, in the order a card shows them.
    ///
    /// A fixed list rather than "whatever the market happens to hold": a card
    /// with three rows next to a card with four is a grid that does not line
    /// up, and "nobody is selling a Myth one" is an answer worth showing.
    pub const ALL: [Track; 4] = [Track::Veteran, Track::Champion, Track::Hero, Track::Myth];

    /// The English name, which is also the source string the templates
    /// translate. The game's own word for it in every language.
    pub const fn as_str(self) -> &'static str {
        match self {
            Track::Veteran => "Veteran",
            Track::Champion => "Champion",
            Track::Hero => "Hero",
            Track::Myth => "Myth",
        }
    }

    /// The form that goes in a URL: `/wow/gear/{item}/veteran`.
    pub const fn slug(self) -> &'static str {
        match self {
            Track::Veteran => "veteran",
            Track::Champion => "champion",
            Track::Hero => "hero",
            Track::Myth => "myth",
        }
    }

    /// The exact inverse of [`Track::slug`], and nothing else.
    ///
    /// Separate from [`Track::parse`] on purpose: `parse` is deliberately
    /// forgiving because it reads catalogue prose like "Champion 2/6", and a
    /// forgiving decoder is the wrong thing under a market key, where two
    /// spellings accepted for one track means two strings naming one market.
    pub fn from_slug(slug: &str) -> Option<Track> {
        Track::ALL.into_iter().find(|t| t.slug() == slug)
    }

    /// Parse a name or a slug. Case-insensitive, and tolerant of the rank
    /// being stuck on the end -- `item_levels` stores "Champion 2/6".
    pub fn parse(raw: &str) -> Option<Track> {
        let word = raw.split_whitespace().next().unwrap_or(raw);
        Track::ALL
            .into_iter()
            .find(|t| word.eq_ignore_ascii_case(t.as_str()))
    }
}

/// What one upgrade bonus id means.
///
/// Gear auctions carry bonus ids and nothing else -- no item level. These are
/// resolved once by `scripts/catalog-sync.py` (SimulationCraft says which ids
/// are upgrade levels, Wowhead says what they render as) and committed, so
/// the running app reads a reviewed file rather than a third-party service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemLevel {
    pub item_level: u16,
    /// "Champion 2/6" -- the track and the rank within it.
    pub upgrade: String,
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
    /// Upgrade bonus id -> what it means. One entry per item level that
    /// actually trades: an item level nobody is selling gets no page, which
    /// is why Mythic gear -- unbuyable, because upgrading binds it -- has no
    /// empty page waiting for it.
    #[serde(default)]
    pub item_levels: BTreeMap<String, ItemLevel>,
    /// Upgrade-track bonus id -> the track it names.
    ///
    /// Separate from `item_levels`, which is keyed by the *rank* bonus. Both
    /// are in every listing, and the track id is the reliable one: the market
    /// carries rank 12827 that no sync has resolved yet, and its listings
    /// still group correctly because 13332 is right there beside it.
    #[serde(default)]
    pub tracks: BTreeMap<String, Track>,
    /// Optional bonus id -> its name: "Prismatic Socket", "Leech". These do
    /// not divide a market, they are counted within one.
    #[serde(default)]
    pub modifiers: BTreeMap<String, String>,
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

    /// Every commodity id we care about, for filtering a commodity snapshot.
    ///
    /// Per-realm items are deliberately absent: they never appear in that
    /// payload, so including them would be asking the wrong market for a
    /// price that cannot be there.
    pub fn tracked_ids(&self) -> Vec<ItemId> {
        self.ids_where(|kind| kind.is_commodity())
    }

    /// Every id auctioned per connected realm, for filtering a realm snapshot.
    pub fn realm_tracked_ids(&self) -> Vec<ItemId> {
        self.ids_where(|kind| !kind.is_commodity())
    }

    fn ids_where(&self, keep: fn(ItemKind) -> bool) -> Vec<ItemId> {
        let mut ids: Vec<ItemId> = self
            .items
            .iter()
            .filter(|i| keep(i.kind))
            .flat_map(|i| i.item_ids())
            .collect();
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

    /// Consumables only: reagents share the `common` audience and would
    /// otherwise appear on the consumables page.
    pub fn by_audience(&self, audience: Audience) -> impl Iterator<Item = &CatalogItem> {
        self.items
            .iter()
            .filter(move |i| i.kind == ItemKind::Consumable && i.audience == audience)
    }

    pub fn of_kind(&self, kind: ItemKind) -> impl Iterator<Item = &CatalogItem> {
        self.items.iter().filter(move |i| i.kind == kind)
    }

    /// Reagents of one profession, in name order.
    pub fn by_profession(&self, profession: Profession) -> impl Iterator<Item = &CatalogItem> {
        self.of_kind(ItemKind::Reagent)
            .filter(move |i| i.profession == Some(profession))
    }

    /// Enchants for one equipment slot.
    pub fn by_slot(&self, slot: Slot) -> impl Iterator<Item = &CatalogItem> {
        self.of_kind(ItemKind::Enchant)
            .filter(move |i| i.slot == Some(slot))
    }

    /// What an upgrade bonus id means, if this catalog knows.
    pub fn item_level(&self, bonus: u32) -> Option<&ItemLevel> {
        self.item_levels.get(&bonus.to_string())
    }

    /// The upgrade track a bonus id names, if it names one.
    pub fn track(&self, bonus: u32) -> Option<Track> {
        self.tracks.get(&bonus.to_string()).copied()
    }

    /// The name of an optional bonus -- "Prismatic Socket", "Leech".
    pub fn modifier(&self, bonus: u32) -> Option<&str> {
        self.modifiers.get(&bonus.to_string()).map(String::as_str)
    }

    /// The bonus ids a stored variant carries.
    ///
    /// The variant is the listing's whole bonus list, comma separated and
    /// opaque to storage: keeping it whole is what makes every grouping rule
    /// below a display decision, so a patch that renumbers a bonus costs a
    /// catalogue entry and never any history (CLAUDE.md §8).
    pub fn bonuses(variant: &str) -> impl Iterator<Item = u32> + '_ {
        variant.split(',').filter_map(|id| id.parse::<u32>().ok())
    }

    /// The upgrade bonus in a variant: the one id this catalogue knows an item
    /// level for. Anything else it carries is optional, and is counted within
    /// a market rather than dividing one.
    pub fn upgrade_in(&self, variant: &str) -> Option<u32> {
        Catalog::bonuses(variant).find(|id| self.item_level(*id).is_some())
    }

    /// The rank a variant carries, if this catalogue has resolved it.
    pub fn rank_in(&self, variant: &str) -> Option<&ItemLevel> {
        self.upgrade_in(variant).and_then(|b| self.item_level(b))
    }

    /// The upgrade track a variant belongs to.
    ///
    /// The track bonus first, because it is the reliable one: the market
    /// carries rank 12827 that no sync has resolved, and its listings still
    /// land in Veteran because 13332 is beside it in the same variant. The
    /// rank's own wording is the fallback, for a catalogue synced before
    /// tracks were recorded.
    ///
    /// This lives here rather than in the page that draws the cards because it
    /// decides *which market a price belongs to*, and two copies of that rule
    /// are two answers to the same question.
    pub fn track_in(&self, variant: &str) -> Option<Track> {
        Catalog::bonuses(variant)
            .find_map(|id| self.track(id))
            .or_else(|| self.rank_in(variant).and_then(|l| Track::parse(&l.upgrade)))
    }

    /// The market a commodity observation belongs to.
    ///
    /// The rank comes from the catalogue rather than from the row, because the
    /// row has only an item id and a reader names the market by its rank.
    /// An item this catalogue does not track still gets a key -- rank 1 -- so
    /// that history collected under an older catalogue stays addressable.
    pub fn market_of(&self, sample: &PriceSample) -> MarketKey {
        let rank = self
            .find(sample.item)
            .and_then(|entry| entry.rank_of(sample.item))
            .unwrap_or(1);
        MarketKey::commodity(sample.region, sample.item, rank)
    }

    /// The market a per-realm observation belongs to.
    ///
    /// A recipe has one version of itself and no track; a BoE is one market
    /// per track. Anything this catalogue does not know the kind of is treated
    /// as gear, because that is the shape the per-realm table holds and an
    /// unresolved track is `None` rather than a market of its own.
    pub fn market_of_realm(&self, sample: &RealmSample) -> MarketKey {
        let kind = self.find(sample.item).map(|entry| entry.kind);
        if kind == Some(ItemKind::Recipe) {
            MarketKey::recipe(sample.region, sample.realm, sample.item)
        } else {
            MarketKey::boe(
                sample.region,
                sample.realm,
                sample.item,
                self.track_in(&sample.variant),
            )
        }
    }

    /// Recipes taught for one profession.
    pub fn recipes_for(&self, profession: Profession) -> impl Iterator<Item = &CatalogItem> {
        self.of_kind(ItemKind::Recipe)
            .filter(move |i| i.profession == Some(profession))
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
