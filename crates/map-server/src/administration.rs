//! Read-only XYZ vector tiles for OSM China administrative names and boundaries.
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{placenames::tile_bounds, AppState};

type ApiError = (StatusCode, String);
const MAX_FEATURES_PER_TILE: i64 = 512;

#[derive(Deserialize, Default)]
pub(crate) struct TileOptions {
    lang: Option<String>,
}

pub(crate) async fn metadata(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let Some(admin) = &state.administration else {
        return Ok(Json(
            json!({"available": false, "reason": "admin.sqlite is not installed"}),
        ));
    };
    let metadata = admin.metadata().await.map_err(internal)?;
    Ok(Json(json!({
        "available": true,
        "metadata": metadata,
        "tile_url": "/api/admin/tiles/{z}/{x}/{y}?lang=zh",
        "zoom_levels": {"country": [0,3], "province": [4,6], "prefecture": [7,8],
                        "county": [9,11], "township": [12,14], "village": [15,20]},
        "coverage_note": "OSM community coverage is incomplete, especially village boundaries"
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
    let level = level_for_zoom(z);
    let Some(admin) = &state.administration else {
        return Ok(Json(json!({"z":z,"x":x,"y":y,"admin_level":level,
                              "features":[],"boundaries":[],"available":false})));
    };
    let (features, boundaries) = tokio::try_join!(
        admin.labels(bounds, level, language, MAX_FEATURES_PER_TILE),
        admin.boundaries(bounds, level, MAX_FEATURES_PER_TILE),
    )
    .map_err(internal)?;
    Ok(Json(json!({
        "z": z, "x": x, "y": y,
        "crs": "EPSG:4326",
        "admin_level": level,
        "features": features,
        "boundaries": boundaries,
        "attribution": "© OpenStreetMap contributors",
        "available": true
    })))
}

fn level_for_zoom(z: u8) -> i64 {
    match z {
        0..=3 => 2,
        4..=6 => 4,
        7..=8 => 5,
        9..=11 => 6,
        12..=14 => 8,
        _ => 10,
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::level_for_zoom;

    #[test]
    fn administrative_detail_increases_at_explicit_zoom_levels() {
        assert_eq!(level_for_zoom(3), 2);
        assert_eq!(level_for_zoom(4), 4);
        assert_eq!(level_for_zoom(7), 5);
        assert_eq!(level_for_zoom(9), 6);
        assert_eq!(level_for_zoom(12), 8);
        assert_eq!(level_for_zoom(15), 10);
    }
}
