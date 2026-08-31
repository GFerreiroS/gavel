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
use app_core::market::catalog::{Catalog, CatalogStatus};
use app_core::market::{ItemKind, Realm, RealmId, Region};
use app_core::repo::{
    MarketEventRepository, RealmPriceRepository, ReleaseRepository, SettingsRepository, Store,
};
use askama::Template;
use axum::Extension;
use axum::Form;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::{Html, Redirect};
use cluster_core::Millis;
use serde::Deserialize;

use crate::csrf::Csrf;
use crate::error::WebResult;
use crate::i18n::filters;
use crate::prefs::MarketPrefs;
use crate::render::page;
use crate::session::current_user;
use crate::views::{
    AdminCategory, AdminLanguage, AdminMarket, AdminRegion, AdminRelease, AdminView, Layout,
};

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

    // Every event, the unchecked and the internal included. There is no public
    // route to this list; §7's operations gate is the one layer that keeps it
    // that way, rather than a filter in each handler.
    let events = env.store().market_events().recent(60).await?;

    let admin = AdminView {
        // Across the catalogues rather than within one. `Catalog::problems`
        // cannot see these: every catalogue involved is coherent on its own,
        // and it is the arrangement that is wrong.
        archive_problems: env.archive().problems(),
        events: events
            .iter()
            .map(|event| crate::views::AdminEvent {
                id: event.id.clone(),
                // `label`, not `as_str`: the machine word is the form's and the
                // column's, and a reader was being shown `raid_opening` in
                // both languages. Every label here is already in
                // `EXTERNAL_STRINGS` and already translated (§13).
                kind: event.kind.label(),
                title: event.title.clone(),
                when: event.starts_at.to_utc_string(),
                scope: super::item::scope_text(&event.scope, prefs.locale),
                provenance: event.provenance.as_str(),
                validation: event.validation.as_str(),
                visibility: event.visibility.as_str(),
                live: event.is_public(),
                // A catalogue or calendar event comes back at the next start,
                // so offering to delete it would be offering something that
                // does not stay done.
                removable: event.provenance == app_core::market::Provenance::Administrator,
            })
            .collect(),
        event_kinds: app_core::market::EventKind::ALL
            .into_iter()
            .map(|kind| (kind.as_str(), kind.label()))
            .collect(),
        releases: env
            .all_catalogs()
            .into_iter()
            .map(|catalog| release_view(&env, catalog))
            .collect(),
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
                Some(&user),
                csrf.masked(),
            ),
            admin,
        },
        prefs.locale,
    )
}

/// One catalogue as the administrator sees it.
///
/// Everything here is either the catalogue's own data or the deployment's
/// state for it. Nothing is a price: a `draft_ptr` catalogue has none, and the
/// point of this panel is to review what a catalogue *says* before it is
/// allowed to start collecting.
fn release_view<E: Ports>(env: &E, catalog: &Catalog) -> AdminRelease {
    let state = env.catalog_state(catalog);
    let problems = catalog.problems();
    // What pressing the button ends. §8 makes activation and archiving one
    // transaction, and a button that does two things while naming one of them
    // is a button somebody presses by accident.
    // Its *season*, not its expansion. Both catalogues of a rollover carry the
    // same expansion name -- that is what makes the archive show one expansion
    // -- so "activating this archives Midnight" named the thing they have in
    // common and told the reader nothing.
    let archives = env
        .active_catalog()
        .filter(|active| active.id != catalog.id)
        .map(|active| active.season_label());
    AdminRelease {
        archives,
        notes: catalog.notes.clone(),
        patches: {
            let mut patches: Vec<crate::views::AdminPatch> = catalog
                .patches
                .iter()
                .map(|patch| crate::views::AdminPatch {
                    patch: patch.patch.clone(),
                    name: patch.name.clone(),
                    started: patch.started.clone(),
                    tiers: catalog
                        .tiers_of_patch(&patch.patch)
                        .map(|tier| tier.name.clone())
                        .collect(),
                })
                .collect();
            patches.sort_by(|a, b| b.started.cmp(&a.started));
            patches
        },
        kinds: ItemKind::ALL
            .into_iter()
            .map(|kind| (kind.label(), catalog.of_kind(kind).count()))
            .filter(|(_, count)| *count > 0)
            .collect(),
        id: catalog.id.clone(),
        expansion: catalog.expansion.clone(),
        season: catalog.season_label(),
        state: state.as_str(),
        state_label: state_label(state),
        patch: catalog
            .patches
            .iter()
            .max_by_key(|p| p.started_at())
            .map(|p| p.patch.clone())
            .unwrap_or_default(),
        tier: catalog
            .current_tier()
            .map(|t| t.name.clone())
            .unwrap_or_default(),
        items: catalog.items.len(),
        catalog_version: catalog.catalog_version,
        activatable: !state.is_active() && problems.is_empty(),
        problems,
    }
}

