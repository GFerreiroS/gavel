//! Per-realm auction houses, and the list of realms themselves.
//!
//! `GET /data/wow/connected-realm/{id}/auctions?namespace=dynamic-{region}`
//!
//! Where the commodity endpoint is one large request per region, this is one
//! per connected realm — measured at ~20 MB and ~108,000 auctions for a FULL
//! realm, of which a few hundred are tracked. Two consequences:
//!
//! * `If-Modified-Since` is checked **per realm**. Realms regenerate on their
//!   own schedules, so a region-wide timestamp would either re-fetch realms
//!   that had not moved or skip ones that had.
//! * Filtering happens while parsing. The 99% we discard never becomes a
//!   `GearListing`, which keeps a 20 MB response from turning into a 20 MB
//!   allocation of things nobody asked for.
//!
//! Gear is priced by `buyout`, not `unit_price`: one item, one price. An
//! auction with only a bid is carried at the bid — it is still a price
//! someone can pay, and dropping those would thin out the cheap end.

use std::collections::BTreeSet;

use app_core::error::{AppError, AppResult};
use app_core::market::{
    Copper, GearListing, ItemId, Realm, RealmAuctionProvider, RealmId, RealmSnapshot, Region,
};
use cluster_core::{Clock, Millis};
use serde::Deserialize;

use super::token::TokenSource;
use super::{BlizzardConfig, BlizzardCredentials};

pub struct BlizzardRealms<C> {
    http: reqwest::Client,
    token: TokenSource<C>,
}

impl<C: Clock + Clone + 'static> BlizzardRealms<C> {
    pub fn new(
        config: BlizzardConfig,
        credentials: BlizzardCredentials,
        clock: C,
    ) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(config.user_agent.clone())
            .build()
            .map_err(|e| AppError::internal(format!("building HTTP client: {e}")))?;
        Ok(Self {
            token: TokenSource::new(http.clone(), config, credentials, clock),
            http,
        })
    }
}

impl<C: Clock + Clone + 'static> RealmAuctionProvider for BlizzardRealms<C> {
    fn provider_name(&self) -> &'static str {
        "Blizzard Game Data API"
    }

    async fn auctions(
        &self,
        region: Region,
        realm: RealmId,
        wanted: &[ItemId],
        if_modified_since: Option<Millis>,
    ) -> AppResult<RealmSnapshot> {
        let bearer = self.token.bearer().await?;
        let url = format!(
            "{}/data/wow/connected-realm/{}/auctions",
            region.api_host(),
            realm.get()
        );

        let mut request = self
            .http
            .get(&url)
            .bearer_auth(bearer)
            .query(&[("namespace", region.namespace().as_str())]);

        if let Some(since) = if_modified_since {
            request = request.header(reqwest::header::IF_MODIFIED_SINCE, http_date(since));
        }

        let response = request.send().await.map_err(|e| {
            AppError::Integration(format!("realm {realm} auctions request failed: {e}"))
        })?;

        match response.status().as_u16() {
            200 => {}
            304 => return Ok(RealmSnapshot::NotModified),
            // A realm that has been merged away, or a typo in the config. Not
            // fatal: the other realms still collect.
            404 => {
                return Err(AppError::Integration(format!(
                    "no auction house for connected realm {realm} in {region}"
                )));
            }
            401 | 403 => {
                return Err(AppError::Integration(
                    "Battle.net rejected the credentials for the auctions endpoint".into(),
                ));
            }
            429 => {
                return Err(AppError::Integration(
                    "Battle.net rate limit reached on the realm auctions endpoint".into(),
                ));
            }
            status => {
                return Err(AppError::Integration(format!(
                    "realm {realm} auctions returned HTTP {status}"
                )));
            }
        }

        let generated_at = response
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_http_date);

        let payload: AuctionsResponse = response.json().await.map_err(|e| {
            AppError::Integration(format!("unexpected realm {realm} auctions payload: {e}"))
        })?;

        let wanted: BTreeSet<u32> = wanted.iter().map(|i| i.get()).collect();
        let total = payload.auctions.len();
        let listings: Vec<GearListing> = payload
            .auctions
            .into_iter()
            .filter(|a| wanted.contains(&a.item.id))
            .filter_map(|a| {
                let price = a.buyout.or(a.bid).filter(|p| *p > 0)?;
                let mut bonus_ids = a.item.bonus_lists;
                bonus_ids.sort_unstable();
                bonus_ids.dedup();
                Some(GearListing {
                    item: ItemId(a.item.id),
                    price: Copper(price),
                    bonus_ids,
                })
            })
            .collect();

        tracing::info!(
            region = %region,
            realm = %realm,
            scanned = total,
            kept = listings.len(),
            "realm auction snapshot fetched"
        );

        Ok(RealmSnapshot::Fresh {
            generated_at: generated_at.unwrap_or_else(|| {
                tracing::warn!(realm = %realm, "realm auctions response had no Last-Modified");
                Millis(0)
            }),
            listings,
        })
    }

    async fn realms(&self, region: Region, wanted: &[RealmId]) -> AppResult<Vec<Realm>> {
        let bearer = self.token.bearer().await?;

        // Empty means every realm in the region. The index is one request and
        // gives ids as URLs; the names come from a small request each.
        let ids: Vec<u32> = if wanted.is_empty() {
            let index: RealmIndex = self
                .http
                .get(format!(
                    "{}/data/wow/connected-realm/index",
                    region.api_host()
                ))
                .bearer_auth(&bearer)
                .query(&[("namespace", region.namespace().as_str())])
                .send()
                .await
                .map_err(|e| AppError::Integration(format!("connected realm index failed: {e}")))?
                .json()
                .await
                .map_err(|e| AppError::Integration(format!("unexpected realm index: {e}")))?;
            index
                .connected_realms
                .iter()
                .filter_map(|r| r.id())
                .collect()
        } else {
            wanted.iter().map(|r| r.get()).collect()
        };

        let mut realms = Vec::new();
        for id in ids {
            // One small request per configured realm, at startup only.
            let detail: ConnectedRealm = self
                .http
                .get(format!(
                    "{}/data/wow/connected-realm/{id}",
                    region.api_host()
                ))
                .bearer_auth(&bearer)
                .query(&[("namespace", region.namespace().as_str())])
                .send()
                .await
                .map_err(|e| AppError::Integration(format!("connected realm {id} failed: {e}")))?
                .json()
                .await
                .map_err(|e| AppError::Integration(format!("unexpected realm {id}: {e}")))?;

            realms.push(Realm {
                id: RealmId(id),
                region,
                name: detail.display_name(id),
                // What the upstream says exists; whether we collect it is our
                // decision, and it lives in the store.
                enabled: true,
            });
        }
        Ok(realms)
    }
}

