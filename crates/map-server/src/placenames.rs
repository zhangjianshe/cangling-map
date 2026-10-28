//! Read-only XYZ JSON tiles backed by the standalone place-name SQLite file.
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;

type ApiError = (StatusCode, String);
const MAX_ZOOM: u8 = 20;
const MAX_PLACES_PER_TILE: i64 = 512;

#[derive(Deserialize, Default)]
pub(crate) struct TileOptions {
    lang: Option<String>,
}

pub(crate) async fn metadata(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let metadata = state.placenames.metadata().await.map_err(internal)?;
    Ok(Json(json!({
        "metadata": metadata,
        "min_zoom": 0,
        "max_zoom": MAX_ZOOM,
        "languages": ["default", "zh", "en"],
        "tile_url": "/api/placenames/tiles/{z}/{x}/{y}?lang=zh"
    })))
}

pub(crate) async fn tile(
    State(state): State<AppState>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
    Query(options): Query<TileOptions>,
) -> Result<Json<Value>, ApiError> {
    let bounds = tile_bounds(z, x, y).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "invalid XYZ tile coordinate".into(),
        )
    })?;
    let language = options.lang.as_deref().unwrap_or("default");
    if !matches!(language, "default" | "zh" | "en") {
        return Err((
            StatusCode::BAD_REQUEST,
            "lang must be default, zh, or en".into(),
        ));
    }
    let features = state
        .placenames
        .within_bounds(bounds, z, language, MAX_PLACES_PER_TILE)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "z": z, "x": x, "y": y,
        "crs": "EPSG:4326",
        "features": features,
        "attribution": "Natural Earth"
    })))
}

fn tile_bounds(z: u8, x: u32, y: u32) -> Option<[f64; 4]> {
    if z > MAX_ZOOM || x >= (1_u32 << z) || y >= (1_u32 << z) {
        return None;
    }
    let count = (1_u32 << z) as f64;
    let west = x as f64 / count * 360.0 - 180.0;
    let east = (x as f64 + 1.0) / count * 360.0 - 180.0;
    let latitude = |row: f64| {
        (std::f64::consts::PI * (1.0 - 2.0 * row / count))
            .sinh()
            .atan()
            .to_degrees()
    };
    Some([west, latitude(y as f64 + 1.0), east, latitude(y as f64)])
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::tile_bounds;

    #[test]
    fn xyz_bounds_cover_the_expected_hemisphere() {
        let northwest = tile_bounds(1, 0, 0).unwrap();
        assert_eq!(northwest[0], -180.0);
        assert_eq!(northwest[2], 0.0);
        assert_eq!(northwest[1], 0.0);
        assert!(northwest[3] > 85.0 && northwest[3] < 85.1);
        assert!(tile_bounds(1, 2, 0).is_none());
        assert!(tile_bounds(21, 0, 0).is_none());
    }
}
