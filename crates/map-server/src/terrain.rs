//! Versioned, on-demand Terrarium DEM tiles. Source metadata is computed only
//! when an operator publishes files from DEM_DATA_ROOT/<id>/source.
use std::{
    collections::HashMap,
    fs,
    io::Cursor,
    path::{Path as FsPath, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use gdal::{
    raster::{reproject, RasterCreationOptions},
    spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef},
    Dataset, DriverManager,
};
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, RgbImage};
use map_catalog::{DemAsset, DemDataset};
use ogc_wmts::{tile_bounds, WEB_MERCATOR_HALF_WORLD};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{
    active_wmts_cache, cache_get, cache_put, sha256_hex, tile_flight, AppState, WmtsCache,
    WmtsCacheKey,
};

const TILE_SIZE: usize = 256;
const VOID: f64 = -3.0e38;
type ApiError = (StatusCode, String);
const MAX_UPLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct FilePage {
    #[serde(default = "first_page")]
    page: usize,
    #[serde(default = "default_page_size")]
    page_size: usize,
}
fn first_page() -> usize {
    1
}
fn default_page_size() -> usize {
    20
}

pub(crate) async fn list_dem_datasets() -> Result<Json<Value>, ApiError> {
    let ids = tokio::task::spawn_blocking(|| -> Result<Vec<String>, String> {
        let root = dem_data_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut ids = fs::read_dir(root)
            .map_err(|e| e.to_string())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|id| valid_id(id).is_ok())
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    Ok(Json(json!({"datasets": ids})))
}

pub(crate) async fn list_dem_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(page): Query<FilePage>,
) -> Result<Json<Value>, ApiError> {
    valid_id(&id)?;
    if page.page == 0 || page.page_size == 0 || page.page_size > 100 {
        return Err(bad_request(
            "page must be positive and page_size must be 1..100",
        ));
    }
    let id_for_scan = id.clone();
    let listing = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let directory = dem_data_root().join(&id_for_scan).join("source");
        let mut files = Vec::new();
        if directory.exists() {
            for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let metadata = entry.metadata().map_err(|e| e.to_string())?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !metadata.is_file() || !is_dem_filename(&name) {
                    continue;
                }
                let modified = metadata
                    .modified()
                    .unwrap_or(UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                files.push(json!({"name":name,"size":metadata.len(),"modified_ms":modified}));
            }
        }
        files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let total = files.len();
        let start = page
            .page
            .saturating_sub(1)
            .saturating_mul(page.page_size)
            .min(total);
        let files = files
            .into_iter()
            .skip(start)
            .take(page.page_size)
            .collect::<Vec<_>>();
        Ok(
            json!({"files":files,"total":total,"page":page.page,"page_size":page.page_size,
            "directory":directory.to_string_lossy()}),
        )
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    let dataset = state.catalog.get_dem_dataset(&id).await.map_err(internal)?;
    let mut listing = listing;
    listing["dataset"] = json!(dataset);
    Ok(Json(listing))
}

pub(crate) async fn upload_dem_file(
    Path((id, filename)): Path<(String, String)>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    valid_id(&id)?;
    if !is_dem_filename(&filename) {
        return Err(bad_request("filename must be a .tif or .tiff file"));
    }
    let root = dem_data_root();
    let directory = root.join(&id).join("source");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(internal)?;
    let root = root.canonicalize().map_err(internal)?;
    let directory = directory.canonicalize().map_err(internal)?;
    if !directory.starts_with(&root) {
        return Err(bad_request("source directory escapes DEM_DATA_ROOT"));
    }
    let destination = directory.join(&filename);
    if destination.exists() {
        return Err((StatusCode::CONFLICT, "file already exists".into()));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = directory.join(format!(".upload-{}-{nonce}.part", std::process::id()));
    let upload_limit = std::env::var("DEM_UPLOAD_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MAX_UPLOAD_BYTES);
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(internal)?;
        let mut body = request.into_body();
        let mut size = 0u64;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(bad_request)?;
            if let Ok(bytes) = frame.into_data() {
                size = size
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| bad_request("file too large"))?;
                if size > upload_limit {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("DEM upload exceeds the configured limit of {upload_limit} bytes"),
                    ));
                }
                file.write_all(&bytes).await.map_err(internal)?;
            }
        }
        file.flush().await.map_err(internal)?;
        drop(file);
        let verify = temporary.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let dem = Dataset::open(verify).map_err(|e| e.to_string())?;
            if dem.raster_count() != 1 {
                return Err("DEM must have exactly one band".into());
            }
            dem.geo_transform().map_err(|e| e.to_string())?;
            dem.spatial_ref().map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
        .map_err(internal)?
        .map_err(bad_request)?;
        fs::hard_link(&temporary, &destination).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                (StatusCode::CONFLICT, "file already exists".into())
            } else {
                internal(error)
            }
        })?;
        Ok::<u64, ApiError>(size)
    }
    .await;
    let _ = tokio::fs::remove_file(&temporary).await;
    let size = result?;
    Ok(Json(json!({"name":filename,"size":size,"dataset_id":id})))
}

