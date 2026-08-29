//! Static item data: `GET /data/wow/item/{id}?namespace=static-{region}`.
//!
//! This is the tooltip source. Unlike the auction house it is a cheap
//! single-item call (1 against the request budget) against data that only
//! changes on a patch, so the caller caches it for days and this adapter stays
//! dumb.
//!
//! Two things about the payload shape drive the code below:
//!
//! * Localised strings arrive as `{"en_GB": "...", "de_DE": "..."}` unless a
//!   `locale` is requested. We deliberately do *not* request one: one
//!   unlocalised response contains *every* language Blizzard publishes -- the
//!   same twelve from every regional host -- so asking for a single locale
//!   would mean paying for the same request again the moment someone switches
//!   language.
//! * Blizzard already renders the tooltip lines ("Requires Level 71",
//!   "+1,020 Intellect"). Those display strings are carried through as text
//!   rather than re-derived from the raw numbers, because re-deriving them
//!   means re-implementing the game's formatting rules.

use std::collections::BTreeMap;

use app_core::error::{AppError, AppResult};
use app_core::item::{ItemDetailProvider, ItemQuality, ItemTooltip, LocalizedTooltips};
use app_core::locale::{ALL_LOCALES, Locale};
use app_core::market::{Copper, ItemId, Region};
use cluster_core::{Clock, Millis};
use serde::Deserialize;

use super::token::TokenSource;
use super::{BlizzardConfig, BlizzardCredentials};

pub struct BlizzardItems<C> {
    http: reqwest::Client,
    token: TokenSource<C>,
    clock: C,
}

impl<C: Clock + Clone + 'static> BlizzardItems<C> {
    /// Keeps its own HTTP client and token.
    ///
    /// The token endpoint is hit roughly once a day per adapter, so sharing
    /// one with the auction client would save two requests a day at the cost
    /// of coupling the two adapters' lifetimes. Not worth it.
    pub fn new(
        config: BlizzardConfig,
        credentials: BlizzardCredentials,
        clock: C,
    ) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.item_timeout)
            .user_agent(config.user_agent.clone())
            .build()
            .map_err(|e| AppError::internal(format!("building HTTP client: {e}")))?;
        Ok(Self {
            token: TokenSource::new(http.clone(), config, credentials, clock.clone()),
            http,
            clock,
        })
    }
}

impl<C: Clock + Clone + 'static> ItemDetailProvider for BlizzardItems<C> {
    fn provider_name(&self) -> &'static str {
        "Blizzard Game Data API"
    }

    async fn tooltips(&self, region: Region, item: ItemId) -> AppResult<LocalizedTooltips> {
        let bearer = self.token.bearer().await?;
        let url = format!("{}/data/wow/item/{}", region.api_host(), item.get());

        let response = self
            .http
            .get(&url)
            .bearer_auth(bearer)
            .query(&[("namespace", region.static_namespace().as_str())])
            .send()
            .await
            .map_err(|e| AppError::Integration(format!("item request failed: {e}")))?;

        match response.status().as_u16() {
            200 => {}
            404 => return Err(AppError::NotFound),
            401 | 403 => {
                return Err(AppError::Integration(
                    "Battle.net rejected the credentials for the item endpoint".into(),
                ));
            }
            429 => {
                return Err(AppError::Integration(
                    "Battle.net rate limit reached on the item endpoint".into(),
                ));
            }
            status => {
                return Err(AppError::Integration(format!(
                    "item endpoint returned HTTP {status}"
                )));
            }
        }

        let payload: RawItem = response
            .json()
            .await
            .map_err(|e| AppError::Integration(format!("unexpected item payload: {e}")))?;

        // Every language, from the one response that already contains them
        // all -- whichever region's host answered.
        let now = self.clock.now();
        Ok(ALL_LOCALES
            .into_iter()
            .map(|locale| (locale, payload.to_tooltip(item, locale, now)))
            .collect())
    }
}

// --- wire format ---------------------------------------------------------
// Private to this module. Only `ItemTooltip` leaves it.

/// A string that may be plain (a `locale` was requested) or a map of locales
/// (none was). Both shapes appear in the wild depending on the endpoint.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Localized {
    Plain(String),
    Map(BTreeMap<String, String>),
}

impl Localized {
    /// The string in one language.
    ///
    /// Falls back to any other language rather than to nothing: a tooltip
    /// with one untranslated line still beats a blank one, and Blizzard does
    /// leave gaps in the smaller locales.
    fn text(&self, locale: Locale) -> Option<String> {
        let value = match self {
            Localized::Plain(s) => s.clone(),
            Localized::Map(map) => map
                .get(locale.code())
                .or_else(|| map.values().next())
                .cloned()?,
        };
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }
}