/// The word a person reads for a state.
///
/// Source strings, translated by the template's `|t`. "PTR draft" rather than
/// `draft_ptr`: the machine word goes in the form, the reader gets English.
pub(crate) const fn state_label(state: CatalogStatus) -> &'static str {
    match state {
        CatalogStatus::DraftPtr => "PTR draft",
        CatalogStatus::Active => "Collecting",
        CatalogStatus::Archived => "Archived",
    }
}

/// `POST /admin/release`
///
/// Activating a catalogue archives whatever was active, in one transaction
/// (§8). Refused for a catalogue whose data does not hold together, because
/// "an administrator explicitly activates it after reviewing it" is only worth
/// anything if the review can say no.
pub async fn activate<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    headers: HeaderMap,
    Form(form): Form<Activate>,
) -> WebResult<Redirect> {
    let user = current_user(&env, &headers).await?;
    let Some(_) = user.filter(|u| u.is_admin) else {
        return Err(app_core::AppError::NotFound.into());
    };
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    // `by_id`, not `public_catalog`: this is the one place a `draft_ptr`
    // catalogue is addressable, which is the whole point of the panel.
    let Some(catalog) = env.catalogs().by_id(&form.catalog) else {
        return Err(app_core::AppError::NotFound.into());
    };
    let problems = catalog.problems();
    if !problems.is_empty() {
        tracing::warn!(
            catalog = %catalog.id,
            problems = ?problems,
            "refused to activate an incoherent catalogue"
        );
        return Err(app_core::AppError::NotFound.into());
    }

    let done = env
        .store()
        .releases()
        .activate(&catalog.id, env.now())
        .await?;
    tracing::info!(
        activated = %done.activated,
        archived = done.archived.as_deref().unwrap_or("none"),
        "catalogue activated"
    );

    // Read the whole picture back rather than patching the two rows we know
    // about: the database is what enforces "at most one active", so it is also
    // what should say what happened.
    let states = env.store().releases().releases().await?;
    env.releases()
        .replace(states.into_iter().map(|r| (r.catalog, r.state)));

    Ok(Redirect::to("/admin"))
}

/// What the activation form submits.
#[derive(Debug, Deserialize)]
pub struct Activate {
    csrf_token: String,
    catalog: String,
}

/// Realms by region, and within a region by the language they are played in.
///
/// Ordered by size, biggest language first: on EU that puts English, German
/// and French where a reader looks, and the two-realm languages at the end
/// rather than scattered through an alphabet.
fn by_region(realms: &[Realm]) -> Vec<AdminRegion> {
    let mut regions: Vec<Region> = realms.iter().map(|r| r.region).collect();
    regions.sort();
    regions.dedup();
    regions
        .into_iter()
        .map(|region| {
            let mine: Vec<&Realm> = realms.iter().filter(|r| r.region == region).collect();

            let mut tags: Vec<&str> = mine.iter().map(|r| r.locale.as_str()).collect();
            tags.sort_unstable();
            tags.dedup();

            let mut languages: Vec<AdminLanguage> = tags
                .into_iter()
                .map(|tag| {
                    // One box per auction house, ordered by the first realm
                    // in it: a player looking for "C'Thun" finds it under C,
                    // inside the box it shares with Dun Modr.
                    let mut markets: Vec<AdminMarket> = mine
                        .iter()
                        .filter(|r| r.locale == tag)
                        .map(|realm| market(realm))
                        .collect();
                    markets.sort_by(|a, b| a.names.cmp(&b.names));
                    AdminLanguage {
                        label: language_name(tag),
                        enabled: markets.iter().filter(|m| m.enabled).count(),
                        markets,
                    }
                })
                .collect();
            languages.sort_by(|a, b| {
                b.markets
                    .len()
                    .cmp(&a.markets.len())
                    .then(a.label.cmp(b.label))
            });

            AdminRegion {
                code: region.as_str(),
                label: region.to_string().to_uppercase(),
                enabled: mine.iter().filter(|r| r.enabled).count(),
                total: mine.len(),
                languages,
            }
        })
        .collect()
}

/// One auction house as a box of realm names.
///
/// A connected realm recorded before the members column existed has none, and
/// falls back to its joined name so the page works before the next startup
/// refreshes it.
fn market(realm: &Realm) -> AdminMarket {
    let mut names = if realm.members.is_empty() {
        vec![realm.name.clone()]
    } else {
        realm.members.clone()
    };
    names.sort();
    AdminMarket {
        id: realm.id.get(),
        names,
        enabled: realm.enabled,
    }
}