fn is_dem_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
        && FsPath::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff"))
}

#[derive(Deserialize)]
pub(crate) struct ImportDemRequest {
    title: String,
    vertical_datum: String,
    /// Used only when a source raster does not declare a unit.
    unit: Option<String>,
    attribution: Option<String>,
    max_zoom: Option<u8>,
    /// Optional filename-to-priority map. Higher values win in overlaps.
    priorities: Option<HashMap<String, i64>>,
    /// Materialize immutable, overview-equipped Cloud Optimized GeoTIFF copies.
    #[serde(default)]
    prepare_cog: bool,
}

pub(crate) async fn import_dem(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ImportDemRequest>,
) -> Result<Json<Value>, ApiError> {
    valid_id(&id)?;
    if request.title.trim().is_empty() || request.vertical_datum.trim().is_empty() {
        return Err(bad_request("title and vertical_datum are required"));
    }
    let max_zoom = request.max_zoom.unwrap_or(18);
    if max_zoom > 22 {
        return Err(bad_request("max_zoom must be at most 22"));
    }
    let prepared_cog = request.prepare_cog;
    let result = tokio::task::spawn_blocking(move || inspect_import(&id, request, max_zoom))
        .await
        .map_err(internal)?
        .map_err(bad_request)?;
    let (dataset, assets) = result;
    if let Err(error) = state.catalog.put_dem_dataset(&dataset, &assets).await {
        if prepared_cog {
            let path = dem_data_root()
                .join(&dataset.id)
                .join("optimized")
                .join(&dataset.revision);
            let _ = fs::remove_dir_all(path);
        }
        return Err(internal(error));
    }
    Ok(Json(json!({"dataset": dataset, "assets": assets.len()})))
}

pub(crate) async fn dem_metadata(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    valid_id(&id)?;
    let dataset = state
        .catalog
        .get_dem_dataset(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "DEM dataset not found".into()))?;
    let url = format!(
        "/api/terrain/{}/tiles/{}/{{z}}/{{x}}/{{y}}",
        id, dataset.revision
    );
    let longitude = |x: f64| x / WEB_MERCATOR_HALF_WORLD * 180.0;
    let latitude = |y: f64| {
        (y * std::f64::consts::PI / WEB_MERCATOR_HALF_WORLD)
            .sinh()
            .atan()
            .to_degrees()
    };
    let bounds_wgs84 = [
        longitude(dataset.min_x),
        latitude(dataset.min_y),
        longitude(dataset.max_x),
        latitude(dataset.max_y),
    ];
    Ok(Json(json!({
        "dataset": dataset, "tileMatrixSet": "WebMercatorQuad", "tileSize": TILE_SIZE,
        "encoding": "terrarium", "unit": "metre", "nodata": [0, 0, 0],
        "boundsWgs84": bounds_wgs84, "tileUrl": url,
    })))
}

pub(crate) async fn dem_tile(
    State(state): State<AppState>,
    Path((id, revision, z, x, y)): Path<(String, String, u8, u32, u32)>,
) -> Result<Response, ApiError> {
    valid_id(&id)?;
    let bounds = tile_bounds(z, y, x).map_err(|e| bad_request(e.to_string()))?;
    let dataset = state
        .catalog
        .get_dem_dataset(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "DEM dataset not found".into()))?;
    if revision != dataset.revision {
        return Err((StatusCode::NOT_FOUND, "DEM revision not found".into()));
    }
    if i64::from(z) < dataset.min_zoom || i64::from(z) > dataset.max_zoom {
        return Err((StatusCode::NOT_FOUND, "zoom outside DEM range".into()));
    }
    let cache = active_wmts_cache(&state);
    let path = tile_cache_path(&id, &revision, z, x, y);
    let key = dem_cache_key(&id, &revision, z, x, y);
    match dem_cache_get(&cache, &key, &path).await {
        Ok(Some(bytes)) => return Ok(png_response(bytes)),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "DEM cache read failed; rendering tile"),
    }
    let flight = tile_flight(&state, &key);
    let _guard = flight.lock().await;
    if state
        .catalog
        .get_dem_dataset(&id)
        .await
        .map_err(internal)?
        .map_or(true, |current| current.revision != revision)
    {
        return Err((StatusCode::NOT_FOUND, "DEM revision not found".into()));
    }
    match dem_cache_get(&cache, &key, &path).await {
        Ok(Some(bytes)) => return Ok(png_response(bytes)),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "DEM cache read failed after waiting; rendering tile"),
    }

    let mercator = [bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y];
    let assets = state
        .catalog
        .dem_assets_at(&id, mercator)
        .await
        .map_err(internal)?;
    let _permit = state.render_limit.acquire().await.map_err(internal)?;
    let bytes = tokio::task::spawn_blocking(move || render_dem(assets, mercator))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    if let Err(error) = dem_cache_put(&cache, &key, &path, &dataset.title, &bytes).await {
        tracing::warn!(%error, "DEM cache write failed; returning rendered tile");
    }
    Ok(png_response(bytes))
}

