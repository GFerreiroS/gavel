//! Item detail: the data behind an in-game style tooltip.
//!
//! Separate from [`crate::market`] on purpose. The market module is about what
//! an item *costs*; this is about what an item *is*. The two are joined only
//! by [`ItemId`].
//!
//! Everything here is our own shape. The Blizzard adapter maps its payload
//! into these types and nothing outside that adapter ever sees a
//! provider-shaped field.

use std::future::Future;

use cluster_core::Millis;
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::locale::Locale;
use crate::market::{Copper, ItemId, Region};

/// Item quality. Drives one thing in the UI -- the colour of the name -- but
/// it is modelled as an enum rather than a colour string because colour is a
/// presentation decision and this crate does not make those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemQuality {
    Poor,
    #[default]
    Common,
    Uncommon,
    Rare,
    Epic,
    Legendary,
    Artifact,
    Heirloom,
}

impl ItemQuality {
    /// Stable slug, used for a CSS class and for cache round-tripping.
    pub const fn as_str(self) -> &'static str {
        match self {
            ItemQuality::Poor => "poor",
            ItemQuality::Common => "common",
            ItemQuality::Uncommon => "uncommon",
            ItemQuality::Rare => "rare",
            ItemQuality::Epic => "epic",
            ItemQuality::Legendary => "legendary",
            ItemQuality::Artifact => "artifact",
            ItemQuality::Heirloom => "heirloom",
        }
    }

    /// How rare this is, ascending. Used to sort a grid of cards so the
    /// rarer item comes first; the enum's own order is the game's, and an
    /// explicit number keeps that ordering from depending on how the variants
    /// happen to be written down.
    pub const fn rarity(self) -> u8 {
        match self {
            ItemQuality::Poor => 0,
            ItemQuality::Common => 1,
            ItemQuality::Uncommon => 2,
            ItemQuality::Rare => 3,
            ItemQuality::Epic => 4,
            ItemQuality::Legendary => 5,
            ItemQuality::Artifact => 6,
            // Not rarer than an artifact, but never in the same grid as one:
            // above epic is enough to sort it where a reader expects.
            ItemQuality::Heirloom => 5,
        }
    }

    /// Parse the upstream's screaming-snake quality type. Unknown values fall
    /// back to `Common` rather than failing the whole tooltip.
    pub fn parse(value: &str) -> ItemQuality {
        match value.trim().to_ascii_uppercase().as_str() {
            "POOR" => ItemQuality::Poor,
            "UNCOMMON" => ItemQuality::Uncommon,
            "RARE" => ItemQuality::Rare,
            "EPIC" => ItemQuality::Epic,
            "LEGENDARY" => ItemQuality::Legendary,
            "ARTIFACT" => ItemQuality::Artifact,
            "HEIRLOOM" => ItemQuality::Heirloom,
            _ => ItemQuality::Common,
        }
    }
}

/// One line of a tooltip that the upstream already rendered for us.
///
/// The game's own tooltip lines ("Requires Level 71", "+1,020 Intellect",
/// "Use: ...") arrive as display strings. Re-deriving them from raw numbers
/// would mean re-implementing Blizzard's formatting rules and getting them
/// subtly wrong, so they are carried through as text and escaped at render.
pub type TooltipLine = String;