/// What to call a realm locale, in that language.
///
/// Endonyms -- "Deutsch", not "German" -- because the label is for the people
/// who play there, and it stays right whatever language the page is in.
fn language_name(tag: &str) -> &'static str {
    match tag {
        "enGB" | "enUS" => "English",
        "deDE" => "Deutsch",
        "frFR" => "Français",
        "esES" => "Español",
        "esMX" => "Español (México)",
        "ruRU" => "Русский",
        "itIT" => "Italiano",
        "ptBR" | "ptPT" => "Português",
        "koKR" => "한국어",
        "zhTW" => "繁體中文",
        "zhCN" => "简体中文",
        _ => "Other",
    }
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

// --- market events (Phase 8) -------------------------------------------------

/// What the annotation form submits.
#[derive(Debug, Deserialize)]
pub struct AddEvent {
    csrf_token: String,
    kind: String,
    title: String,
    /// `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM`. A date without a time is taken as
    /// midnight UTC -- everything in `market_events` is UTC, because a local
    /// time in that table would be a different instant depending on who read
    /// it.
    starts_at: String,
    #[serde(default)]
    notes: String,
    /// Empty for every region. An event scoped to one region is not evidence
    /// about another, and the analysis page prints the scope for that reason.
    #[serde(default)]
    region: String,
}

/// `POST /admin/events` -- write down that something happened.
///
/// It lands **unvalidated and internal**, always, whoever typed it. That is
/// not a workflow preference: an annotation is the one event kind whose truth
/// rests on somebody's word, and a page that marked it on a chart before
/// anybody checked would be making a claim on the reader's behalf. Publishing
/// it is a second, deliberate action.
pub async fn add_event<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(cache): Extension<std::sync::Arc<crate::FragmentCache>>,
    headers: HeaderMap,
    Form(form): Form<AddEvent>,
) -> WebResult<Redirect> {
    let user = current_user(&env, &headers).await?;
    let Some(user) = user.filter(|u| u.is_admin) else {
        return Err(app_core::AppError::NotFound.into());
    };
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    let title = form.title.trim();
    let Some(kind) = app_core::market::EventKind::parse(&form.kind) else {
        return Err(app_core::AppError::NotFound.into());
    };
    let Some(starts_at) = parse_instant(form.starts_at.trim()) else {
        return Err(
            app_core::AppError::Validation(app_core::error::Message::new(
                app_core::error::text::EVENT_NEEDS_A_DATE,
            ))
            .into(),
        );
    };
    if title.is_empty() {
        return Err(
            app_core::AppError::Validation(app_core::error::Message::new(
                app_core::error::text::EVENT_NEEDS_A_TITLE,
            ))
            .into(),
        );
    }

    let scope = app_core::market::EventScope {
        regions: app_core::market::Region::parse(form.region.trim())
            .into_iter()
            .collect(),
        ..Default::default()
    };
    // Derived from the instant and the title so that submitting the same
    // annotation twice writes it once -- `record` is `DO NOTHING` on the id,
    // and a random id would defeat that.
    let id = format!(
        "note:{}:{}",
        starts_at.get(),
        title
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(32)
            .collect::<String>()
            .to_lowercase()
    );

    let event = app_core::market::MarketEvent {
        id,
        kind,
        title: title.to_string(),
        notes: Some(form.notes.trim().to_string()).filter(|n| !n.is_empty()),
        starts_at,
        ends_at: None,
        scope,
        provenance: app_core::market::Provenance::Administrator,
        validation: app_core::market::Validation::Unvalidated,
        visibility: app_core::market::Visibility::Internal,
    };
    let written = env.store().market_events().record(&[event]).await?;
    // The analysis version has not moved, so nothing else would make the
    // cached fragments miss. See `FragmentCache::events_epoch`.
    cache.bump_events();
    tracing::info!(by = %user.username, written, "market event recorded");

    Ok(Redirect::to("/admin"))
}

/// What the review buttons submit.
#[derive(Debug, Deserialize)]
pub struct ReviewEvent {
    csrf_token: String,
    id: String,
    /// `publish`, `reject`, `retract`, or `forget`.
    action: String,
}