fn dem_cache_key(id: &str, revision: &str, z: u8, x: u32, y: u32) -> WmtsCacheKey {
    // Cangling Tile's item is one URL path segment. Hash the dataset and
    // revision together so a republish always gets a distinct cache entry.
    WmtsCacheKey {
        database: "dem".into(),
        item: sha256_hex(format!("{id}:{revision}").as_bytes()),
        zoom: z,
        col: x,
        row: y,
    }
}

async fn dem_cache_get(
    cache: &WmtsCache,
    key: &WmtsCacheKey,
    path: &FsPath,
) -> Result<Option<Vec<u8>>, String> {
    match cache {
        WmtsCache::CanglingTile { .. } => cache_get(cache, key).await,
        WmtsCache::Filesystem { .. } => match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        },
        WmtsCache::None => Ok(None),
    }
}

async fn dem_cache_put(
    cache: &WmtsCache,
    key: &WmtsCacheKey,
    path: &FsPath,
    title: &str,
    bytes: &[u8],
) -> Result<(), String> {
    match cache {
        WmtsCache::CanglingTile { .. } => cache_put(cache, key, "DEM", title, bytes).await,
        WmtsCache::Filesystem { .. } => {
            write_cache_atomic(path, bytes).map_err(|error| error.to_string())
        }
        WmtsCache::None => Ok(()),
    }
}

