//! The WoW vertical slice: search a character, render a profile.

use app_core::Ports;
use app_core::repo::Store;
use app_core::service::{CharacterService, Freshness};
use app_core::wow::CharacterProvider;
use askama::Template;
use axum::extract::{Query, State};
use axum::response::Html;
use serde::Deserialize;

use crate::error::WebResult;
use crate::render::page;
use crate::views::CharacterView;

#[derive(Debug, Deserialize)]
pub struct CharacterQueryParams {
    pub region: String,
    pub realm: String,
    pub name: String,
}

#[derive(Template)]
#[template(path = "partials/character.html")]
struct CharacterFragment {
    character: CharacterView,
}

/// `GET /wow/character` -> a character card fragment for HTMX to swap in.
pub async fn character<E: Ports>(
    State(env): State<E>,
    Query(params): Query<CharacterQueryParams>,
) -> WebResult<Html<String>> {
    let store = env.store();
    let service = CharacterService::new(
        env.characters(),
        store.cache(),
        env.config().upstream_cache_ttl_ms,
    );

    let query = CharacterService::<E::Characters, <E::Store as Store>::Cache>::validate(
        &params.region,
        &params.realm,
        &params.name,
    )?;
    let (character, freshness) = service.lookup(&query, env.now()).await?;

    page(&CharacterFragment {
        character: CharacterView::new(
            &character,
            env.characters().provider_name(),
            freshness == Freshness::Cached,
        ),
    })
}