fn http_date(at: Millis) -> String {
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_millis(at.get()))
}

fn parse_http_date(value: &str) -> Option<Millis> {
    let parsed = httpdate::parse_http_date(value).ok()?;
    let since = parsed.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(Millis(since.as_millis() as u64))
}

// --- wire format ---------------------------------------------------------
// Private. Gear auctions are one item at a time: `buyout` and `bid` apply,
// `unit_price` does not.

#[derive(Debug, Deserialize)]
struct AuctionsResponse {
    #[serde(default)]
    auctions: Vec<RawAuction>,
}

#[derive(Debug, Deserialize)]
struct RawAuction {
    item: RawItem,
    #[serde(default)]
    bid: Option<u64>,
    #[serde(default)]
    buyout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    id: u32,
    /// The only description of *which version* of the item this is. Absent on
    /// gear with no upgrades, which is a variant in its own right.
    #[serde(default)]
    bonus_lists: Vec<u32>,
}

#[derive(Debug, Deserialize)]
struct RealmIndex {
    #[serde(default)]
    connected_realms: Vec<RealmLink>,
}

#[derive(Debug, Deserialize)]
struct RealmLink {
    href: String,
}

impl RealmLink {
    /// The index gives URLs, not ids: `.../connected-realm/1403?namespace=...`
    fn id(&self) -> Option<u32> {
        self.href
            .rsplit('/')
            .next()?
            .split('?')
            .next()?
            .parse()
            .ok()
    }
}

#[derive(Debug, Deserialize)]
struct ConnectedRealm {
    #[serde(default)]
    realms: Vec<RealmName>,
}

impl ConnectedRealm {
    /// "Dentarg, Tarren Mill": a connected realm is several realms sharing one
    /// auction house, and players know it by whichever name they play on.
    fn display_name(&self, id: u32) -> String {
        let names: Vec<&str> = self
            .realms
            .iter()
            .filter_map(|r| r.name.text())
            .collect::<Vec<_>>();
        if names.is_empty() {
            return format!("Realm {id}");
        }
        names.join(", ")
    }
}

#[derive(Debug, Deserialize)]
struct RealmName {
    name: LocalizedName,
}

/// Realm names come back as a locale map, or as a plain string when a locale
/// was requested. Realm names are not translated, so any of them will do.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LocalizedName {
    Plain(String),
    Map(std::collections::BTreeMap<String, String>),
}

impl LocalizedName {
    fn text(&self) -> Option<&str> {
        match self {
            LocalizedName::Plain(s) => Some(s.as_str()),
            LocalizedName::Map(map) => map
                .get("en_GB")
                .or_else(|| map.get("en_US"))
                .or_else(|| map.values().next())
                .map(|s| s.as_str()),
        }
    }
}
