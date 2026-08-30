//! What you asked to be told about.
//!
//! Alerts used to be a feed: the twenty most recent, on the consumables page,
//! for everybody including people who had never signed in. That is not an
//! alert. An alert is about something *you* said you cared about, it is about
//! now, and it is nobody else's business.
//!
//! So three rules, and each of them is a thing the old version got wrong:
//!
//! * **Signed in, or nothing.** A visitor who is not signed in has no
//!   followed items, so there is nothing to show them and nothing is shown.
//! * **Following something, or nothing.** An empty alerts box that explains
//!   why it is empty is worse than no box; a reader learns the page's layout
//!   from what is on it.
//! * **Today.** The table still holds every alert ever raised -- that is the
//!   price history's account of itself -- but "this was cheap on Tuesday" is
//!   not something anyone can act on.
//!
//! What counts as *cheap* is deliberately still not per user. That is a
//! property of the market, it lives in the collector's `AlertRule`, and a
//! percentile of a fortnight's history is not something a person should have
//! to tune. Following an item says which markets you want that judgement
//! applied to.

use std::collections::HashSet;

use app_core::market::{ItemId, Region};
use app_core::model::User;
use app_core::repo::{PriceRepository, ReadModelRepository, Store, WatchRepository};
use app_core::{AppError, Ports};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use cluster_core::Millis;
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{AlertRow, AlertsView, Layout, WatchRow, WatchlistView};

/// A day, in milliseconds. The window "today's alerts" means.
///
/// A rolling twenty-four hours rather than since-midnight-somewhere: the
/// server, the reader and the auction house are routinely in three different
/// time zones, and "since midnight" would have to pick one of them to be
/// wrong about.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Most alerts one day can put in front of a reader.
///
/// Generous -- a watchlist that fires this often is a watchlist to prune --
/// but it is the bound that stops a runaway collector turning a page into a
/// thousand-row table.
const TODAY_LIMIT: usize = 200;

#[derive(Template)]
#[template(path = "alerts.html")]
struct AlertsPage {
    layout: Layout,
    view: WatchlistView,
    /// Named `alerts` because `partials/alerts.html` is included here and in
    /// the index fragment, and an include reads the caller's scope.
    alerts: AlertsView,
}

#[derive(Template)]
#[template(path = "partials/alerts.html")]
pub struct AlertsFragment {
    pub alerts: AlertsView,
}

/// Today's alerts among the items this person follows.
///
/// Returns an invisible view for a visitor who is signed out or who follows
/// nothing, which is what every caller renders as "no box at all".
pub(crate) async fn today<E: Ports>(
    env: &E,
    user: Option<&User>,
    prefs: MarketPrefs,
    now: Millis,
) -> WebResult<AlertsView> {
    let Some(user) = user else {
        return Ok(AlertsView::default());
    };

    let watched: HashSet<(ItemId, Region)> = env
        .store()
        .watches()
        .watches(user.id)
        .await?
        .into_iter()
        .map(|w| (w.item, w.region))
        .collect();
    if watched.is_empty() {
        return Ok(AlertsView::default());
    }

    // One day's alerts, then filtered here rather than joined in SQL: the
    // price repository has no business knowing what a user is, and a day's
    // worth is small enough that the join would buy nothing.
    let since = Millis(now.get().saturating_sub(DAY_MS));
    let alerts = env
        .store()
        .prices()
        .alerts_since(since, TODAY_LIMIT)
        .await?;

    let index = env.catalogs().index();
    let rows = alerts
        .into_iter()
        .filter(|alert| watched.contains(&(alert.item, alert.region)))
        .map(|alert| AlertRow {
            item_id: alert.item.get(),
            name: index
                .get(&alert.item)
                .map(|(_, item)| item.name.clone())
                .unwrap_or_else(|| alert.item.to_string()),
            region: alert.region.to_string().to_uppercase(),
            severity: alert.severity.as_str(),
            current: alert.current.to_string(),
            baseline: alert.baseline.to_string(),
            discount_percent: alert.discount_percent,
            quantity: alert.quantity,
            when: crate::format::ago(
                prefs.locale,
                now.get().saturating_sub(alert.observed_at.get()),
            ),
        })
        .collect();

    Ok(AlertsView {
        visible: true,
        rows,
    })
}

/// `GET /partials/alerts` -- the summary's contents, fetched after the page.
///
/// Its own request so the index does not wait on it. The alerts are the one
/// thing on that page that needs the signed-in user resolved *and* a scan of
/// today's alerts, and neither of those should hold up the prices.
pub async fn fragment<E: Ports>(
    State(env): State<E>,
    Extension(prefs): Extension<MarketPrefs>,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let alerts = today(&env, user.as_ref(), prefs, env.now()).await?;
    page(&AlertsFragment { alerts }, prefs.locale)
}