fn text(value: &Option<Localized>, locale: Locale) -> Option<String> {
    value.as_ref().and_then(|v| v.text(locale))
}

#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(default)]
    name: Option<Localized>,
    #[serde(default)]
    quality: Option<RawQuality>,
    #[serde(default)]
    item_class: Option<RawNamed>,
    #[serde(default)]
    item_subclass: Option<RawNamed>,
    #[serde(default)]
    sell_price: Option<u64>,
    #[serde(default)]
    preview_item: Option<RawPreview>,
}

#[derive(Debug, Deserialize)]
struct RawQuality {
    #[serde(default)]
    r#type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawNamed {
    #[serde(default)]
    name: Option<Localized>,
}

/// The interesting half of the payload: everything the game shows in the
/// tooltip proper.
#[derive(Debug, Deserialize)]
struct RawPreview {
    #[serde(default)]
    name: Option<Localized>,
    #[serde(default)]
    quality: Option<RawQuality>,
    #[serde(default)]
    binding: Option<RawNamed>,
    /// Set on items whose subclass the game does not draw -- most
    /// consumables. Honouring it is what keeps this a tooltip rather than a
    /// data dump.
    #[serde(default)]
    is_subclass_hidden: bool,
    #[serde(default)]
    unique_equipped: Option<Localized>,
    #[serde(default)]
    level: Option<RawDisplay>,
    #[serde(default)]
    requirements: Option<RawRequirements>,
    #[serde(default)]
    stats: Vec<RawStat>,
    #[serde(default)]
    spells: Vec<RawSpell>,
    #[serde(default)]
    description: Option<Localized>,
    #[serde(default)]
    crafting_reagent: Option<Localized>,
    #[serde(default)]
    sell_price: Option<RawSellPrice>,
    /// Gems only. Their effect lives here rather than in `stats` or `spells`.
    #[serde(default)]
    gem_properties: Option<RawGemProperties>,
}

/// What a gem does, and what it may be socketed into.
#[derive(Debug, Deserialize)]
struct RawGemProperties {
    /// "+13 Critical Strike", already rendered and already localised.
    #[serde(default)]
    effect: Option<Localized>,
    #[serde(default)]
    min_item_level: Option<RawDisplay>,
}

#[derive(Debug, Deserialize)]
struct RawDisplay {
    #[serde(default)]
    display_string: Option<Localized>,
}

#[derive(Debug, Deserialize)]
struct RawRequirements {
    #[serde(default)]
    level: Option<RawDisplay>,
}

#[derive(Debug, Deserialize)]
struct RawStat {
    #[serde(default)]
    display: Option<RawStatDisplay>,
}

#[derive(Debug, Deserialize)]
struct RawStatDisplay {
    #[serde(default)]
    display_string: Option<Localized>,
}

#[derive(Debug, Deserialize)]
struct RawSpell {
    #[serde(default)]
    description: Option<Localized>,
}

#[derive(Debug, Deserialize)]
struct RawSellPrice {
    #[serde(default)]
    value: u64,
    #[serde(default)]
    display_strings: Option<RawSellPriceStrings>,
}

#[derive(Debug, Deserialize)]
struct RawSellPriceStrings {
    /// "Sell Price:", already in the right language.
    #[serde(default)]
    header: Option<Localized>,
}