/// What a tooltip needs to draw, in the order the game draws it.
///
/// Fields are optional because item types differ: a consumable has an effect
/// and no stats, a weapon has stats and no effect, and plenty have neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemTooltip {
    pub item: ItemId,
    /// Which language these strings are in. Carried so the page can mark the
    /// tooltip with a `lang` attribute rather than lying about it.
    pub locale: Locale,
    pub name: String,
    pub quality: ItemQuality,
    /// "Item Level 80".
    pub item_level: Option<TooltipLine>,
    /// "Binds when picked up".
    pub binding: Option<TooltipLine>,
    /// "Unique" / "Unique-Equipped (3)".
    pub unique: Option<TooltipLine>,
    /// "Consumable" / "Flask", shown as the type line.
    pub item_class: Option<String>,
    pub item_subclass: Option<String>,
    /// The game does not draw the subclass for most consumables and reagents.
    /// Recorded rather than applied at parse time: the tooltip honours it, but
    /// a reagent card still wants to say "Herb".
    #[serde(default)]
    pub subclass_hidden: bool,
    /// "Requires Level 71".
    pub required_level: Option<TooltipLine>,
    /// "Requires Item Level: 80" -- a gem's own requirement, which the game
    /// draws under the effect rather than with the character requirements.
    ///
    /// Defaulted: tooltips cached before gems were tracked have no such field
    /// and must keep deserialising.
    #[serde(default)]
    pub required_item_level: Option<TooltipLine>,
    /// "+1,020 Intellect" and friends.
    pub stats: Vec<TooltipLine>,
    /// "Use: Restores 1,000,000 health." -- the lines that make a consumable
    /// worth buying, so the ones this feature exists for.
    pub effects: Vec<TooltipLine>,
    /// Italic flavour text.
    pub flavor: Option<TooltipLine>,
    /// "Reagent" note for crafting materials.
    pub crafting_reagent: Option<TooltipLine>,
    pub sell_price: Option<Copper>,
    /// The upstream's own "Sell Price:" label, already translated. Our
    /// fallback is English, which would otherwise be the one untranslated
    /// line in a Korean tooltip.
    pub sell_price_label: Option<String>,
    /// When the upstream was asked, so the UI can say how stale this is.
    pub fetched_at: Millis,
}

impl ItemTooltip {
    /// Cache key: language and item, with no region in it.
    ///
    /// Item text is identical whichever region's host serves it -- the four
    /// static namespaces return the same locale set with the same strings --
    /// so keying by region would store the same tooltip four times and make
    /// switching region re-fetch text we already hold.
    pub fn cache_key(item: ItemId, locale: Locale) -> String {
        // The version prefix retires cached entries whose shape predates a
        // field, instead of letting them render with a defaulted value until
        // the TTL expires.
        format!("item-tooltip:v3:{}:{}", locale.code(), item.get())
    }

    /// A minimal tooltip built from what we already know locally.
    ///
    /// Used when the upstream is unreachable or unconfigured: a name and a
    /// type line still beat an empty box.
    pub fn placeholder(item: ItemId, locale: Locale, name: impl Into<String>, at: Millis) -> Self {
        Self {
            item,
            locale,
            name: name.into(),
            quality: ItemQuality::Common,
            item_level: None,
            binding: None,
            unique: None,
            item_class: None,
            item_subclass: None,
            subclass_hidden: false,
            required_level: None,
            required_item_level: None,
            stats: Vec::new(),
            effects: Vec::new(),
            flavor: None,
            crafting_reagent: None,
            sell_price: None,
            sell_price_label: None,
            fetched_at: at,
        }
    }

    /// Whether anything beyond the name is known. A tooltip that is only a
    /// name is not worth opening a panel for.
    pub fn is_detailed(&self) -> bool {
        self.item_level.is_some()
            || self.required_level.is_some()
            || self.required_item_level.is_some()
            || !self.stats.is_empty()
            || !self.effects.is_empty()
            || self.flavor.is_some()
    }
}

/// One item in every language its region publishes.
///
/// The upstream returns all of them in a single response, so this is the
/// honest unit of a fetch: asking for one language and discarding the rest
/// would mean paying for the same request again the moment someone switches
/// language.
pub type LocalizedTooltips = Vec<(Locale, ItemTooltip)>;

/// Reads static item data: the description of an item, not its price.
///
/// Static data changes only when the game patches, which is what makes a long
/// cache lifetime correct here (unlike prices, which move hourly).
pub trait ItemDetailProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &'static str;

    /// Whether the adapter can actually reach upstream. An adapter without
    /// credentials is still constructible, so callers need to be able to ask.
    fn is_configured(&self) -> bool {
        true
    }

    /// Every language `region` publishes for `item`, from one request.
    fn tooltips(
        &self,
        region: Region,
        item: ItemId,
    ) -> impl Future<Output = AppResult<LocalizedTooltips>> + Send;
}