/// `GET /wow/alerts`
pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    let now = env.now();

    let alerts = today(&env, user.as_ref(), prefs, now).await?;
    let view = match user.as_ref() {
        None => WatchlistView::default(),
        Some(user) => {
            let watches = env.store().watches().watches(user.id).await?;
            let index = env.catalogs().index();

            // One price read per region represented, not one per followed
            // item: a watchlist of forty items is at most four regions.
            let mut regions: Vec<Region> = watches.iter().map(|w| w.region).collect();
            regions.sort();
            regions.dedup();
            let mut latest = std::collections::HashMap::new();
            for region in regions {
                for market in env.store().read_model().commodities(region).await? {
                    latest.insert((market.key.item(), region), market.min_price);
                }
            }

            WatchlistView {
                signed_in: true,
                watches: watches
                    .iter()
                    .map(|watch| {
                        let entry = index.get(&watch.item);
                        WatchRow {
                            item_id: watch.item.get(),
                            name: entry
                                .map(|(_, item)| item.name.clone())
                                .unwrap_or_else(|| watch.item.to_string()),
                            region: watch.region.to_string().to_uppercase(),
                            region_code: watch.region.as_str(),
                            icon: entry.and_then(|(_, item)| item.icon_url()),
                            current: latest
                                .get(&(watch.item, watch.region))
                                .map(|price| price.to_string()),
                            href: format!("/wow/item/{}", watch.item.get()),
                        }
                    })
                    .collect(),
            }
        }
    };

    page(
        &AlertsPage {
            layout: crate::routes::pages::layout_for(
                &env,
                &headers,
                &csrf,
                prefs.locale,
                "Alerts",
                "/wow/alerts",
                &uri,
            )
            .await?,
            view,
            alerts,
        },
        prefs.locale,
    )
}

/// What the follow/unfollow button submits.
#[derive(Debug, Deserialize)]
pub struct WatchForm {
    csrf_token: String,
    item_id: u32,
    region: String,
    /// `true` to follow, `false` to stop.
    watch: bool,
    /// Where to send the browser back to. Validated as a local path.
    #[serde(default)]
    back: String,
}

/// `POST /wow/alerts`
pub async fn toggle<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    axum::Form(form): axum::Form<WatchForm>,
) -> WebResult<Response> {
    // Signed out is *not found*, not unauthorized: there is no shared
    // watchlist to be forbidden from, and a 401 would invite guessing at one.
    let Some(user) = current_user(&env, &headers).await? else {
        return Err(AppError::NotFound.into());
    };
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let region = Region::parse(&form.region).ok_or(AppError::NotFound)?;
    let item = ItemId(form.item_id);
    // Only something this instance actually tracks. Otherwise a watchlist
    // could be filled with ids that will never have a price or a name.
    if !env.catalogs().index().contains_key(&item) {
        return Err(AppError::NotFound.into());
    }

    let watches = env.store().watches();
    if form.watch {
        watches.watch(user.id, item, region, env.now()).await?;
    } else {
        watches.unwatch(user.id, item, region).await?;
    }

    Ok(Redirect::to(&safe_return(&form.back)).into_response())
}

/// Where to send the browser after following an item.
///
/// A same-site path or nothing. The value arrives in a form field, and a form
/// field that becomes a `Location` is an open redirect unless it is checked:
/// `//evil.example` and `https://evil.example` are both absolute URLs a
/// browser will happily follow off this site.
fn safe_return(raw: &str) -> String {
    let ok = raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.contains('\\')
        && !raw.contains(char::is_control);
    if ok {
        raw.to_string()
    } else {
        "/wow/alerts".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_return_path_stays_on_this_site() {
        assert_eq!(safe_return("/wow/item/1234"), "/wow/item/1234");
        assert_eq!(safe_return("/wow/alerts?x=1"), "/wow/alerts?x=1");
    }

    /// Every one of these is a browser-followable absolute URL, or a way to
    /// smuggle a header break into a `Location`.
    #[test]
    fn anything_that_could_leave_the_site_falls_back() {
        for raw in [
            "//evil.example",
            "https://evil.example",
            "http://evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "javascript:alert(1)",
            "",
            "wow/alerts",
            "/wow\r\nSet-Cookie: a=b",
        ] {
            assert_eq!(safe_return(raw), "/wow/alerts", "accepted {raw:?}");
        }
    }

    /// The empty view is what a signed-out visitor and a visitor who follows
    /// nothing both get, and it must render as nothing at all.
    #[test]
    fn nobodys_alerts_are_invisible_rather_than_empty() {
        let view = AlertsView::default();
        assert!(!view.visible);
        assert_eq!(view.count(), 0);
    }
}