impl RawItem {
    /// Render one language out of the payload. Called once per locale the
    /// region publishes, which is why it borrows rather than consumes.
    fn to_tooltip(&self, item: ItemId, locale: Locale, fetched_at: Millis) -> ItemTooltip {
        let preview = self.preview_item.as_ref();

        // The preview block is the authoritative one where both exist: it is
        // what the game renders.
        let name = preview
            .and_then(|p| text(&p.name, locale))
            .or_else(|| text(&self.name, locale))
            .unwrap_or_else(|| format!("Item {}", item.get()));

        let quality = preview
            .and_then(|p| p.quality.as_ref())
            .or(self.quality.as_ref())
            .and_then(|q| q.r#type.as_deref())
            .map(ItemQuality::parse)
            .unwrap_or_default();

        // A zero sell price means "cannot be sold", which is not a line the
        // game draws.
        let sell_price = preview
            .and_then(|p| p.sell_price.as_ref().map(|s| s.value))
            .or(self.sell_price)
            .filter(|v| *v > 0)
            .map(Copper);

        let mut tooltip = ItemTooltip {
            item,
            locale,
            name,
            quality,
            item_level: None,
            binding: None,
            unique: None,
            item_class: self.item_class.as_ref().and_then(|c| text(&c.name, locale)),
            item_subclass: self
                .item_subclass
                .as_ref()
                .and_then(|c| text(&c.name, locale)),
            subclass_hidden: false,
            required_level: None,
            required_item_level: None,
            stats: Vec::new(),
            effects: Vec::new(),
            flavor: None,
            crafting_reagent: None,
            sell_price,
            sell_price_label: preview
                .and_then(|p| p.sell_price.as_ref())
                .and_then(|s| s.display_strings.as_ref())
                .and_then(|d| text(&d.header, locale)),
            fetched_at,
        };

        if let Some(preview) = preview {
            tooltip.item_level = preview
                .level
                .as_ref()
                .and_then(|l| text(&l.display_string, locale));
            tooltip.binding = preview.binding.as_ref().and_then(|b| text(&b.name, locale));
            tooltip.unique = text(&preview.unique_equipped, locale);
            tooltip.required_level = preview
                .requirements
                .as_ref()
                .and_then(|r| r.level.as_ref())
                .and_then(|l| text(&l.display_string, locale));
            tooltip.stats = preview
                .stats
                .iter()
                .filter_map(|s| {
                    s.display
                        .as_ref()
                        .and_then(|d| text(&d.display_string, locale))
                })
                .collect();
            tooltip.effects = preview
                .spells
                .iter()
                .filter_map(|s| text(&s.description, locale))
                .collect();
            // A gem carries no spells and no stats: what it does is one line
            // inside `gem_properties`, and without it a gem tooltip is a name
            // and nothing else.
            if let Some(gem) = preview.gem_properties.as_ref() {
                if let Some(effect) = text(&gem.effect, locale) {
                    tooltip.effects.push(effect);
                }
                tooltip.required_item_level = gem
                    .min_item_level
                    .as_ref()
                    .and_then(|l| text(&l.display_string, locale));
            }
            tooltip.flavor = text(&preview.description, locale);
            tooltip.crafting_reagent = text(&preview.crafting_reagent, locale);
            tooltip.subclass_hidden = preview.is_subclass_hidden;
        }

        tooltip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real response for a rank-3 flask: enough shape to prove
    /// the mapping, including the localised-map form and the preview block
    /// taking precedence.
    const FLASK: &str = r#"{
        "id": 212283,
        "name": {"en_GB": "Flask of Alchemical Chaos", "de_DE": "Fläschchen der alchemistischen Chaos"},
        "quality": {"type": "EPIC", "name": {"en_GB": "Epic"}},
        "level": 80,
        "item_class": {"name": {"en_GB": "Consumable"}},
        "item_subclass": {"name": {"en_GB": "Flask"}},
        "sell_price": 12500,
        "preview_item": {
            "name": {"en_GB": "Flask of Alchemical Chaos",
                     "de_DE": "Fläschchen der alchemistischen Chaos"},
            "quality": {"type": "EPIC"},
            "binding": {"type": "NONE", "name": {"en_GB": "Binds when picked up"}},
            "level": {"value": 80, "display_string": {"en_GB": "Item Level 80"}},
            "requirements": {"level": {"value": 71, "display_string": {"en_GB": "Requires Level 71"}}},
            "stats": [{"type": {"type": "INTELLECT"}, "value": 1020,
                       "display": {"display_string": {"en_GB": "+1,020 Intellect"}}}],
            "spells": [{"spell": {"id": 431971},
                        "description": {"en_GB": "Use: Grants a random secondary stat."}}],
            "description": {"en_GB": "Smells faintly of ozone."},
            "sell_price": {"value": 12500,
                           "display_strings": {"header": {"en_GB": "Sell Price:",
                                                          "de_DE": "Verkaufspreis:"}}}
        }
    }"#;

    /// Trimmed from a real gem response. A gem carries no `stats` and no
    /// `spells`: everything it does is inside `gem_properties`, which is why
    /// this shape needs a test of its own.
    const GEM: &str = r#"{
        "id": 240903,
        "name": {"en_GB": "Flawless Deadly Garnet", "es_ES": "Granate mortal impecable"},
        "quality": {"type": "RARE"},
        "item_class": {"name": {"en_GB": "Gem"}},
        "item_subclass": {"name": {"en_GB": "Critical Strike"}},
        "preview_item": {
            "name": {"en_GB": "Flawless Deadly Garnet"},
            "quality": {"type": "RARE"},
            "gem_properties": {
                "min_item_level": {"value": 80,
                                   "display_string": {"en_GB": "Requires Item Level: 80",
                                                      "es_ES": "Requiere nivel de objeto: 80"}},
                "effect": {"en_GB": "+13 Critical Strike", "es_ES": "+13 p. de golpe crítico"}
            }
        }
    }"#;

