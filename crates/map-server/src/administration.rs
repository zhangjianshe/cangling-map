//! Read-only XYZ JSON tiles for China administrative names and boundaries.
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{placenames::tile_bounds, AppState};

type ApiError = (StatusCode, String);
const MAX_FEATURES_PER_TILE: i64 = 512;

#[derive(Deserialize, Default)]
pub(crate) struct TileOptions {
    lang: Option<String>,
    revision: Option<String>,
}

pub(crate) async fn metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some(admin) = &state.administration else {
        return json_response(
            json!({"available": false, "reason": "admin.sqlite is not installed"}),
            &headers,
            "no-store",
        );
    };
    let metadata = admin.metadata().await.map_err(internal)?;
    let official = metadata.contains_key("ngcc_source_snapshot");
    let github_mirror = metadata.contains_key("geocode_mirror_commit");
    let preview =
        metadata.contains_key("taiwan_preview_source") || metadata.contains_key("dash_source");
    json_response(
        json!({
            "available": true,
            "dataset_revision": admin.revision(),
            "metadata": metadata,
            "tile_url": "/api/admin/tiles/{z}/{x}/{y}?lang=zh&revision={dataset_revision}",
            "zoom_levels": {"country": [0,3], "province": [4,6], "prefecture": [7,8],
                            "county": [9,11], "township": [12,14], "village": [15,20]},
            "coverage_note": if official {
                "National 1:1M public data supplies country, province and prefecture tiers; finer OSM tiers may be incomplete"
            } else if github_mirror {
                "Country, province, prefecture and county polygons use a GitHub mirror that claims Tianditu origin; original source snapshot and license are unverified. Township and village coverage remains OSM and may be incomplete"
            } else if preview {
                "Country, province and prefecture tiers remain OSM; Taiwan and sea dashes are separately attributed community derivatives of NGCC data"
            } else {
                "OSM community coverage is incomplete, especially village boundaries"
            }
        }),
        &headers,
        "no-cache",
    )
}

pub(crate) async fn tile(
    State(state): State<AppState>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
    Query(options): Query<TileOptions>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
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
        return json_response(
            json!({"z":z,"x":x,"y":y,"admin_level":level,
                              "features":[],"boundaries":[],"available":false}),
            &headers,
            "no-store",
        );
    };
    if options
        .revision
        .as_deref()
        .is_some_and(|revision| revision != admin.revision())
    {
        return Err((
            StatusCode::CONFLICT,
            "administration dataset revision has changed".into(),
        ));
    }
    let (features, boundaries) = tokio::try_join!(
        admin.labels(bounds, level, language, MAX_FEATURES_PER_TILE),
        admin.boundaries(bounds, level, MAX_FEATURES_PER_TILE),
    )
    .map_err(internal)?;
    let cache_control = if options.revision.is_some() {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=300, stale-while-revalidate=3600"
    };
    json_response(
        json!({
            "z": z, "x": x, "y": y,
            "crs": "EPSG:4326",
            "admin_level": level,
            "features": features,
            "boundaries": boundaries,
            "attribution": admin.attribution(),
            "available": true
        }),
        &headers,
        cache_control,
    )
}

fn json_response(
    value: Value,
    request_headers: &HeaderMap,
    cache_control: &'static str,
) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec(&value).map_err(internal)?;
    let etag = format!("\"{:x}\"", Sha256::digest(&bytes));
    let matched = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|tag| tag.trim() == etag || tag.trim() == format!("W/{etag}"))
        });
    let mut response = if matched {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (StatusCode::OK, bytes).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(internal)?,
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if !matched {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    Ok(response)
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
    use super::{json_response, level_for_zoom};
    use axum::http::{header, HeaderMap, StatusCode};
    use serde_json::json;

    #[test]
    fn administrative_detail_increases_at_explicit_zoom_levels() {
        assert_eq!(level_for_zoom(3), 2);
        assert_eq!(level_for_zoom(4), 4);
        assert_eq!(level_for_zoom(7), 5);
        assert_eq!(level_for_zoom(9), 6);
        assert_eq!(level_for_zoom(12), 8);
        assert_eq!(level_for_zoom(15), 10);
    }

    #[test]
    fn etag_revalidates_the_same_tile_without_a_body() {
        let initial = json_response(
            json!({"features": []}),
            &HeaderMap::new(),
            "public, max-age=300",
        )
        .unwrap();
        let etag = initial.headers()[header::ETAG].clone();
        assert_eq!(initial.status(), StatusCode::OK);
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, etag.clone());
        let unchanged =
            json_response(json!({"features": []}), &headers, "public, max-age=300").unwrap();
        assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(unchanged.headers()[header::ETAG], etag);
        assert_eq!(
            unchanged.headers()[header::CACHE_CONTROL],
            "public, max-age=300"
        );
        let changed =
            json_response(json!({"features": [1]}), &headers, "public, max-age=300").unwrap();
        assert_eq!(changed.status(), StatusCode::OK);
        assert_ne!(changed.headers()[header::ETAG], etag);
    }
}