/// `POST /admin/events/review` -- check an event, or take it back.
pub async fn review_event<E: Ports>(
    State(env): State<E>,
    Extension(csrf): Extension<Csrf>,
    Extension(cache): Extension<std::sync::Arc<crate::FragmentCache>>,
    headers: HeaderMap,
    Form(form): Form<ReviewEvent>,
) -> WebResult<Redirect> {
    let user = current_user(&env, &headers).await?;
    let Some(user) = user.filter(|u| u.is_admin) else {
        return Err(app_core::AppError::NotFound.into());
    };
    csrf.verify_request(&headers, Some(&form.csrf_token))?;

    use app_core::market::{Validation, Visibility};
    let events = env.store().market_events();
    let done = match form.action.as_str() {
        // Validated *and* public together, because they are one decision:
        // "this is true, and people may see it". Doing the halves separately
        // would allow published-and-unchecked to exist in between.
        "publish" => {
            events
                .review(&form.id, Validation::Validated, Visibility::Public)
                .await?
        }
        // Checked and found wrong. Kept rather than deleted: that somebody
        // looked and rejected it is worth more than the row's absence.
        "reject" => {
            events
                .review(&form.id, Validation::Rejected, Visibility::Internal)
                .await?
        }
        // Back to unchecked and internal -- an undo for a publication.
        "retract" => {
            events
                .review(&form.id, Validation::Unvalidated, Visibility::Internal)
                .await?
        }
        "forget" => events.forget(&form.id).await?,
        _ => return Err(app_core::AppError::NotFound.into()),
    };
    cache.bump_events();
    tracing::info!(by = %user.username, id = %form.id, action = %form.action, done,
        "market event reviewed");

    Ok(Redirect::to("/admin"))
}

/// `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM`, as UTC milliseconds.
///
/// Hand-rolled rather than a date crate for the reason the rest of this
/// codebase gives: `cluster_core` already does civil-date arithmetic, and one
/// form field is not a dependency.
fn parse_instant(raw: &str) -> Option<Millis> {
    let (date, time) = raw.split_once('T').unwrap_or((raw, "00:00"));
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut clock = time.split(':');
    let hour: i64 = clock.next().unwrap_or("0").parse().unwrap_or(0);
    let minute: i64 = clock.next().unwrap_or("0").parse().unwrap_or(0);
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }

    // Days since the epoch, by the civil-from-days algorithm (Howard Hinnant's),
    // which is the same one `cluster_core::time` uses in the other direction.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    if days < 0 {
        return None;
    }
    Some(Millis(
        (days as u64) * 86_400_000 + (hour as u64) * 3_600_000 + (minute as u64) * 60_000,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The date arithmetic, against the inverse this codebase already has.
    ///
    /// Hand-rolled civil-date maths is exactly the kind of thing that is
    /// plausibly wrong for years at a time, so it is checked against
    /// `Millis::to_utc_string` rather than against another copy of my own
    /// reasoning: parse a date, print it back, and see the same day.
    #[test]
    fn a_typed_date_round_trips_through_the_formatter() {
        for (typed, expected) in [
            ("1970-01-01", "1970-01-01 00:00:00"),
            ("2000-02-29", "2000-02-29 00:00:00"),
            ("2026-08-30", "2026-08-30 00:00:00"),
            ("2026-12-31", "2026-12-31 00:00:00"),
            ("2026-08-30T14:35", "2026-08-30 14:35:00"),
            // A century that is not a leap year, which is the case a naive
            // "divisible by four" would get wrong -- 2100-02-29 does not
            // exist, so the first of March is day 59 of that year and not 60.
            ("2100-03-01", "2100-03-01 00:00:00"),
            // And one that is: 2000 was a leap year, hence the 29th above.
            ("2000-03-01", "2000-03-01 00:00:00"),
        ] {
            let at = parse_instant(typed).unwrap_or_else(|| panic!("{typed} did not parse"));
            assert_eq!(at.to_utc_string(), expected, "for {typed}");
        }
    }

    /// Rubbish is refused rather than being taken as the epoch.
    ///
    /// A date that silently became 1970 would file an annotation fifty-six
    /// years before anything this app has ever observed, where no page would
    /// show it and nobody would find out.
    #[test]
    fn an_unparseable_date_is_refused() {
        for bad in [
            "",
            "today",
            "2026",
            "2026-08",
            "2026-13-01",
            "2026-00-10",
            "2026-08-32",
            "2026-08-30T25:00",
            "2026-08-30T12:61",
            // Before the epoch. `Millis` is unsigned, so there is no instant
            // to return -- and an annotation that silently became 1970 would
            // sit fifty-six years before anything this app has observed, where
            // no page would show it and nobody would find out.
            "1969-12-31",
            "1900-03-01",
        ] {
            assert_eq!(parse_instant(bad), None, "{bad:?} should not parse");
        }
    }
}
