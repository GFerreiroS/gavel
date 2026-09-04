//! WoW Token history, rendered from its own region-scoped archive.

use app_core::Ports;
use app_core::market::Point;
use app_core::repo::{Store, TokenPriceRepository};
use askama::Template;
use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::Html;

use crate::chart::{self, Series, Unit};
use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{Layout, PanelHead, TokenSummaryView};

#[derive(Template)]
#[template(path = "wow_token.html")]
struct WowTokenPage {
    layout: Layout,
    region: String,
    current: Option<String>,
    updated: Option<String>,
    observations: usize,
    chart: String,
}

pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let history = env.store().prices().history(prefs.region).await?;
    let current = history.last();
    let points: Vec<Point> = history
        .iter()
        .map(|sample| Point {
            at: sample.observed_at,
            price: sample.price,
            quantity: 0,
        })
        .collect();
    let chart = chart::line_chart_localised(
        prefs.locale,
        &[Series {
            label: "WoW Token",
            points: &points,
            slot: 0,
            show_stock: false,
        }],
        Unit::Gold,
        crate::i18n::translate(
            prefs.locale,
            "Not enough history yet — the chart appears after a few collections.",
        ),
    );
    let user = current_user(&env, &headers).await?;

    page(
        &WowTokenPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                crate::i18n::translate(prefs.locale, "WoW Token"),
                "/wow/token",
                &uri,
                user.as_ref(),
                csrf.masked(),
            ),
            region: prefs.region.to_string().to_uppercase(),
            current: current.map(|sample| sample.price.to_string()),
            updated: current.map(|sample| {
                crate::format::ago(prefs.locale, env.now().since(sample.observed_at))
            }),
            observations: history.len(),
            chart,
        },
        prefs.locale,
    )
}

/// The token page and the auction-house landing page describe the same latest
/// regional observation. Keeping that wording here prevents one route from
/// calling an empty history a zero while the other correctly calls it absent.
pub(crate) async fn summary<E: Ports>(env: &E, prefs: MarketPrefs) -> WebResult<TokenSummaryView> {
    let history = env.store().prices().history(prefs.region).await?;
    let current = history.last();
    let has_price = current.is_some();
    let updated = current.map_or_else(
        || crate::i18n::translate(prefs.locale, "not applicable").to_string(),
        |sample| crate::format::ago(prefs.locale, env.now().since(sample.observed_at)),
    );

    Ok(TokenSummaryView {
        panel: PanelHead {
            question: "What is the current WoW Token price for this region?",
            window: "latest collection".into(),
            units: "gold",
            coverage: Some(
                crate::i18n::translate(
                    prefs.locale,
                    if has_price {
                        "one regional token price"
                    } else {
                        "no token price collected"
                    },
                )
                .to_string(),
            ),
            freshness: Some(updated.clone()),
        },
        region: prefs.region.to_string().to_uppercase(),
        has_price,
        current: current
            .map(|sample| sample.price.to_string())
            .unwrap_or_default(),
        updated,
        observations: history.len(),
        href: format!("/wow/token?region={}", prefs.region.as_str()),
    })
}