    fn parse(json: &str) -> ItemTooltip {
        parse_as(json, Locale::EnGb)
    }

    fn parse_as(json: &str, locale: Locale) -> ItemTooltip {
        serde_json::from_str::<RawItem>(json)
            .expect("payload parses")
            .to_tooltip(ItemId(212283), locale, Millis(1_000))
    }

    #[test]
    fn maps_a_full_payload() {
        let tooltip = parse(FLASK);
        assert_eq!(tooltip.name, "Flask of Alchemical Chaos");
        assert_eq!(tooltip.quality, ItemQuality::Epic);
        assert_eq!(tooltip.item_level.as_deref(), Some("Item Level 80"));
        assert_eq!(tooltip.required_level.as_deref(), Some("Requires Level 71"));
        assert_eq!(tooltip.stats, ["+1,020 Intellect"]);
        assert_eq!(tooltip.effects, ["Use: Grants a random secondary stat."]);
        assert_eq!(tooltip.flavor.as_deref(), Some("Smells faintly of ozone."));
        assert_eq!(tooltip.item_subclass.as_deref(), Some("Flask"));
        assert_eq!(tooltip.sell_price, Some(Copper(12500)));
        assert_eq!(tooltip.sell_price_label.as_deref(), Some("Sell Price:"));
        assert!(tooltip.is_detailed());
    }

    /// Without `gem_properties` a gem tooltip is a name and nothing else --
    /// which is exactly what it rendered as before this was mapped.
    #[test]
    fn maps_what_a_gem_does() {
        let tooltip = parse(GEM);
        assert_eq!(tooltip.effects, ["+13 Critical Strike"]);
        assert_eq!(
            tooltip.required_item_level.as_deref(),
            Some("Requires Item Level: 80")
        );
        assert!(tooltip.is_detailed());

        let es = parse_as(GEM, Locale::EsEs);
        assert_eq!(es.effects, ["+13 p. de golpe crítico"]);
    }

    #[test]
    fn survives_a_bare_payload() {
        // Every optional block missing: unknown items must still render a
        // name rather than fail the request.
        let tooltip = parse(r#"{"id": 1}"#);
        assert_eq!(tooltip.name, "Item 212283");
        assert_eq!(tooltip.quality, ItemQuality::Common);
        assert!(!tooltip.is_detailed());
        assert_eq!(tooltip.sell_price, None);
    }

    #[test]
    fn accepts_a_plain_string_name() {
        // What comes back when a `locale` *is* requested.
        let tooltip = parse(r#"{"id": 1, "name": "Healing Potion", "quality": {"type": "RARE"}}"#);
        assert_eq!(tooltip.name, "Healing Potion");
        assert_eq!(tooltip.quality, ItemQuality::Rare);
    }

    #[test]
    fn renders_the_requested_language() {
        let de = parse_as(FLASK, Locale::DeDe);
        assert_eq!(de.name, "Fläschchen der alchemistischen Chaos");
        assert_eq!(de.locale, Locale::DeDe);

        assert_eq!(de.sell_price_label.as_deref(), Some("Verkaufspreis:"));

        let en = parse_as(FLASK, Locale::EnGb);
        assert_eq!(en.name, "Flask of Alchemical Chaos");
    }

    #[test]
    fn falls_back_to_another_language_rather_than_a_blank_line() {
        // Blizzard leaves gaps in the smaller locales. A German tooltip with
        // one English line beats a German tooltip with a hole in it.
        let it = parse_as(FLASK, Locale::ItIt);
        assert_eq!(it.locale, Locale::ItIt);
        assert_eq!(it.flavor.as_deref(), Some("Smells faintly of ozone."));
    }

    #[test]
    fn records_a_hidden_subclass_without_discarding_it() {
        // The game shows "Consumable" and not "Flasks & Phials" for these --
        // but the reagent cards still want the material type, so the value is
        // kept and the flag carries the game's intent.
        let tooltip = parse(
            r#"{"id": 1, "item_class": {"name": "Consumable"},
                 "item_subclass": {"name": "Flasks & Phials"},
                 "preview_item": {"is_subclass_hidden": true}}"#,
        );
        assert_eq!(tooltip.item_class.as_deref(), Some("Consumable"));
        assert_eq!(tooltip.item_subclass.as_deref(), Some("Flasks & Phials"));
        assert!(tooltip.subclass_hidden);
    }

    #[test]
    fn drops_a_zero_sell_price() {
        let tooltip = parse(r#"{"id": 1, "sell_price": 0}"#);
        assert_eq!(tooltip.sell_price, None);
    }
}
