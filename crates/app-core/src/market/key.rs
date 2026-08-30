//! What identifies a market.
//!
//! A statistic is always *about* something, and until now that something was
//! assembled differently by every caller: a `(region, item)` tuple here, a
//! `(region, realm_id, variant)` key there, a `HashMap<ItemId, Vec<&Sample>>`
//! somewhere else, and a page that grouped on the track by re-deriving it from
//! a comma-separated bonus list. Each of those is correct on its own and none
//! of them is the same key, which is exactly the problem the specification's
//! "typed, and not a string assembled differently by each caller" is about.
//!
//! Three kinds of market, because there are three kinds of auction house
//! behind them:
//!
//! ```text
//! Commodity  -> item + region + rank      one price for a whole region
//! Recipe     -> item + region + realm     one price per connected realm
//! BoE        -> item + region + realm + track
//! ```
//!
//! The rank is part of a commodity key even though a rank already has its own
//! item id. It is not redundant to a reader: "Algari Healing Potion rank 3" is
//! how a player names the market, and a key that could only say `212265` makes
//! every label a lookup. It *is* redundant to the data, and
//! [`MarketKey::commodity`] is what keeps the two from disagreeing.
//!
//! Nothing here reduces anything. This is identity, so that the reductions
//! that arrive in Phase 2 have something stable to be filed under -- including
//! after a patch renumbers a bonus id, which changes what a track is called
//! and must not change which market a price belonged to.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::catalog::Track;
use super::{ItemId, RealmId, Region};

/// One market: the thing a price, a percentile and a chart are all about.
///
/// Ordered, because a list of markets has to have one order and "whatever the
/// database returned" is not one. The order is the encoding's order: kind,
/// then region, then realm, then item, then the discriminant within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "market", rename_all = "snake_case")]
pub enum MarketKey {
    /// Stackable and region-wide: one price for all of EU.
    Commodity {
        region: Region,
        item: ItemId,
        /// The quality rank, 1-based. `1` when the item has no ranks.
        rank: u8,
    },
    /// A recipe on one connected realm. A recipe has one version of itself,
    /// so there is nothing below the realm.
    Recipe {
        region: Region,
        realm: RealmId,
        item: ItemId,
    },
    /// One upgrade track of one bind-on-equip piece, on one connected realm.
    ///
    /// The track is the market and the rank within it is not (CLAUDE.md §8).
    /// `track` is `None` for a listing whose bonus list carries no track this
    /// catalogue knows -- an unresolved market is still a market, and dropping
    /// it would lose the history that a later catalogue sync could name.
    Boe {
        region: Region,
        realm: RealmId,
        item: ItemId,
        track: Option<Track>,
    },
}

impl MarketKey {
    pub fn commodity(region: Region, item: ItemId, rank: u8) -> MarketKey {
        MarketKey::Commodity {
            region,
            item,
            // A rank is 1-based and a zero means the caller did not know. Say
            // 1 rather than carry a rank that cannot be displayed.
            rank: rank.max(1),
        }
    }

    pub fn recipe(region: Region, realm: RealmId, item: ItemId) -> MarketKey {
        MarketKey::Recipe {
            region,
            realm,
            item,
        }
    }

    pub fn boe(region: Region, realm: RealmId, item: ItemId, track: Option<Track>) -> MarketKey {
        MarketKey::Boe {
            region,
            realm,
            item,
            track,
        }
    }

    pub const fn region(self) -> Region {
        match self {
            MarketKey::Commodity { region, .. }
            | MarketKey::Recipe { region, .. }
            | MarketKey::Boe { region, .. } => region,
        }
    }

    pub const fn item(self) -> ItemId {
        match self {
            MarketKey::Commodity { item, .. }
            | MarketKey::Recipe { item, .. }
            | MarketKey::Boe { item, .. } => item,
        }
    }

    /// The connected realm, or `None` for a region-wide commodity market.
    pub const fn realm(self) -> Option<RealmId> {
        match self {
            MarketKey::Commodity { .. } => None,
            MarketKey::Recipe { realm, .. } | MarketKey::Boe { realm, .. } => Some(realm),
        }
    }

    /// Whether this market is region-wide.
    ///
    /// The same question `ItemKind::is_commodity` answers about a *kind*, from
    /// the market's own side.
    pub const fn is_commodity(self) -> bool {
        matches!(self, MarketKey::Commodity { .. })
    }

    /// The one-letter tag the encoding leads with.
    const fn tag(self) -> char {
        match self {
            MarketKey::Commodity { .. } => 'c',
            MarketKey::Recipe { .. } => 'r',
            MarketKey::Boe { .. } => 'b',
        }
    }
}

/// The canonical encoding.
///
/// Stable, and meant to stay so: it is what a read-model row, a cache key and
/// a work partition will be filed under from Phase 2 onwards, so a change to
/// it is a migration rather than an edit. Colons and lower case, because it
/// goes in places -- a URL, a cache key, a log line -- where a delimiter that
/// needs escaping is a bug waiting for the first item with a comma in it.
///
/// ```text
/// c:eu:212265:3          a rank-3 commodity in the EU house
/// r:eu:1403:271441       a recipe on Sargeras
/// b:eu:1403:271441:hero  the Hero track of a BoE on Sargeras
/// b:eu:1403:271441:-     the same piece, on a track no catalogue names
/// ```
impl fmt::Display for MarketKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:", self.tag(), self.region().as_str())?;
        match *self {
            MarketKey::Commodity { item, rank, .. } => write!(f, "{}:{rank}", item.get()),
            MarketKey::Recipe { realm, item, .. } => write!(f, "{realm}:{}", item.get()),
            MarketKey::Boe {
                realm, item, track, ..
            } => write!(
                f,
                "{realm}:{}:{}",
                item.get(),
                match track {
                    // Not the empty string: a trailing delimiter with nothing
                    // after it is the shape that gets trimmed by something on
                    // the way past and comes back as a different key.
                    None => "-",
                    Some(track) => track.slug(),
                }
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a market key: {0}")]
pub struct BadMarketKey(String);

impl FromStr for MarketKey {
    type Err = BadMarketKey;

    fn from_str(raw: &str) -> Result<MarketKey, BadMarketKey> {
        let bad = || BadMarketKey(raw.to_string());
        let mut parts = raw.split(':');
        let tag = parts.next().ok_or_else(bad)?;
        let region = Region::parse(parts.next().ok_or_else(bad)?).ok_or_else(bad)?;

        let key = match tag {
            "c" => {
                let item = ItemId(parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?);
                let rank: u8 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
                if rank == 0 {
                    return Err(bad());
                }
                MarketKey::Commodity { region, item, rank }
            }
            "r" => {
                let realm = RealmId(parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?);
                let item = ItemId(parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?);
                MarketKey::Recipe {
                    region,
                    realm,
                    item,
                }
            }
            "b" => {
                let realm = RealmId(parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?);
                let item = ItemId(parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?);
                let track = match parts.next().ok_or_else(bad)? {
                    "-" => None,
                    slug => Some(Track::from_slug(slug).ok_or_else(bad)?),
                };
                MarketKey::Boe {
                    region,
                    realm,
                    item,
                    track,
                }
            }
            _ => return Err(bad()),
        };

        // Trailing anything is a different key that happens to start the same,
        // and accepting it would make two strings mean one market.
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(key)
    }
}