fn inspect_import(
    id: &str,
    request: ImportDemRequest,
    max_zoom: u8,
) -> Result<(DemDataset, Vec<DemAsset>), String> {
    let root = dem_data_root();
    let source_dir = root
        .join(id)
        .join("source")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !source_dir.starts_with(&root) {
        return Err("DEM source directory escapes DEM_DATA_ROOT".into());
    }
    let mut files = fs::read_dir(&source_dir)
        .map_err(|e| e.to_string())?
        .map(|entry| entry.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    files.retain(|path| {
        path.extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
    });
    files.sort();
    if files.is_empty() {
        return Err("source directory has no GeoTIFF files".into());
    }
    let mut digest = Sha256::new();
    for file in &files {
        let meta = fs::metadata(file).map_err(|e| e.to_string())?;
        digest.update(file.to_string_lossy().as_bytes());
        digest.update(meta.len().to_le_bytes());
        digest.update(
            meta.modified()
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
    }
    digest.update(request.vertical_datum.as_bytes());
    digest.update(max_zoom.to_le_bytes());
    digest.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    let revision = format!("{:x}", digest.finalize());
    let optimized_dir = root.join(id).join("optimized").join(&revision);
    if request.prepare_cog {
        fs::create_dir_all(&optimized_dir).map_err(|e| e.to_string())?;
    }
    let mut cog_cleanup = CogCleanup(request.prepare_cog.then_some(optimized_dir.clone()));
    let mut assets = Vec::with_capacity(files.len());
    let mut extent = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for file in files {
        let file = file.canonicalize().map_err(|e| e.to_string())?;
        if !file.starts_with(&source_dir) {
            return Err("DEM file escapes source directory".into());
        }
        let dataset = Dataset::open(&file).map_err(|e| e.to_string())?;
        if dataset.raster_count() != 1 {
            return Err(format!("{}: expected one elevation band", file.display()));
        }
        let band = dataset.rasterband(1).map_err(|e| e.to_string())?;
        if band.scale().is_some_and(|v| (v - 1.0).abs() > 1e-12)
            || band.offset().is_some_and(|v| v.abs() > 1e-12)
        {
            return Err(format!(
                "{}: scaled bands must first be normalized to numeric elevation",
                file.display()
            ));
        }
        let band_unit = band.unit();
        let unit = if band_unit.trim().is_empty() {
            request.unit.as_deref().unwrap_or("")
        } else {
            band_unit.trim()
        };
        let unit = normalize_unit(unit)
            .ok_or_else(|| format!("{}: unit must be metres or feet", file.display()))?;
        let (width, height) = dataset.raster_size();
        let gt = dataset.geo_transform().map_err(|e| e.to_string())?;
        let corners = [
            (0.0, 0.0),
            (width as f64, 0.0),
            (0.0, height as f64),
            (width as f64, height as f64),
        ];
        let xs = corners.map(|(col, row)| gt[0] + col * gt[1] + row * gt[2]);
        let ys = corners.map(|(col, row)| gt[3] + col * gt[4] + row * gt[5]);
        let native = [
            xs.into_iter().fold(f64::INFINITY, f64::min),
            ys.into_iter().fold(f64::INFINITY, f64::min),
            xs.into_iter().fold(f64::NEG_INFINITY, f64::max),
            ys.into_iter().fold(f64::NEG_INFINITY, f64::max),
        ];
        let mut srs = dataset.spatial_ref().map_err(|e| e.to_string())?;
        srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let mut mercator_srs = SpatialRef::from_epsg(3857).map_err(|e| e.to_string())?;
        mercator_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let transform = CoordTransform::new(&srs, &mercator_srs).map_err(|e| e.to_string())?;
        let mut bounds = transform
            .transform_bounds(&native, 21)
            .map_err(|e| e.to_string())?;
        for value in &mut bounds {
            *value = value.clamp(-WEB_MERCATOR_HALF_WORLD, WEB_MERCATOR_HALF_WORLD);
        }
        if !bounds.iter().all(|v| v.is_finite()) || bounds[0] >= bounds[2] || bounds[1] >= bounds[3]
        {
            return Err(format!("{}: invalid projected bounds", file.display()));
        }
        extent[0] = extent[0].min(bounds[0]);
        extent[1] = extent[1].min(bounds[1]);
        extent[2] = extent[2].max(bounds[2]);
        extent[3] = extent[3].max(bounds[3]);
        let published_path = if request.prepare_cog {
            let destination = optimized_dir.join(file.file_name().ok_or("DEM file has no name")?);
            let driver = DriverManager::get_driver_by_name("COG").map_err(|e| e.to_string())?;
            let options = RasterCreationOptions::from_iter([
                "COMPRESS=DEFLATE",
                "BLOCKSIZE=512",
                "OVERVIEWS=AUTO",
                "OVERVIEW_RESAMPLING=AVERAGE",
            ]);
            dataset
                .create_copy(&driver, &destination, &options)
                .map_err(|e| e.to_string())?;
            destination
        } else {
            file.clone()
        };
        assets.push(DemAsset {
            dataset_id: id.into(),
            path: published_path.to_string_lossy().into_owned(),
            native_crs: srs
                .authority()
                .or_else(|_| srs.to_wkt())
                .map_err(|e| e.to_string())?,
            width: width as i64,
            height: height as i64,
            resolution_m: ((bounds[2] - bounds[0]) / width as f64)
                .max((bounds[3] - bounds[1]) / height as f64),
            nodata: band.no_data_value().filter(|v| v.is_finite()),
            unit: unit.into(),
            priority: request
                .priorities
                .as_ref()
                .and_then(|values| {
                    file.file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| values.get(name))
                })
                .copied()
                .unwrap_or(0),
            min_x: bounds[0],
            min_y: bounds[1],
            max_x: bounds[2],
            max_y: bounds[3],
        });
    }
    cog_cleanup.0 = None;
    Ok((
        DemDataset {
            id: id.into(),
            title: request.title,
            revision,
            vertical_datum: request.vertical_datum,
            attribution: request.attribution,
            min_zoom: 0,
            max_zoom: i64::from(max_zoom),
            min_x: extent[0],
            min_y: extent[1],
            max_x: extent[2],
            max_y: extent[3],
        },
        assets,
    ))
}

struct CogCleanup(Option<PathBuf>);
impl Drop for CogCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn dem_data_root() -> PathBuf {
    std::env::var_os("DEM_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/dem"))
}

