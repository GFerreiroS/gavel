//! What this instance collects.
//!
//! Everything the tracker follows used to be a deployment decision: a
//! command-line flag, a restart, a redeploy. That is the wrong place for it.
//! The person watching the prices is the person who knows that Sargeras is
//! worth following and that thirty low-population realms are not, and they
//! should not need shell access to say so.
//!
//! Two kinds of switch:
//!
//! * **Categories** — consumables, reagents, enchants, gems, gear, recipes.
//!   Turning one off stops collecting it; its history stays readable.
//! * **Realms** — 184 of them. Gear and recipes are fetched per realm at
//!   roughly 20 MB each, so this is the switch that decides what a cycle
//!   costs.
//!
//! Nothing here deletes anything. A switch turned off stops a market growing;
//! it never takes away what has already been recorded, which is what makes
//! turning one off a safe thing to try.

use app_core::Ports;
use app_core::market::{ItemKind, Realm, RealmId, Region};
use app_core::repo::{RealmPriceRepository, SettingsRepository, Store};
use askama::Template;
use axum::Extension;
use axum::Form;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::{Html, Redirect};
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{AdminCategory, AdminRealm, AdminRegion, AdminView, Layout};

#[derive(Template)]
#[template(path = "admin.html")]
struct AdminPage {
    layout: Layout,
    admin: AdminView,
}

/// `GET /admin`
pub async fn page_handler<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(prefs): Extension<MarketPrefs>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> WebResult<Html<String>> {
    let user = current_user(&env, &headers).await?;
    // Not found rather than forbidden for a signed-out visitor: a page that
    // announces itself to everyone is an invitation to guess a password.
    let Some(user) = user.filter(|u| u.is_admin) else {
        return Err(app_core::AppError::NotFound.into());
    };

    let disabled = env.store().settings().disabled().await?;
    let realms = env.store().realm_prices().realms().await?;

    let admin = AdminView {
        categories: ItemKind::ALL
            .into_iter()
            .map(|kind| AdminCategory {
                key: kind.as_str(),
                label: kind.label(),
                enabled: !disabled.iter().any(|name| name == kind.as_str()),
                per_realm: !kind.is_commodity(),
            })
            .collect(),
        regions: by_region(&realms),
        realms_enabled: realms.iter().filter(|r| r.enabled).count(),
        realms_total: realms.len(),
    };

    page(
        &AdminPage {
            layout: Layout::new(
                env.config(),
                prefs.locale,
                "Collection",
                "/admin",
                &uri,
                Some(user.username),
                csrf.0.clone(),
            ),
            admin,
        },
        prefs.locale,
    )
}

/// Realms grouped by region, in the order the picker shows them.
fn by_region(realms: &[Realm]) -> Vec<AdminRegion> {
    let mut regions: Vec<Region> = realms.iter().map(|r| r.region).collect();
    regions.sort();
    regions.dedup();
    regions
        .into_iter()
        .map(|region| AdminRegion {
            code: region.as_str(),
            label: region.to_string().to_uppercase(),
            realms: realms
                .iter()
                .filter(|r| r.region == region)
                .map(|realm| AdminRealm {
                    id: realm.id.get(),
                    name: realm.name.clone(),
                    enabled: realm.enabled,
                })
                .collect(),
            enabled: realms
                .iter()
                .filter(|r| r.region == region && r.enabled)
                .count(),
        })
        .collect()
}

/// What the form submits: one switch, and what to set it to.
///
/// A switch at a time rather than the whole page: a form of 184 checkboxes
/// posts nothing for the ones that are off, so a lost checkbox and a realm
/// deliberately turned off would look identical.
#[derive(Debug, Deserialize)]
pub struct Toggle {
    csrf_token: String,
    /// `category:gem` or `realm:eu:1403`.
    switch: String,
    enabled: bool,
}

/// `POST /admin`
pub async fn toggle<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    Form(form): Form<Toggle>,
) -> WebResult<Redirect> {
    let user = current_user(&env, &headers).await?;
    let Some(_) = user.filter(|u| u.is_admin) else {
        return Err(app_core::AppError::NotFound.into());
    };
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    match parse_switch(&form.switch) {
        Some(Switch::Category(kind)) => {
            env.store()
                .settings()
                .set_enabled(kind, form.enabled)
                .await?;
            tracing::info!(
                category = kind,
                enabled = form.enabled,
                "collection changed"
            );
        }
        Some(Switch::Realm(region, realm)) => {
            env.store()
                .realm_prices()
                .set_realm_enabled(region, realm, form.enabled)
                .await?;
            tracing::info!(%region, %realm, enabled = form.enabled, "realm collection changed");
        }
        None => return Err(app_core::AppError::NotFound.into()),
    }

    // Back to the page rather than a fragment: the counts at the top change
    // with every switch, and a partial swap would leave them stale.
    Ok(Redirect::to("/admin"))
}

enum Switch<'a> {
    Category(&'a str),
    Realm(Region, RealmId),
}

fn parse_switch(raw: &str) -> Option<Switch<'_>> {
    match raw.split_once(':')? {
        ("category", kind) => ItemKind::ALL
            .into_iter()
            .find(|k| k.as_str() == kind)
            .map(|k| Switch::Category(k.as_str())),
        ("realm", rest) => {
            let (region, id) = rest.split_once(':')?;
            Some(Switch::Realm(
                Region::parse(region)?,
                RealmId(id.parse().ok()?),
            ))
        }
        _ => None,
    }
}
