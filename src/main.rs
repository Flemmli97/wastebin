#![forbid(unsafe_code)]

use crate::cache::Cache;
use crate::db::Database;
use crate::errors::Error;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::{Router, Server};
use axum_extra::extract::cookie::Key;
use std::process::ExitCode;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use url::Url;

mod cache;
mod crypto;
mod db;
mod env;
mod errors;
mod highlight;
mod id;
mod pages;
pub(crate) mod routes;
#[cfg(test)]
mod test_helpers;

#[derive(Clone)]
pub struct AppState {
    db: Database,
    cache: Cache,
    key: Key,
    base_url: Option<Url>,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.key.clone()
    }
}

pub(crate) fn make_app(max_body_size: usize) -> Router<AppState> {
    Router::new().merge(routes::routes()).layer(
        ServiceBuilder::new()
            .layer(DefaultBodyLimit::max(max_body_size))
            .layer(DefaultBodyLimit::disable())
            .layer(CompressionLayer::new())
            .layer(TraceLayer::new_for_http())
            .layer(TimeoutLayer::new(env::HTTP_TIMEOUT)),
    )
}

async fn start() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cache_size = env::cache_size()?;
    let method = env::database_method()?;
    let key = env::signing_key()?;
    let addr = env::addr()?;
    let max_body_size = env::max_body_size()?;
    let base_url = env::base_url()?;
    let cache = Cache::new(cache_size);
    let db = Database::new(method)?;
    let state = AppState {
        db,
        cache,
        key,
        base_url,
    };

    tracing::debug!("serving on {addr}");
    tracing::debug!("caching {cache_size} paste highlights");
    tracing::debug!("restricting maximum body size to {max_body_size} bytes");

    let service: Router<()> = make_app(max_body_size).with_state(state);

    Server::bind(&addr)
        .serve(service.into_make_service())
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to listen to ctrl-c");
        })
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match start().await {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err}");
            ExitCode::FAILURE
        }
    }
}