fn render_dem(assets: Vec<DemAsset>, bounds: [f64; 4]) -> Result<Vec<u8>, String> {
    let mut heights = vec![f64::NAN; TILE_SIZE * TILE_SIZE];
    let driver = DriverManager::get_driver_by_name("MEM").map_err(|e| e.to_string())?;
    let srs = SpatialRef::from_epsg(3857).map_err(|e| e.to_string())?;
    for asset in assets {
        if heights.iter().all(|h| h.is_finite()) {
            break;
        }
        let source = Dataset::open(&asset.path).map_err(|e| e.to_string())?;
        let mut target = driver
            .create_with_band_type::<f32, _>("", TILE_SIZE, TILE_SIZE, 1)
            .map_err(|e| e.to_string())?;
        target.set_spatial_ref(&srs).map_err(|e| e.to_string())?;
        target
            .set_geo_transform(&[
                bounds[0],
                (bounds[2] - bounds[0]) / TILE_SIZE as f64,
                0.0,
                bounds[3],
                0.0,
                -(bounds[3] - bounds[1]) / TILE_SIZE as f64,
            ])
            .map_err(|e| e.to_string())?;
        let mut band = target.rasterband(1).map_err(|e| e.to_string())?;
        band.set_no_data_value(Some(VOID))
            .map_err(|e| e.to_string())?;
        band.fill(VOID, None).map_err(|e| e.to_string())?;
        reproject(&source, &target).map_err(|e| e.to_string())?;
        let pixels = target
            .rasterband(1)
            .map_err(|e| e.to_string())?
            .read_band_as::<f32>()
            .map_err(|e| e.to_string())?;
        let scale = if asset.unit == "foot" { 0.3048 } else { 1.0 };
        for (out, value) in heights.iter_mut().zip(pixels.data()) {
            if !out.is_finite() && value.is_finite() && f64::from(*value) > VOID / 2.0 {
                *out = f64::from(*value) * scale;
            }
        }
    }
    let mut rgb = Vec::with_capacity(TILE_SIZE * TILE_SIZE * 3);
    for elevation in heights {
        rgb.extend_from_slice(&encode_height(elevation));
    }
    let image =
        RgbImage::from_raw(TILE_SIZE as u32, TILE_SIZE as u32, rgb).ok_or("invalid tile buffer")?;
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(bytes.into_inner())
}

fn encode_height(elevation: f64) -> [u8; 3] {
    if !elevation.is_finite() || !(-32767.996..=32767.996).contains(&elevation) {
        return [0, 0, 0];
    }
    let encoded = ((elevation + 32768.0) * 256.0).round() as u32;
    [
        ((encoded >> 16) & 255) as u8,
        ((encoded >> 8) & 255) as u8,
        (encoded & 255) as u8,
    ]
}

fn tile_cache_path(id: &str, revision: &str, z: u8, x: u32, y: u32) -> PathBuf {
    let root = std::env::var_os("DEM_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::map_cache_root().join("terrain"));
    root.join(id)
        .join(revision)
        .join(z.to_string())
        .join(x.to_string())
        .join(format!("{y}.png"))
}

fn write_cache_atomic(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(path.parent().expect("tile has parent"))?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)
}

fn png_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

fn normalize_unit(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "m" | "metre" | "meter" | "metres" | "meters" => Some("metre"),
        "ft" | "foot" | "feet" => Some("foot"),
        _ => None,
    }
}

fn valid_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        Err(bad_request(
            "dataset id must contain only ASCII letters, digits, '-' or '_'",
        ))
    } else {
        Ok(())
    }
}
fn bad_request(message: impl ToString) -> ApiError {
    (StatusCode::BAD_REQUEST, message.to_string())
}
fn internal(error: impl ToString) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{dem_cache_key, encode_height, is_dem_filename};

    #[test]
    fn dem_cache_key_uses_one_item_segment_and_changes_with_revision() {
        let first = dem_cache_key("sichuan", "old-revision", 8, 3, 4);
        let next = dem_cache_key("sichuan", "new-revision", 8, 3, 4);
        assert_eq!(first.database, "dem");
        assert_eq!(first.item.len(), 64);
        assert!(first.item.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first.item, next.item);
    }

    #[test]
    fn terrarium_height_encoding_preserves_signed_metres_and_void() {
        assert_eq!(encode_height(100.0), [128, 100, 0]);
        assert_eq!(encode_height(0.0), [128, 0, 0]);
        assert_eq!(encode_height(-100.0), [127, 156, 0]);
        assert_eq!(encode_height(f64::NAN), [0, 0, 0]);
    }

    #[test]
    fn upload_name_stays_within_source_directory() {
        assert!(is_dem_filename("sichuan_dem.tif"));
        assert!(is_dem_filename("高程数据.TIFF"));
        for name in [
            "../other.tif",
            "nested/file.tif",
            "\\\\server.tif",
            ".hidden.tif",
            "map.png",
        ] {
            assert!(!is_dem_filename(name), "unexpectedly accepted {name}");
        }
    }
}
