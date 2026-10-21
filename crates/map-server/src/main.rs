use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex, OnceLock, Weak,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use axum::{
    body::Bytes as AxumBytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use cangling_gdal_core::{
    BoundingBox as GdalBoundingBox, DataSource as GdalDataSource, GdalEngine, ImageFormat,
    RasterRenderRequest, Resampling,
};
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use map_catalog::{
    Catalog, CatalogItem, CatalogItemId, LayerGroup, LayerGroupMember, PublicationStore,
    PublishedLayer, SourceKind, SqliteCatalog, StyleRecord, StyleStore,
};
use map_render::{
    blank_image, blank_png, FallbackRasterizerStats, FallbackVectorRasterizer,
    ImageFormat as RenderImageFormat, RasterizedRgba, VectorColor, VectorImageFill, VectorPaint,
    VectorRasterRequest as RenderVectorRequest, VectorRasterizer, VectorRenderRule, VectorSource,
    VectorTextStyle,
};
use map_style::{compile_geocss, to_sld_1_1, ColorExpression, Style};
use ogc_wms::{
    capabilities_xml, service_exception, CapabilitiesGroup, CapabilitiesLayer, Operation,
    WmsRequest,
};
use ogc_wmts::{
    capabilities_xml as wmts_capabilities_xml, exception_xml as wmts_exception_xml, tile_bounds,
    CapabilitiesLayer as WmtsCapabilitiesLayer, Operation as WmtsOperation, TileRequest,
    WmtsRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore};
use tower_http::trace::TraceLayer;

static MISSING_STYLE_ASSET_WARNINGS: OnceLock<std::sync::Mutex<HashSet<String>>> = OnceLock::new();

#[derive(Clone)]
struct AppState {
    catalog: SqliteCatalog,
    wmts_cache: Arc<StdMutex<WmtsCache>>,
    local_wmts_cache: WmtsCache,
    cache_service_settings: Arc<StdMutex<CacheServiceSettings>>,
    vector_rasterizer: Arc<dyn VectorRasterizer>,
    gpu_rasterizer: Option<Arc<FallbackVectorRasterizer>>,
    gpu_backend: Option<Arc<map_gdal::GpuVectorRasterizer>>,
    vector_render_backend: String,
    render_limit: Arc<Semaphore>,
    tile_flights: Arc<StdMutex<HashMap<WmtsCacheKey, Weak<Mutex<()>>>>>,
    prepared_vector_sources: Arc<Mutex<HashSet<String>>>,
    metrics: Arc<ServiceMetrics>,
}

struct ServiceMetrics {
    started: Instant,
    wmts_requests: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    coalesced_requests: AtomicU64,
    rendered_tiles: AtomicU64,
    render_failures: AtomicU64,
    render_time_micros: AtomicU64,
    cache_write_time_micros: AtomicU64,
    details: StdMutex<MetricsDetails>,
}

#[derive(Default)]
struct MetricsDetails {
    layers: HashMap<String, LayerMetricState>,
    slow_requests: VecDeque<SlowTileRequest>,
    recent_wms_requests: VecDeque<WmsRequestSnapshot>,
    events: VecDeque<MetricEvent>,
}

#[derive(Default)]
struct LayerMetricState {
    id: String,
    title: String,
    requests: u64,
    cache_hits: u64,
    cache_misses: u64,
    coalesced: u64,
    rendered: u64,
    failures: u64,
    output_bytes: u64,
    cache_read_micros: u64,
    preparation_micros: u64,
    render_micros: u64,
    cache_write_micros: u64,
    render_samples_micros: VecDeque<u64>,
}

#[derive(Clone, Serialize)]
struct SlowTileRequest {
    timestamp: u64,
    layer: String,
    layer_id: String,
    zoom: u8,
    col: u32,
    row: u32,
    total_ms: f64,
    preparation_ms: f64,
    render_ms: f64,
    cache_write_ms: f64,
}

#[derive(Clone, Serialize)]
struct WmsRequestSnapshot {
    timestamp: u64,
    layer: String,
    width: u32,
    height: u32,
    logical_width: u32,
    logical_height: u32,
    pixel_ratio: f32,
    total_ms: f64,
}

struct WmsRequestTimer {
    metrics: Arc<ServiceMetrics>,
    snapshot: WmsRequestSnapshot,
    started: Instant,
}

impl Drop for WmsRequestTimer {
    fn drop(&mut self) {
        self.snapshot.total_ms = duration_ms(self.started.elapsed());
        let mut details = self
            .metrics
            .details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        details
            .recent_wms_requests
            .push_front(self.snapshot.clone());
        details.recent_wms_requests.truncate(50);
    }
}

struct MetricEvent {
    timestamp: u64,
    cache_hit: bool,
    duration_micros: u64,
}

impl Default for ServiceMetrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            wmts_requests: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            coalesced_requests: AtomicU64::new(0),
            rendered_tiles: AtomicU64::new(0),
            render_failures: AtomicU64::new(0),
            render_time_micros: AtomicU64::new(0),
            cache_write_time_micros: AtomicU64::new(0),
            details: StdMutex::new(MetricsDetails::default()),
        }
    }
}

impl ServiceMetrics {
    fn record_lookup(&self, layer: &PublishedLayer, hit: bool, elapsed: std::time::Duration) {
        let mut details = self
            .details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metric = details.layers.entry(layer.name.clone()).or_default();
        metric.id.clone_from(&layer.id);
        metric.title.clone_from(&layer.title);
        metric.requests += 1;
        metric.cache_read_micros += elapsed.as_micros() as u64;
        if hit {
            metric.cache_hits += 1;
        } else {
            metric.cache_misses += 1;
        }
    }

    fn record_completed(&self, cache_hit: bool, duration: std::time::Duration) {
        let mut details = self
            .details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = unix_seconds();
        details.events.push_back(MetricEvent {
            timestamp: now,
            cache_hit,
            duration_micros: duration.as_micros() as u64,
        });
        while details
            .events
            .front()
            .is_some_and(|event| event.timestamp + 3600 < now)
        {
            details.events.pop_front();
        }
    }

    fn record_coalesced(&self, layer_name: &str) {
        let mut details = self
            .details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        details
            .layers
            .entry(layer_name.to_owned())
            .or_default()
            .coalesced += 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn record_rendered(
        &self,
        layer: &PublishedLayer,
        tile: &TileRequest,
        bytes: usize,
        preparation: std::time::Duration,
        render: std::time::Duration,
        cache_write: std::time::Duration,
        total: std::time::Duration,
    ) {
        let mut details = self
            .details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metric = details.layers.entry(layer.name.clone()).or_default();
        metric.id.clone_from(&layer.id);
        metric.title.clone_from(&layer.title);
        metric.rendered += 1;
        metric.output_bytes += bytes as u64;
        metric.preparation_micros += preparation.as_micros() as u64;
        metric.render_micros += render.as_micros() as u64;
        metric.cache_write_micros += cache_write.as_micros() as u64;
        metric
            .render_samples_micros
            .push_back(render.as_micros() as u64);
        if metric.render_samples_micros.len() > 1024 {
            metric.render_samples_micros.pop_front();
        }
        if total.as_millis() >= 50 {
            details.slow_requests.push_front(SlowTileRequest {
                timestamp: unix_seconds(),
                layer: layer.name.clone(),
                layer_id: layer.id.clone(),
                zoom: tile.zoom,
                col: tile.col,
                row: tile.row,
                total_ms: duration_ms(total),
                preparation_ms: duration_ms(preparation),
                render_ms: duration_ms(render),
                cache_write_ms: duration_ms(cache_write),
            });
            details.slow_requests.truncate(50);
        }
    }

    fn record_failure(&self, layer_name: &str) {
        let mut details = self
            .details
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        details
            .layers
            .entry(layer_name.to_owned())
            .or_default()
            .failures += 1;
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn duration_ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Clone)]
enum WmtsCache {
    Filesystem {
        root: std::path::PathBuf,
    },
    CanglingTile {
        base_url: String,
        token: String,
        database_id: Option<String>,
        client: Client<HttpConnector, Full<Bytes>>,
    },
    None,
}

impl WmtsCache {
    fn from_env() -> anyhow::Result<Self> {
        match std::env::var("WMTS_CACHE_BACKEND")
            .unwrap_or_else(|_| "filesystem".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "filesystem" => Ok(Self::Filesystem {
                root: std::env::var("WMTS_CACHE_PATH")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| map_cache_root().join("wmts"))
                    .into(),
            }),
            "cangling-tile" => {
                let base_url = std::env::var("TILE_CACHE_URL")
                    .or_else(|_| std::env::var("CANGLING_TILE_URL"))
                    .unwrap_or_else(|_| "http://127.0.0.1:7601".into())
                    .trim_end_matches('/')
                    .to_owned();
                if !base_url.starts_with("http://") {
                    anyhow::bail!("CANGLING_TILE_URL currently requires an http:// URL");
                }
                let client = Client::builder(TokioExecutor::new()).build_http();
                Ok(Self::CanglingTile {
                    base_url,
                    token: std::env::var("TILE_CACHE_TOKEN")
                        .or_else(|_| std::env::var("CANGLING_TILE_TOKEN"))
                        .unwrap_or_default(),
                    database_id: std::env::var("CANGLING_TILE_DATABASE_ID").ok(),
                    client,
                })
            }
            "none" => Ok(Self::None),
            value => anyhow::bail!("unsupported WMTS_CACHE_BACKEND: {value}"),
        }
    }

    fn cangling_tile(settings: &CacheServiceSettings) -> Result<Self, String> {
        let base_url = settings.base_url.trim().trim_end_matches('/').to_owned();
        if !base_url.starts_with("http://") {
            return Err("Cangling Tile 地址当前必须使用 http://".into());
        }
        Ok(Self::CanglingTile {
            base_url,
            token: settings.token.clone(),
            database_id: settings
                .database_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Filesystem { .. } => "filesystem",
            Self::CanglingTile { .. } => "cangling-tile",
            Self::None => "none",
        }
    }

    fn status(&self) -> CacheBackendStatus {
        match self {
            Self::Filesystem { root } => CacheBackendStatus {
                backend: "filesystem",
                label: "本地目录".into(),
                detail: root.display().to_string(),
            },
            Self::CanglingTile {
                base_url,
                database_id,
                ..
            } => CacheBackendStatus {
                backend: "cangling-tile",
                label: "Cangling Tile".into(),
                detail: database_id
                    .as_deref()
                    .map(|database| format!("{base_url} · {database}"))
                    .unwrap_or_else(|| base_url.clone()),
            },
            Self::None => CacheBackendStatus {
                backend: "none",
                label: "未启用".into(),
                detail: "当前未配置 WMTS 缓存".into(),
            },
        }
    }
}

fn map_cache_root() -> std::path::PathBuf {
    std::env::var_os("MAP_CACHE_ROOT")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("cache"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheServiceSettings {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    database_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CacheServiceSettingsInput {
    enabled: bool,
    base_url: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    clear_token: bool,
    #[serde(default)]
    database_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct CacheServiceSettingsView {
    enabled: bool,
    base_url: String,
    database_id: Option<String>,
    has_token: bool,
    effective_backend: &'static str,
    local_backend: &'static str,
}

fn active_wmts_cache(state: &AppState) -> WmtsCache {
    state
        .wmts_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn cache_service_settings_view(state: &AppState) -> CacheServiceSettingsView {
    let settings = state
        .cache_service_settings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    CacheServiceSettingsView {
        enabled: settings.enabled,
        base_url: settings.base_url,
        database_id: settings.database_id,
        has_token: !settings.token.is_empty(),
        effective_backend: active_wmts_cache(state).name(),
        local_backend: state.local_wmts_cache.name(),
    }
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

#[derive(Serialize)]
struct CatalogItemView {
    #[serde(flatten)]
    item: CatalogItem,
    reference_count: usize,
}

#[derive(Deserialize)]
struct InspectSourceRequest {
    source_kind: String,
    source_config: Value,
}

#[derive(Deserialize)]
struct HostFilesQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct HostFileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    created_unix_ms: Option<u128>,
}

#[derive(Serialize)]
struct HostFilesResponse {
    path: String,
    parent: Option<String>,
    entries: Vec<HostFileEntry>,
}

#[derive(Deserialize)]
struct FieldStatsQuery {
    field: String,
}

const STYLE_COMPILER_VERSION: &str = "map-style-geocss-label/3";

#[derive(Debug, Clone, PartialEq)]
struct VectorRenderingStyle {
    rules: Vec<VectorRenderRule>,
    background: [u8; 4],
}

impl Default for VectorRenderingStyle {
    fn default() -> Self {
        Self {
            rules: vec![VectorRenderRule {
                filter: None,
                minimum_scale_denominator: None,
                maximum_scale_denominator: None,
                paint: VectorPaint {
                    fill: Some(VectorColor::Literal(map_core::Rgba {
                        red: 70,
                        green: 70,
                        blue: 70,
                        alpha: 255,
                    })),
                    fill_image: None,
                    stroke: None,
                    stroke_width: 0.0,
                    stroke_dasharray: Vec::new(),
                    text: None,
                },
            }],
            background: [245, 245, 245, 255],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct StyleInput {
    app_id: String,
    name: String,
    title: String,
    description: Option<String>,
    geometry_type: String,
    source_format: String,
    source_text: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "map_server=info,cangling_gdal_core=debug,tower_http=info".into()
            }),
        )
        .init();
    let database_url = std::env::var("CATALOG_DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://catalog.db?mode=rwc".into());
    let render_concurrency = std::env::var("WMTS_RENDER_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(4);
    let local_wmts_cache = WmtsCache::from_env().context("failed to configure WMTS cache")?;
    let catalog = SqliteCatalog::connect(&database_url)
        .await
        .context("failed to initialize catalog")?;
    let cache_service_settings: CacheServiceSettings = catalog
        .get_setting("cache_service")
        .await
        .context("failed to load cache service settings")?
        .map(serde_json::from_value)
        .transpose()
        .context("invalid cache service settings in catalog")?
        .unwrap_or_default();
    let active_wmts_cache = if cache_service_settings.enabled {
        WmtsCache::cangling_tile(&cache_service_settings)
            .map_err(anyhow::Error::msg)
            .context("invalid persisted cache service settings")?
    } else {
        local_wmts_cache.clone()
    };
    let (vector_rasterizer, gpu_rasterizer, gpu_backend, vector_render_backend) =
        configure_vector_rasterizer()?;
    let state = AppState {
        catalog,
        wmts_cache: Arc::new(StdMutex::new(active_wmts_cache)),
        local_wmts_cache,
        cache_service_settings: Arc::new(StdMutex::new(cache_service_settings)),
        vector_rasterizer,
        gpu_rasterizer,
        gpu_backend,
        vector_render_backend,
        render_limit: Arc::new(Semaphore::new(render_concurrency)),
        tile_flights: Arc::new(StdMutex::new(HashMap::new())),
        prepared_vector_sources: Arc::new(Mutex::new(HashSet::new())),
        metrics: Arc::new(ServiceMetrics::default()),
    };
    let app = Router::new()
        .route("/", get(metrics_page))
        .route("/catalog", get(index))
        .route("/assets/styles.css", get(styles))
        .route("/assets/service-frame.js", get(service_frame_script))
        .route("/assets/app.js", get(script))
        .route("/published-layers", get(published_page))
        .route("/assets/published.js", get(published_script))
        .route("/styles", get(style_management_page))
        .route("/style-assets", get(style_assets_page))
        .route("/metrics", get(metrics_page))
        .route("/settings/cache", get(cache_settings_page))
        .route("/api/metrics", get(metrics_snapshot))
        .route(
            "/api/settings/cache-service",
            get(get_cache_service_settings).put(update_cache_service_settings),
        )
        .route("/assets/metrics.js", get(metrics_script))
        .route("/assets/cache-settings.js", get(cache_settings_script))
        .route("/assets/style-assets.js", get(style_assets_script))
        .route("/style-assets/{*path}", get(style_asset_file))
        .route("/assets/style-editor.js", get(style_editor_script))
        .route("/assets/ace/ace.js", get(ace_script))
        .route(
            "/assets/ace/ext-language_tools.js",
            get(ace_language_tools_script),
        )
        .route("/assets/ace/theme-chrome.js", get(ace_theme_script))
        .route("/assets/fonts/iosevka-fixed.woff2", get(iosevka_font))
        .route(
            "/published-layers/{id}/preview",
            get(published_preview_page),
        )
        .route("/layer-groups/{id}/preview", get(published_preview_page))
        .route("/assets/preview.js", get(published_preview_script))
        .route("/assets/openlayers/ol.js", get(openlayers_script))
        .route("/assets/openlayers/ol.css", get(openlayers_styles))
        .route("/layer-groups", get(layer_groups_page))
        .route("/assets/groups.js", get(layer_groups_script))
        .route("/health", get(|| async { "ok" }))
        .route("/wms", get(wms))
        .route("/wmts", get(wmts))
        .route("/api/catalog", get(list_catalog).post(create_catalog_item))
        .route("/api/host-files", axum::routing::post(list_host_files))
        .route("/api/catalog/{id}/features", get(preview_catalog_features))
        .route("/api/catalog/{id}/field-stats", get(catalog_field_stats))
        .route(
            "/api/catalog/inspect",
            axum::routing::post(inspect_catalog_source),
        )
        .route(
            "/api/catalog/{id}",
            axum::routing::put(update_catalog_item).delete(delete_catalog_item),
        )
        .route(
            "/api/published-layers",
            get(list_published_layers).post(create_published_layer),
        )
        .route(
            "/api/published-layers/{id}",
            axum::routing::put(update_published_layer).delete(delete_published_layer),
        )
        .route(
            "/api/published-layers/{id}/cache",
            axum::routing::delete(clear_published_layer_cache),
        )
        .route(
            "/api/published-layers/{id}/preview.png",
            get(published_preview_image),
        )
        .route(
            "/api/layer-groups",
            get(list_layer_groups).post(create_layer_group),
        )
        .route(
            "/api/layer-groups/{id}",
            axum::routing::put(update_layer_group).delete(delete_layer_group),
        )
        .route("/api/styles", get(list_styles).post(create_style))
        .route("/api/styles/validate", axum::routing::post(validate_style))
        .route(
            "/api/style-assets",
            get(list_style_assets).post(upload_style_asset),
        )
        .route(
            "/api/style-assets/{*path}",
            axum::routing::delete(delete_style_asset),
        )
        .route(
            "/api/styles/{id}",
            get(get_style).put(update_style).delete(delete_style),
        )
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .with_state(state)
        .layer(TraceLayer::new_for_http());
    let address = std::env::var("MAP_SERVER_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse::<SocketAddr>()
        .context("invalid MAP_SERVER_ADDRESS")?;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    tracing::info!(%address, "map server listening");
    axum::serve(listener, app).await.context("server failed")
}

fn configure_vector_rasterizer() -> anyhow::Result<(
    Arc<dyn VectorRasterizer>,
    Option<Arc<FallbackVectorRasterizer>>,
    Option<Arc<map_gdal::GpuVectorRasterizer>>,
    String,
)> {
    let backend = std::env::var("MAP_RENDER_BACKEND")
        .unwrap_or_else(|_| "cpu".into())
        .to_ascii_lowercase();
    let cpu: Arc<dyn VectorRasterizer> = Arc::new(map_gdal::GdalVectorRasterizer);
    if backend == "cpu" {
        tracing::info!("vector renderer backend: cpu");
        return Ok((cpu, None, None, "cpu".into()));
    }
    if backend != "auto" && backend != "gpu" {
        anyhow::bail!("MAP_RENDER_BACKEND must be cpu, auto, or gpu");
    }
    let adapter = std::env::var("MAP_GPU_ADAPTER").ok();
    match map_gdal::GpuVectorRasterizer::new(adapter) {
        Ok(gpu) => {
            let adapter_name = gpu.adapter_info().name.clone();
            let gpu = Arc::new(gpu);
            tracing::info!(adapter = %adapter_name, "vector renderer GPU initialized");
            if backend == "gpu" {
                return Ok((
                    gpu.clone(),
                    None,
                    Some(gpu),
                    format!("gpu · {adapter_name}"),
                ));
            }
            let fallback = Arc::new(FallbackVectorRasterizer::new(gpu.clone(), cpu));
            Ok((
                fallback.clone(),
                Some(fallback),
                Some(gpu),
                format!("auto · {adapter_name}"),
            ))
        }
        Err(error) if backend == "auto" => {
            tracing::warn!(%error, "GPU initialization failed; using CPU renderer");
            Ok((cpu, None, None, "auto · cpu fallback".into()))
        }
        Err(error) => Err(anyhow::Error::msg(error.to_string())
            .context("failed to initialize required GPU renderer")),
    }
}

async fn inspect_catalog_source(Json(request): Json<InspectSourceRequest>) -> Response {
    let path = match request.source_config.get("path").and_then(Value::as_str) {
        Some(path) if !path.trim().is_empty() => path.to_owned(),
        _ => return api_message(StatusCode::BAD_REQUEST, "数据源配置中缺少非空的 path 字段"),
    };
    let layer_name = request
        .source_config
        .get("layer")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let encoding = request
        .source_config
        .get("encoding")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kind = match request.source_kind.as_str() {
        "raster" => map_gdal::DatasetKind::Raster,
        "vector" => map_gdal::DatasetKind::Vector,
        _ => return api_message(StatusCode::BAD_REQUEST, "不支持的数据源类型"),
    };
    match tokio::task::spawn_blocking(move || {
        map_gdal::inspect_source(&path, kind, layer_name.as_deref(), encoding.as_deref())
    })
    .await
    {
        Ok(Ok(info)) => Json(info).into_response(),
        Ok(Err(error)) => api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
        Err(error) => api_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("读取数据源失败：{error}"),
        ),
    }
}

async fn list_host_files(headers: HeaderMap, Json(query): Json<HostFilesQuery>) -> Response {
    let requested_with = headers
        .get("x-requested-with")
        .and_then(|value| value.to_str().ok());
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let same_origin = match (host, origin) {
        (Some(host), Some(origin)) => {
            origin == format!("http://{host}") || origin == format!("https://{host}")
        }
        _ => false,
    };
    if requested_with != Some("CanglingMap") || !same_origin {
        return api_message(StatusCode::FORBIDDEN, "主机路径浏览请求来源无效");
    }

    let requested = query.path.as_deref().filter(|path| !path.trim().is_empty());
    let path = match requested
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
        .canonicalize()
    {
        Ok(path) if path.is_dir() => path,
        Ok(_) => return api_message(StatusCode::BAD_REQUEST, "主机路径不是目录"),
        Err(error) => {
            return api_message(
                StatusCode::BAD_REQUEST,
                &format!("无法访问主机路径：{error}"),
            )
        }
    };
    let directory = match std::fs::read_dir(&path) {
        Ok(directory) => directory,
        Err(error) => {
            return api_message(StatusCode::FORBIDDEN, &format!("无法读取主机目录：{error}"))
        }
    };
    let mut entries = directory
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() && !file_type.is_file() {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            let created_unix_ms = metadata
                .created()
                .or_else(|_| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis());
            Some(HostFileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path().to_string_lossy().into_owned(),
                is_dir: file_type.is_dir(),
                size: if file_type.is_file() {
                    metadata.len()
                } else {
                    0
                },
                created_unix_ms,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    let parent = path.parent();
    Json(HostFilesResponse {
        path: path.to_string_lossy().into_owned(),
        parent: parent.map(|parent| parent.to_string_lossy().into_owned()),
        entries,
    })
    .into_response()
}

fn management_page(template: &str) -> Html<String> {
    Html(
        template
            .replace("{{MAP_VERSION}}", env!("CARGO_PKG_VERSION"))
            .replace("{{MAP_BUILD_DATE}}", env!("MAP_BUILD_DATE")),
    )
}
async fn index() -> Html<String> {
    management_page(include_str!("../web/index.html"))
}
async fn published_page() -> Html<String> {
    management_page(include_str!("../web/published.html"))
}
async fn published_preview_page() -> Html<String> {
    management_page(include_str!("../web/preview.html"))
}
async fn layer_groups_page() -> Html<String> {
    management_page(include_str!("../web/groups.html"))
}
async fn style_management_page() -> Html<String> {
    management_page(include_str!("../web/style-editor.html"))
}
async fn style_assets_page() -> Html<String> {
    management_page(include_str!("../web/style-assets.html"))
}
async fn metrics_page() -> Html<String> {
    management_page(include_str!("../web/metrics.html"))
}
async fn cache_settings_page() -> Html<String> {
    management_page(include_str!("../web/cache-settings.html"))
}

async fn get_cache_service_settings(State(state): State<AppState>) -> Response {
    Json(cache_service_settings_view(&state)).into_response()
}

async fn update_cache_service_settings(
    State(state): State<AppState>,
    Json(input): Json<CacheServiceSettingsInput>,
) -> Response {
    let current = state
        .cache_service_settings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let token = if input.clear_token {
        String::new()
    } else {
        input
            .token
            .filter(|token| !token.is_empty())
            .unwrap_or(current.token)
    };
    let settings = CacheServiceSettings {
        enabled: input.enabled,
        base_url: input.base_url.trim().trim_end_matches('/').to_owned(),
        token,
        database_id: input
            .database_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    };
    let backend = if settings.enabled {
        if settings.base_url.is_empty() {
            return api_message(StatusCode::BAD_REQUEST, "启用远程缓存时必须填写服务地址");
        }
        match WmtsCache::cangling_tile(&settings) {
            Ok(cache) => cache,
            Err(error) => return api_message(StatusCode::BAD_REQUEST, &error),
        }
    } else {
        state.local_wmts_cache.clone()
    };
    let setting_value = match serde_json::to_value(&settings) {
        Ok(value) => value,
        Err(error) => {
            return api_message(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("序列化缓存服务设置失败：{error}"),
            )
        }
    };
    if let Err(error) = state
        .catalog
        .put_setting("cache_service", &setting_value)
        .await
    {
        return api_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("保存缓存服务设置失败：{error}"),
        );
    }
    *state
        .cache_service_settings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    *state
        .wmts_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = backend;
    Json(cache_service_settings_view(&state)).into_response()
}

#[derive(Serialize)]
struct MetricsSnapshot {
    uptime_seconds: u64,
    wmts_requests: u64,
    cache_hits: u64,
    cache_misses: u64,
    coalesced_requests: u64,
    rendered_tiles: u64,
    render_failures: u64,
    average_render_ms: f64,
    average_cache_write_ms: f64,
    render_concurrency_available: usize,
    active_tile_requests: usize,
    projection_cache_budget_mb: u64,
    vector_render_backend: String,
    cpu_renderer: CpuRendererStatus,
    gpu_renderer: Option<GpuRendererStatus>,
    cache_backend: CacheBackendStatus,
    layers: Vec<LayerMetricsSnapshot>,
    slow_requests: Vec<SlowTileRequest>,
    recent_wms_requests: Vec<WmsRequestSnapshot>,
    series: Vec<MetricSeriesPoint>,
    render_cache: cangling_gdal_core::RenderCacheStats,
}

#[derive(Serialize)]
struct CpuRendererStatus {
    render_cache_enabled: bool,
    memory_entries: usize,
    memory_resident_bytes: usize,
    memory_hits: u64,
    disk_hits: u64,
    misses: u64,
    writes: u64,
    errors: u64,
}

#[derive(Serialize)]
struct GpuRendererStatus {
    primary_attempts: u64,
    primary_successes: u64,
    fallback_requests: u64,
    primary_failures: u64,
    primary_disabled: bool,
    scene_cache_entries: usize,
    scene_cache_resident_bytes: usize,
    scene_cache_hits: u64,
    scene_cache_misses: u64,
    mesh_disk_enabled: bool,
    mesh_disk_hits: u64,
    mesh_disk_misses: u64,
    mesh_disk_writes: u64,
    mesh_disk_errors: u64,
}

#[derive(Serialize)]
struct CacheBackendStatus {
    backend: &'static str,
    label: String,
    detail: String,
}

#[derive(Serialize)]
struct LayerMetricsSnapshot {
    id: String,
    name: String,
    title: String,
    requests: u64,
    cache_hit_rate: f64,
    rendered_tiles: u64,
    failures: u64,
    coalesced_requests: u64,
    output_bytes: u64,
    average_cache_read_ms: f64,
    average_preparation_ms: f64,
    average_render_ms: f64,
    p95_render_ms: f64,
    average_cache_write_ms: f64,
}

#[derive(Serialize)]
struct MetricSeriesPoint {
    timestamp: u64,
    requests: u64,
    cache_hit_rate: f64,
    average_duration_ms: f64,
}

async fn metrics_snapshot(State(state): State<AppState>) -> Json<MetricsSnapshot> {
    let rendered_tiles = state.metrics.rendered_tiles.load(Ordering::Relaxed);
    let render_time = state.metrics.render_time_micros.load(Ordering::Relaxed);
    let cache_write_time = state
        .metrics
        .cache_write_time_micros
        .load(Ordering::Relaxed);
    let active_tile_requests = state
        .tile_flights
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|flight| flight.strong_count() > 0)
        .count();
    let details = state
        .metrics
        .details
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut layers = details
        .layers
        .iter()
        .map(|(name, metric)| layer_metrics_snapshot(name, metric))
        .collect::<Vec<_>>();
    layers.sort_by(|left, right| right.requests.cmp(&left.requests));
    let slow_requests = details.slow_requests.iter().cloned().collect();
    let recent_wms_requests = details.recent_wms_requests.iter().cloned().collect();
    let series = metric_series(&details.events);
    let cache_backend = active_wmts_cache(&state).status();
    Json(MetricsSnapshot {
        uptime_seconds: state.metrics.started.elapsed().as_secs(),
        wmts_requests: state.metrics.wmts_requests.load(Ordering::Relaxed),
        cache_hits: state.metrics.cache_hits.load(Ordering::Relaxed),
        cache_misses: state.metrics.cache_misses.load(Ordering::Relaxed),
        coalesced_requests: state.metrics.coalesced_requests.load(Ordering::Relaxed),
        rendered_tiles,
        render_failures: state.metrics.render_failures.load(Ordering::Relaxed),
        average_render_ms: average_millis(render_time, rendered_tiles),
        average_cache_write_ms: average_millis(cache_write_time, rendered_tiles),
        render_concurrency_available: state.render_limit.available_permits(),
        active_tile_requests,
        projection_cache_budget_mb: std::env::var("VECTOR_PROJECTION_CACHE_MB")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2048),
        vector_render_backend: state.vector_render_backend.clone(),
        cpu_renderer: {
            let cache = map_gdal::GdalVectorRasterizer::cache_stats();
            CpuRendererStatus {
                render_cache_enabled: cache.enabled,
                memory_entries: cache.memory_entries,
                memory_resident_bytes: cache.memory_resident_bytes,
                memory_hits: cache.memory_hits,
                disk_hits: cache.disk_hits,
                misses: cache.misses,
                writes: cache.writes,
                errors: cache.errors,
            }
        },
        gpu_renderer: state.gpu_backend.as_ref().map(|gpu| {
            let fallback = state
                .gpu_rasterizer
                .as_ref()
                .map(|renderer| renderer.stats())
                .unwrap_or_else(FallbackRasterizerStats::default);
            let cache = gpu.scene_cache_stats();
            GpuRendererStatus {
                primary_attempts: fallback.primary_attempts,
                primary_successes: fallback.primary_successes,
                fallback_requests: fallback.fallback_requests,
                primary_failures: fallback.primary_failures,
                primary_disabled: fallback.primary_disabled,
                scene_cache_entries: cache.entries,
                scene_cache_resident_bytes: cache.resident_bytes,
                scene_cache_hits: cache.hits,
                scene_cache_misses: cache.misses,
                mesh_disk_enabled: cache.mesh_disk_enabled,
                mesh_disk_hits: cache.mesh_disk_hits,
                mesh_disk_misses: cache.mesh_disk_misses,
                mesh_disk_writes: cache.mesh_disk_writes,
                mesh_disk_errors: cache.mesh_disk_errors,
            }
        }),
        cache_backend,
        layers,
        slow_requests,
        recent_wms_requests,
        series,
        render_cache: GdalEngine.render_cache_stats(),
    })
}

fn layer_metrics_snapshot(name: &str, metric: &LayerMetricState) -> LayerMetricsSnapshot {
    let lookups = metric.cache_hits + metric.cache_misses;
    let mut samples = metric
        .render_samples_micros
        .iter()
        .copied()
        .collect::<Vec<_>>();
    samples.sort_unstable();
    let p95 = samples
        .get(samples.len().saturating_sub(1) * 95 / 100)
        .copied()
        .unwrap_or(0);
    LayerMetricsSnapshot {
        id: metric.id.clone(),
        name: name.to_owned(),
        title: metric.title.clone(),
        requests: metric.requests,
        cache_hit_rate: ratio(metric.cache_hits, lookups),
        rendered_tiles: metric.rendered,
        failures: metric.failures,
        coalesced_requests: metric.coalesced,
        output_bytes: metric.output_bytes,
        average_cache_read_ms: average_millis(metric.cache_read_micros, metric.requests),
        average_preparation_ms: average_millis(metric.preparation_micros, metric.rendered),
        average_render_ms: average_millis(metric.render_micros, metric.rendered),
        p95_render_ms: p95 as f64 / 1000.0,
        average_cache_write_ms: average_millis(metric.cache_write_micros, metric.rendered),
    }
}

fn metric_series(events: &VecDeque<MetricEvent>) -> Vec<MetricSeriesPoint> {
    let current_minute = unix_seconds() / 60 * 60;
    (0..15)
        .rev()
        .map(|offset| {
            let timestamp = current_minute.saturating_sub(offset * 60);
            let bucket = events
                .iter()
                .filter(|event| event.timestamp >= timestamp && event.timestamp < timestamp + 60)
                .collect::<Vec<_>>();
            let requests = bucket.len() as u64;
            let hits = bucket.iter().filter(|event| event.cache_hit).count() as u64;
            let duration = bucket.iter().map(|event| event.duration_micros).sum();
            MetricSeriesPoint {
                timestamp,
                requests,
                cache_hit_rate: ratio(hits, requests),
                average_duration_ms: average_millis(duration, requests),
            }
        })
        .collect()
}

fn ratio(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64
    }
}

fn average_millis(total_micros: u64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        total_micros as f64 / count as f64 / 1000.0
    }
}
async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/styles.css"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}
async fn service_frame_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/service-frame.js"),
    )
}
async fn published_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/published.js"),
    )
}
async fn style_editor_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/style-editor.js"),
    )
}
async fn style_assets_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/style-assets.js"),
    )
}
async fn metrics_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/metrics.js"),
    )
}
async fn cache_settings_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/cache-settings.js"),
    )
}
async fn ace_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_bytes!("../web/vendor/ace/ace.js").as_slice(),
    )
}
async fn ace_language_tools_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_bytes!("../web/vendor/ace/ext-language_tools.js").as_slice(),
    )
}
async fn ace_theme_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_bytes!("../web/vendor/ace/theme-chrome.js").as_slice(),
    )
}
async fn iosevka_font() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        include_bytes!("../web/vendor/iosevka/Iosevka-Fixed.woff2").as_slice(),
    )
}

#[derive(Debug, Serialize)]
struct StyleAssetInfo {
    path: String,
    name: String,
    category: String,
    size: u64,
    modified_unix_ms: u128,
}

#[derive(Debug, Deserialize)]
struct UploadStyleAssetQuery {
    path: String,
}

fn style_asset_root() -> Result<std::path::PathBuf, String> {
    let root = std::env::var_os("STYLE_ASSET_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("data/style-assets"));
    std::fs::create_dir_all(&root).map_err(|error| format!("无法创建图片资源目录：{error}"))?;
    root.canonicalize()
        .map_err(|error| format!("无法访问图片资源目录：{error}"))
}

fn checked_style_asset_path(relative: &str) -> Result<std::path::PathBuf, String> {
    let relative = std::path::Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("图片路径无效".into());
    }
    let extension = relative
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("png") && !extension.eq_ignore_ascii_case("webp") {
        return Err("只允许 PNG 或 WebP 图片".into());
    }
    Ok(style_asset_root()?.join(relative))
}

fn collect_style_assets(
    root: &std::path::Path,
    directory: &std::path::Path,
    output: &mut Vec<StyleAssetInfo>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            collect_style_assets(root, &path, output)?;
            continue;
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if !extension.eq_ignore_ascii_case("png") && !extension.eq_ignore_ascii_case("webp") {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        let category = relative
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| "未分类".into());
        output.push(StyleAssetInfo {
            path: relative.to_string_lossy().replace('\\', "/"),
            name: entry.file_name().to_string_lossy().into_owned(),
            category,
            size: metadata.len(),
            modified_unix_ms: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_millis()),
        });
    }
    Ok(())
}

async fn list_style_assets() -> Response {
    match tokio::task::spawn_blocking(|| {
        let root = style_asset_root()?;
        let mut assets = Vec::new();
        collect_style_assets(&root, &root, &mut assets)?;
        assets.sort_by(|left, right| left.path.cmp(&right.path));
        Ok::<_, String>(assets)
    })
    .await
    {
        Ok(Ok(assets)) => Json(assets).into_response(),
        Ok(Err(error)) => api_message(StatusCode::INTERNAL_SERVER_ERROR, &error),
        Err(error) => api_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn upload_style_asset(
    Query(query): Query<UploadStyleAssetQuery>,
    body: AxumBytes,
) -> Response {
    if body.is_empty() || body.len() > 10 * 1024 * 1024 {
        return api_message(
            StatusCode::BAD_REQUEST,
            "图片大小必须在 1 字节到 10 MB 之间",
        );
    }
    let expected = match checked_style_asset_path(&query.path) {
        Ok(path) => path,
        Err(error) => return api_message(StatusCode::BAD_REQUEST, &error),
    };
    let format = match image::guess_format(&body) {
        Ok(image::ImageFormat::Png) => image::ImageFormat::Png,
        Ok(image::ImageFormat::WebP) => image::ImageFormat::WebP,
        _ => {
            return api_message(
                StatusCode::BAD_REQUEST,
                "文件内容不是有效的 PNG 或 WebP 图片",
            )
        }
    };
    if image::load_from_memory_with_format(&body, format).is_err() {
        return api_message(StatusCode::BAD_REQUEST, "图片无法解码或文件已损坏");
    }
    let parent = expected.parent().expect("validated asset path has parent");
    if let Err(error) = tokio::fs::create_dir_all(parent).await {
        return api_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }
    let root = match style_asset_root() {
        Ok(root) => root,
        Err(error) => return api_message(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let canonical_parent = match parent.canonicalize() {
        Ok(parent) if parent.starts_with(&root) => parent,
        _ => return api_message(StatusCode::BAD_REQUEST, "图片目录超出资源根目录"),
    };
    let target = canonical_parent.join(expected.file_name().expect("validated asset filename"));
    if target.exists() {
        return api_message(StatusCode::CONFLICT, "同名图片已经存在");
    }
    match tokio::fs::write(target, body).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"message":"图片上传成功"})),
        )
            .into_response(),
        Err(error) => api_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn delete_style_asset(Path(path): Path<String>) -> Response {
    let target = match checked_style_asset_path(&path) {
        Ok(path) => path,
        Err(error) => return api_message(StatusCode::BAD_REQUEST, &error),
    };
    let root = match style_asset_root() {
        Ok(root) => root,
        Err(error) => return api_message(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let canonical = match target.canonicalize() {
        Ok(path) if path.starts_with(&root) => path,
        Ok(_) => return api_message(StatusCode::BAD_REQUEST, "图片路径超出资源根目录"),
        Err(_) => return api_message(StatusCode::NOT_FOUND, "图片不存在"),
    };
    match tokio::fs::remove_file(canonical).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => api_message(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn style_asset_file(Path(path): Path<String>) -> Response {
    let target = match checked_style_asset_path(&path) {
        Ok(path) => path,
        Err(error) => return api_message(StatusCode::BAD_REQUEST, &error),
    };
    let root = match style_asset_root() {
        Ok(root) => root,
        Err(error) => return api_message(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let canonical = match target.canonicalize() {
        Ok(path) if path.starts_with(&root) => path,
        _ => return api_message(StatusCode::NOT_FOUND, "图片不存在"),
    };
    let content_type = if canonical
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("webp"))
    {
        "image/webp"
    } else {
        "image/png"
    };
    match tokio::fs::read(canonical).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => api_message(StatusCode::NOT_FOUND, "图片不存在"),
    }
}
async fn published_preview_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/preview.js"),
    )
}
async fn openlayers_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_bytes!("../web/vendor/openlayers/ol.js").as_slice(),
    )
}
async fn openlayers_styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/vendor/openlayers/ol.css"),
    )
}

async fn published_preview_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let layer = match state.catalog.get_published_layer(&id).await {
        Ok(Some(layer)) => layer,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "发布图层不存在"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let item = match state.catalog.get_item(layer.catalog_item_id).await {
        Ok(Some(item)) => item,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "关联目录项不存在"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let Some(path) = item.source_config.get("path").and_then(Value::as_str) else {
        return api_message(StatusCode::UNPROCESSABLE_ENTITY, "数据源配置缺少 path");
    };
    let path = path.to_owned();
    let source_layer = item
        .source_config
        .get("layer")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source_encoding = item
        .source_config
        .get("encoding")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source_kind = item.source_kind;
    let rendering_style = match vector_rendering_style(&state, &layer, true).await {
        Ok(style) => style,
        Err(error) => return api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
    };
    let extent = item.native_extent;
    let target_crs = item.native_crs.0;
    let vector_rasterizer = state.vector_rasterizer.clone();
    let height = ((1200.0 * (extent.max_y - extent.min_y) / (extent.max_x - extent.min_x)).round()
        as u32)
        .max(1);
    match tokio::task::spawn_blocking(move || {
        let source = GdalDataSource {
            path: path.into(),
            layer: source_layer,
            encoding: source_encoding,
        };
        let bbox = GdalBoundingBox {
            min_x: extent.min_x,
            min_y: extent.min_y,
            max_x: extent.max_x,
            max_y: extent.max_y,
        };
        match source_kind {
            SourceKind::Raster => GdalEngine
                .render(&RasterRenderRequest {
                    source,
                    target_crs,
                    bbox,
                    width: 1200,
                    height,
                    resampling: Resampling::Bilinear,
                    format: ImageFormat::Png,
                    nodata: Some(0.0),
                })
                .map(|image| (image.bytes, image.content_type))
                .map_err(|error| error.to_string()),
            SourceKind::Vector => vector_rasterizer
                .render(&render_vector_request(
                    source,
                    target_crs,
                    bbox,
                    1200,
                    height,
                    0,
                    None,
                    ImageFormat::Png,
                    rendering_style,
                ))
                .map(|image| (image.bytes, image.content_type))
                .map_err(|error| error.to_string()),
        }
    })
    .await
    {
        Ok(Ok((bytes, content_type))) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "private, max-age=60"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => api_message(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
        Err(e) => api_message(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn vector_rendering_style(
    state: &AppState,
    layer: &PublishedLayer,
    transparent: bool,
) -> Result<VectorRenderingStyle, String> {
    vector_rendering_style_with_id(state, layer.default_style_id.as_deref(), transparent).await
}

async fn vector_rendering_style_with_id(
    state: &AppState,
    style_id: Option<&str>,
    transparent: bool,
) -> Result<VectorRenderingStyle, String> {
    let Some(style_id) = style_id else {
        let mut palette = VectorRenderingStyle::default();
        if transparent {
            palette.background = [0, 0, 0, 0];
        }
        return Ok(palette);
    };
    let record = state
        .catalog
        .get_style(style_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("默认样式不存在：{style_id}"))?;
    let warning_scope = format!("{}:{}:r{}", record.app_id, record.name, record.revision);
    let style: Style = serde_json::from_value(record.compiled_ir)
        .map_err(|error| format!("样式编译结果无效：{error}"))?;
    rendering_style_from_style_scoped(&style, transparent, &warning_scope)
}

fn compose_images(
    width: u32,
    height: u32,
    background: [u8; 4],
    layers: Vec<(RasterizedRgba, f64)>,
    format: ImageFormat,
) -> Result<cangling_gdal_core::RenderedImage, String> {
    use image::{DynamicImage, ImageFormat as EncoderFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    let mut canvas = RgbaImage::from_pixel(width, height, Rgba(background));
    for (layer, opacity) in layers {
        if (layer.width, layer.height) != (width, height) {
            return Err("成员图层输出尺寸不一致".into());
        }
        let image = RgbaImage::from_raw(layer.width, layer.height, layer.pixels)
            .ok_or_else(|| "成员图层 RGBA 数据长度无效".to_owned())?;
        let opacity = opacity.clamp(0.0, 1.0) as f32;
        for (dst, src) in canvas.pixels_mut().zip(image.pixels()) {
            let source_alpha = src[3] as f32 / 255.0 * opacity;
            let destination_alpha = dst[3] as f32 / 255.0;
            let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
            if output_alpha <= f32::EPSILON {
                *dst = Rgba([0, 0, 0, 0]);
                continue;
            }
            for channel in 0..3 {
                dst[channel] = ((src[channel] as f32 * source_alpha
                    + dst[channel] as f32 * destination_alpha * (1.0 - source_alpha))
                    / output_alpha)
                    .round() as u8;
            }
            dst[3] = (output_alpha * 255.0).round() as u8;
        }
    }
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(canvas)
        .write_to(
            &mut output,
            match format {
                ImageFormat::Png => EncoderFormat::Png,
                ImageFormat::Jpeg => EncoderFormat::Jpeg,
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(cangling_gdal_core::RenderedImage {
        bytes: output.into_inner(),
        content_type: match format {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        },
    })
}

#[cfg(test)]
fn rendering_style_from_style(
    style: &Style,
    transparent: bool,
) -> Result<VectorRenderingStyle, String> {
    rendering_style_from_style_scoped(style, transparent, &style.name)
}

fn rendering_style_from_style_scoped(
    style: &Style,
    transparent: bool,
    warning_scope: &str,
) -> Result<VectorRenderingStyle, String> {
    if style.rules.is_empty() {
        return Err("样式没有可渲染规则".to_owned());
    }
    let rgba = |color: map_core::Rgba, opacity: f32| {
        [
            color.red,
            color.green,
            color.blue,
            (color.alpha as f32 * opacity.clamp(0.0, 1.0)).round() as u8,
        ]
    };
    let to_color = |value: [u8; 4]| map_core::Rgba {
        red: value[0],
        green: value[1],
        blue: value[2],
        alpha: value[3],
    };
    let rules = style
        .rules
        .iter()
        .map(|rule| {
            let color = |expression: &ColorExpression, opacity| match expression {
                ColorExpression::Literal(value) => {
                    VectorColor::Literal(to_color(rgba(*value, opacity)))
                }
                ColorExpression::Property { property } => VectorColor::Property {
                    property: property.clone(),
                    opacity,
                },
            };
            let mut fill = rule
                .symbolizer
                .fill
                .as_ref()
                .map(|value| color(value, rule.symbolizer.fill_opacity));
            let stroke = rule
                .symbolizer
                .stroke
                .as_ref()
                .map(|value| color(value, rule.symbolizer.stroke_opacity));
            let fill_image = if let Some(path) = rule.symbolizer.fill_image.as_deref() {
                let asset = checked_style_asset_path(path)
                    .map_err(|error| format!("填充图片路径无效 `{path}`：{error}"))?;
                if asset.is_file() {
                    Some(VectorImageFill {
                        path: path.into(),
                        scale: rule.symbolizer.fill_image_scale,
                        opacity: rule.symbolizer.fill_opacity,
                    })
                } else {
                    let alpha =
                        (255.0 * rule.symbolizer.fill_opacity.clamp(0.0, 1.0)).round() as u8;
                    fill = Some(VectorColor::Literal(map_core::Rgba {
                        red: 176,
                        green: 176,
                        blue: 176,
                        alpha,
                    }));
                    warn_missing_style_asset(warning_scope, path);
                    None
                }
            } else {
                None
            };
            let text = rule.symbolizer.label.as_ref().map(|field| VectorTextStyle {
                field: field.clone(),
                font_family: rule.symbolizer.font_family.clone(),
                font_size: rule.symbolizer.font_size,
                font_italic: rule.symbolizer.font_italic,
                font_bold: rule.symbolizer.font_bold,
                fill: rule.symbolizer.font_fill,
                halo: rule.symbolizer.halo_color,
                halo_radius: rule.symbolizer.halo_radius,
                spacing: rule.symbolizer.label_spacing,
                min_area: rule.symbolizer.label_min_area,
                repeat_distance: rule.symbolizer.label_repeat_distance,
                repeat_count: rule.symbolizer.label_repeat_count,
            });
            if fill.is_none() && fill_image.is_none() && stroke.is_none() && text.is_none() {
                return Err("样式规则没有面、线或文字符号".to_owned());
            }
            Ok(VectorRenderRule {
                filter: rule.filter.clone(),
                minimum_scale_denominator: rule.minimum_scale_denominator,
                maximum_scale_denominator: rule.maximum_scale_denominator,
                paint: VectorPaint {
                    fill,
                    fill_image,
                    stroke,
                    stroke_width: rule.symbolizer.stroke_width,
                    stroke_dasharray: rule.symbolizer.stroke_dasharray.clone(),
                    text,
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(VectorRenderingStyle {
        rules,
        background: if transparent {
            [0, 0, 0, 0]
        } else {
            [255, 255, 255, 255]
        },
    })
}

fn warn_missing_style_asset(scope: &str, path: &str) {
    let key = format!("{scope}\0{path}");
    let warnings =
        MISSING_STYLE_ASSET_WARNINGS.get_or_init(|| std::sync::Mutex::new(HashSet::new()));
    let should_warn = warnings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key);
    if should_warn {
        tracing::warn!(
            style = scope,
            fill_image = path,
            "style fill image is missing; rendering a gray fallback"
        );
    }
}

fn scale_rendering_style(mut style: VectorRenderingStyle, scale: f32) -> VectorRenderingStyle {
    if (scale - 1.0).abs() <= f32::EPSILON {
        return style;
    }
    let area_scale = scale * scale;
    for rule in &mut style.rules {
        rule.paint.stroke_width *= scale;
        for dash in &mut rule.paint.stroke_dasharray {
            *dash *= scale;
        }
        if let Some(image) = &mut rule.paint.fill_image {
            image.scale *= scale;
        }
        if let Some(text) = &mut rule.paint.text {
            text.font_size *= scale;
            text.halo_radius *= scale;
            text.spacing *= scale;
            text.min_area *= area_scale;
            text.repeat_distance *= scale;
        }
    }
    style
}

fn render_vector_request(
    source: GdalDataSource,
    target_crs: String,
    bbox: GdalBoundingBox,
    width: u32,
    height: u32,
    buffer_pixels: u32,
    simplify_tolerance: Option<f64>,
    format: ImageFormat,
    style: VectorRenderingStyle,
) -> RenderVectorRequest {
    let rgba = |value: [u8; 4]| map_core::Rgba {
        red: value[0],
        green: value[1],
        blue: value[2],
        alpha: value[3],
    };
    let scale_denominator = scale_denominator(&target_crs, &bbox, width, height);
    let buffer_pixels = if style.rules.iter().any(|rule| rule.paint.text.is_some()) {
        buffer_pixels.max(64)
    } else {
        buffer_pixels
    };
    RenderVectorRequest {
        source: VectorSource {
            path: source.path,
            layer: source.layer,
            encoding: source.encoding,
        },
        target_crs,
        bbox: map_core::BoundingBox {
            min_x: bbox.min_x,
            min_y: bbox.min_y,
            max_x: bbox.max_x,
            max_y: bbox.max_y,
        },
        width,
        height,
        buffer_pixels,
        allow_view_reuse: false,
        simplify_tolerance,
        format: match format {
            ImageFormat::Png => RenderImageFormat::Png,
            ImageFormat::Jpeg => RenderImageFormat::Jpeg,
        },
        rules: style.rules,
        scale_denominator,
        background: rgba(style.background),
    }
}

fn scale_denominator(crs: &str, bbox: &GdalBoundingBox, width: u32, height: u32) -> f64 {
    let resolution = vector_resolution(bbox, width, height);
    let metres_per_unit =
        if crs.eq_ignore_ascii_case("EPSG:4326") || crs.eq_ignore_ascii_case("CRS:84") {
            111_319.490_793_273_58
        } else {
            1.0
        };
    resolution * metres_per_unit / 0.00028
}

fn vector_resolution(bbox: &GdalBoundingBox, width: u32, height: u32) -> f64 {
    ((bbox.max_x - bbox.min_x).abs() / f64::from(width))
        .max((bbox.max_y - bbox.min_y).abs() / f64::from(height))
}

fn scale_is_visible(minimum: Option<f64>, maximum: Option<f64>, scale: f64) -> bool {
    minimum.is_none_or(|minimum| scale >= minimum) && maximum.is_none_or(|maximum| scale < maximum)
}

fn configured_simplify_tolerance(factor: Option<f64>, resolution: f64) -> Option<f64> {
    factor
        .filter(|factor| *factor > 0.0)
        .map(|factor| factor * resolution)
}
async fn layer_groups_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/groups.js"),
    )
}

fn compile_style(input: StyleInput) -> Result<StyleRecord, String> {
    if input.geometry_type != "polygon" {
        return Err("当前阶段仅支持 polygon 样式".into());
    }
    if input.source_format != "geocss" {
        return Err("当前阶段仅支持 geocss 源格式".into());
    }
    let compiled = compile_geocss(&input.name, &input.source_text).map_err(|e| e.to_string())?;
    let compiled_sld = to_sld_1_1(&compiled, &input.name).map_err(|e| e.to_string())?;
    let compiled_ir = serde_json::to_value(&compiled).map_err(|e| e.to_string())?;
    let content_hash =
        sha256_hex(format!("{STYLE_COMPILER_VERSION}\0{}", input.source_text).as_bytes());
    let id = StyleRecord::derive_id(&input.app_id, &input.name).map_err(|e| e.to_string())?;
    Ok(StyleRecord {
        id,
        app_id: input.app_id,
        name: input.name,
        title: input.title,
        description: input.description,
        geometry_type: input.geometry_type,
        source_format: input.source_format,
        source_text: input.source_text,
        compiled_ir,
        compiled_sld,
        compiler_version: STYLE_COMPILER_VERSION.into(),
        revision: 1,
        content_hash,
        validation_error: None,
    })
}

async fn validate_style(Json(input): Json<StyleInput>) -> Response {
    match compile_style(input) {
        Ok(style) => Json(style).into_response(),
        Err(error) => api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct ListQuery {
    page: Option<usize>,
    page_size: Option<usize>,
    q: Option<String>,
}

fn paged_list_response<T: Serialize>(items: Vec<T>, query: &ListQuery) -> Response {
    if query.page.is_none() && query.page_size.is_none() && query.q.is_none() {
        return Json(items).into_response();
    }
    let page_size = query.page_size.unwrap_or(20).clamp(5, 200);
    let total = items.len();
    let total_pages = total.div_ceil(page_size).max(1);
    let page = query.page.unwrap_or(1).clamp(1, total_pages);
    let offset = (page - 1) * page_size;
    let items = items
        .into_iter()
        .skip(offset)
        .take(page_size)
        .collect::<Vec<_>>();
    Json(serde_json::json!({
        "items": items,
        "page": page,
        "page_size": page_size,
        "total": total,
        "total_pages": total_pages
    }))
    .into_response()
}

fn contains_query(values: &[&str], query: &ListQuery) -> bool {
    let Some(term) = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };
    let term = term.to_lowercase();
    values
        .iter()
        .any(|value| value.to_lowercase().contains(&term))
}

async fn list_styles(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    match state.catalog.list_styles().await {
        Ok(styles) => paged_list_response(
            styles
                .into_iter()
                .filter(|item| contains_query(&[&item.app_id, &item.name, &item.title], &query))
                .collect(),
            &query,
        ),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn get_style(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.catalog.get_style(&id).await {
        Ok(Some(style)) => Json(style).into_response(),
        Ok(None) => api_message(StatusCode::NOT_FOUND, "样式不存在"),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn create_style(State(state): State<AppState>, Json(input): Json<StyleInput>) -> Response {
    let style = match compile_style(input) {
        Ok(style) => style,
        Err(error) => return api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
    };
    match state.catalog.put_style(&style).await {
        Ok(style) => (StatusCode::CREATED, Json(style)).into_response(),
        Err(error) => api_error(StatusCode::UNPROCESSABLE_ENTITY, error),
    }
}

async fn update_style(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<StyleInput>,
) -> Response {
    let existing = match state.catalog.get_style(&id).await {
        Ok(Some(style)) => style,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "样式不存在"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if input.app_id != existing.app_id || input.name != existing.name {
        return api_message(StatusCode::CONFLICT, "应用 ID 和样式名称不可修改");
    }
    let style = match compile_style(input) {
        Ok(style) => style,
        Err(error) => return api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
    };
    match state.catalog.put_style(&style).await {
        Ok(style) => {
            clear_layers_using_style(&state, &id).await;
            Json(style).into_response()
        }
        Err(error) => api_error(StatusCode::UNPROCESSABLE_ENTITY, error),
    }
}

async fn delete_style(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.catalog.delete_style(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_message(StatusCode::NOT_FOUND, "样式不存在"),
        Err(error) => api_error(StatusCode::CONFLICT, error),
    }
}

#[derive(Serialize)]
struct CacheClearResult {
    backend: &'static str,
    cleared_targets: usize,
    message: String,
}

async fn clear_published_layer_cache(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let layer = match state.catalog.get_published_layer(&id).await {
        Ok(Some(layer)) => layer,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "发布图层不存在"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    match clear_layer_cache(&state, &layer).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => api_message(StatusCode::BAD_GATEWAY, &error),
    }
}

async fn layer_cache_location(
    state: &AppState,
    layer: &PublishedLayer,
) -> Result<(String, String), String> {
    let cache = active_wmts_cache(state);
    let database = match &cache {
        WmtsCache::CanglingTile {
            database_id: Some(id),
            ..
        } => id.clone(),
        _ => sha256_hex(layer.app_id.as_bytes()),
    };
    Ok((database, layer_cache_item_id(layer)))
}

fn layer_cache_item_id(layer: &PublishedLayer) -> String {
    sha256_hex(layer.id.as_bytes())
}

fn pixel_ratio_cache_item(item: &str, pixel_ratio: u8) -> String {
    if pixel_ratio == 1 {
        item.to_owned()
    } else {
        sha256_hex(format!("{item}\0{pixel_ratio}x").as_bytes())
    }
}

async fn clear_layers_using_style(state: &AppState, style_id: &str) {
    let Ok(layers) = state.catalog.list_published_layers().await else {
        tracing::warn!(%style_id, "failed to list published layers for style cache invalidation");
        return;
    };
    for layer in layers
        .iter()
        .filter(|layer| layer.default_style_id.as_deref() == Some(style_id))
    {
        if let Err(error) = clear_layer_cache(state, layer).await {
            tracing::warn!(%error, layer_id = %layer.id, %style_id, "failed to invalidate style cache");
        }
    }
}

async fn clear_layers_using_catalog_item(state: &AppState, catalog_item_id: CatalogItemId) {
    let Ok(layers) = state.catalog.list_published_layers().await else {
        tracing::warn!(%catalog_item_id, "failed to list published layers for catalog cache invalidation");
        return;
    };
    for layer in layers
        .iter()
        .filter(|layer| layer.catalog_item_id == catalog_item_id)
    {
        if let Err(error) = clear_layer_cache(state, layer).await {
            tracing::warn!(%error, layer_id = %layer.id, %catalog_item_id, "failed to invalidate catalog cache");
        }
    }
}

async fn clear_layer_cache(
    state: &AppState,
    layer: &PublishedLayer,
) -> Result<CacheClearResult, String> {
    let (database, item) = layer_cache_location(state, layer).await?;
    let cache = active_wmts_cache(state);
    match &cache {
        WmtsCache::None => Ok(CacheClearResult {
            backend: "none",
            cleared_targets: 0,
            message: "当前未启用瓦片缓存".into(),
        }),
        WmtsCache::Filesystem { root } => {
            let targets = [
                root.join(&database).join(&item),
                root.join(&database).join(pixel_ratio_cache_item(&item, 2)),
            ];
            tokio::task::spawn_blocking(move || {
                let mut cleared = 0;
                for target in targets {
                    cleared += clear_filesystem_layer_cache(&target)?;
                }
                Ok(CacheClearResult {
                    backend: "filesystem",
                    cleared_targets: cleared,
                    message: "该图层的本地 WMTS 缓存已清空".into(),
                })
            })
            .await
            .map_err(|error| error.to_string())?
        }
        WmtsCache::CanglingTile {
            base_url,
            token,
            client,
            ..
        } => {
            let cache_items = [item.clone(), pixel_ratio_cache_item(&item, 2)];
            let mut cleared = 0;
            for cache_item in cache_items {
                let url = format!(
                    "{base_url}/api/v1/databases/{}/tilesets/{}",
                    percent_encode(&database),
                    percent_encode(&cache_item)
                );
                let mut builder = hyper::Request::delete(url);
                if !token.is_empty() {
                    builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
                }
                let response = client
                    .request(
                        builder
                            .body(Full::new(Bytes::new()))
                            .map_err(|error| error.to_string())?,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                cleared += usize::from(response.status().is_success());
                if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
                    return Err(format!(
                        "cangling-tile DELETE returned {} for tileset {cache_item}",
                        response.status()
                    ));
                }
            }
            Ok(CacheClearResult {
                backend: "cangling-tile",
                cleared_targets: cleared,
                message: "该图层的 Cangling Tile 缓存已清空".into(),
            })
        }
    }
}

fn clear_filesystem_layer_cache(target: &std::path::Path) -> Result<usize, String> {
    match std::fs::remove_dir_all(target) {
        Ok(()) => Ok(1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.to_string()),
    }
}

async fn list_layer_groups(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Response {
    match state.catalog.list_layer_groups().await {
        Ok(groups) => paged_list_response(
            groups
                .into_iter()
                .filter(|item| contains_query(&[&item.app_id, &item.name, &item.title], &query))
                .collect(),
            &query,
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn create_layer_group(
    State(state): State<AppState>,
    Json(group): Json<LayerGroup>,
) -> Response {
    match state.catalog.put_layer_group(&group).await {
        Ok(group) => (StatusCode::CREATED, Json(group)).into_response(),
        Err(e) => api_error(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}
async fn update_layer_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut group): Json<LayerGroup>,
) -> Response {
    let existing = match state.catalog.get_layer_group(&id).await {
        Ok(Some(group)) => group,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "图层组不存在"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if group.app_id != existing.app_id || group.name != existing.name {
        return api_message(StatusCode::CONFLICT, "应用 ID 和组名称不可修改");
    }
    group.id = id;
    match state.catalog.put_layer_group(&group).await {
        Ok(group) => Json(group).into_response(),
        Err(e) => api_error(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}
async fn delete_layer_group(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.catalog.delete_layer_group(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_message(StatusCode::NOT_FOUND, "图层组不存在"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn list_published_layers(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Response {
    match state.catalog.list_published_layers().await {
        Ok(items) => paged_list_response(
            items
                .into_iter()
                .filter(|item| contains_query(&[&item.app_id, &item.name, &item.title], &query))
                .collect(),
            &query,
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn create_published_layer(
    State(state): State<AppState>,
    Json(layer): Json<PublishedLayer>,
) -> Response {
    match state.catalog.put_published_layer(&layer).await {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => api_error(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}
async fn update_published_layer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut layer): Json<PublishedLayer>,
) -> Response {
    let existing = match state.catalog.get_published_layer(&id).await {
        Ok(Some(v)) => v,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "发布图层不存在"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if layer.app_id != existing.app_id || layer.name != existing.name {
        return api_message(
            StatusCode::CONFLICT,
            "应用 ID 和 WMS 图层名称不可修改；请创建新的发布图层",
        );
    }
    layer.id = id;
    match state.catalog.put_published_layer(&layer).await {
        Ok(item) => {
            if item != existing {
                if let Err(error) = clear_layer_cache(&state, &item).await {
                    tracing::warn!(%error, layer_id = %item.id, "failed to invalidate updated layer cache");
                }
            }
            Json(item).into_response()
        }
        Err(e) => api_error(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}
async fn delete_published_layer(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.catalog.delete_published_layer(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_message(StatusCode::NOT_FOUND, "发布图层不存在"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn list_catalog(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    let items = match state.catalog.list_items().await {
        Ok(items) => items,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let published = match state.catalog.list_published_layers().await {
        Ok(published) => published,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let mut reference_counts = HashMap::new();
    for layer in published {
        *reference_counts
            .entry(layer.catalog_item_id)
            .or_insert(0usize) += 1;
    }
    paged_list_response(
        items
            .into_iter()
            .filter(|item| contains_query(&[&item.app_id, &item.catalog_path, &item.title], &query))
            .map(|item| CatalogItemView {
                reference_count: item
                    .id
                    .and_then(|id| reference_counts.get(&id).copied())
                    .unwrap_or_default(),
                item,
            })
            .collect(),
        &query,
    )
}

async fn preview_catalog_features(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let id = match CatalogItemId::from_str(&id) {
        Ok(id) => id,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
    };
    let item = match state.catalog.get_item(id).await {
        Ok(Some(item)) => item,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "目录项不存在"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if item.source_kind != SourceKind::Vector {
        return api_message(
            StatusCode::UNPROCESSABLE_ENTITY,
            "只有矢量数据源可以预览属性记录",
        );
    }
    let Some(path) = item
        .source_config
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return api_message(StatusCode::UNPROCESSABLE_ENTITY, "数据源配置缺少 path");
    };
    let layer = item
        .source_config
        .get("layer")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let encoding = item
        .source_config
        .get("encoding")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match tokio::task::spawn_blocking(move || {
        map_gdal::preview_vector_features(&path, layer.as_deref(), encoding.as_deref(), 10)
    })
    .await
    {
        Ok(Ok(preview)) => Json(preview).into_response(),
        Ok(Err(error)) => api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
        Err(error) => api_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("读取数据记录失败：{error}"),
        ),
    }
}

async fn catalog_field_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FieldStatsQuery>,
) -> Response {
    let id = match CatalogItemId::from_str(&id) {
        Ok(id) => id,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
    };
    let item = match state.catalog.get_item(id).await {
        Ok(Some(item)) => item,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "目录项不存在"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    if item.source_kind != SourceKind::Vector {
        return api_message(
            StatusCode::UNPROCESSABLE_ENTITY,
            "只有矢量数据源可以统计字段",
        );
    }
    let Some(path) = item
        .source_config
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return api_message(StatusCode::UNPROCESSABLE_ENTITY, "数据源配置缺少 path");
    };
    let layer = item
        .source_config
        .get("layer")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let encoding = item
        .source_config
        .get("encoding")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let field = query.field;
    match tokio::task::spawn_blocking(move || {
        map_gdal::vector_field_stats(&path, layer.as_deref(), encoding.as_deref(), &field)
    })
    .await
    {
        Ok(Ok(stats)) => Json(stats).into_response(),
        Ok(Err(error)) => api_message(StatusCode::UNPROCESSABLE_ENTITY, &error),
        Err(error) => api_message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("统计字段失败：{error}"),
        ),
    }
}

async fn create_catalog_item(
    State(state): State<AppState>,
    Json(mut item): Json<CatalogItem>,
) -> Response {
    item.id = None;
    match state.catalog.put_item(&item).await {
        Ok(item) => (StatusCode::CREATED, Json(item)).into_response(),
        Err(e) => api_error(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}

async fn update_catalog_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut item): Json<CatalogItem>,
) -> Response {
    let id = match CatalogItemId::from_str(&id) {
        Ok(id) => id,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };
    let existing = match state.catalog.get_item(id).await {
        Ok(Some(item)) => item,
        Ok(None) => return api_message(StatusCode::NOT_FOUND, "catalog item not found"),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if item.app_id != existing.app_id || item.catalog_path != existing.catalog_path {
        return api_message(
            StatusCode::CONFLICT,
            "app_id and catalog_path are immutable; create a new item instead",
        );
    }
    item.id = Some(id);
    match state.catalog.put_item(&item).await {
        Ok(item) => {
            if item != existing {
                clear_layers_using_catalog_item(&state, id).await;
            }
            Json(item).into_response()
        }
        Err(e) => api_error(StatusCode::UNPROCESSABLE_ENTITY, e),
    }
}

async fn delete_catalog_item(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let id = match CatalogItemId::from_str(&id) {
        Ok(id) => id,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };
    clear_layers_using_catalog_item(&state, id).await;
    match state.catalog.delete_item(id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_message(StatusCode::NOT_FOUND, "catalog item not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

fn api_error(status: StatusCode, error: impl std::fmt::Display) -> Response {
    api_message(status, &error.to_string())
}
fn api_message(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

async fn wms(
    State(state): State<AppState>,
    Query(parameters): Query<HashMap<String, String>>,
) -> Response {
    match WmsRequest::from_query(parameters) {
        Ok(request) if request.operation == Operation::GetCapabilities => {
            let published = match state.catalog.list_published_layers().await {
                Ok(items) => items,
                Err(e) => {
                    return xml_exception(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "NoApplicableCode",
                        &e.to_string(),
                    )
                }
            };
            let mut layers_by_id = HashMap::new();
            for layer in published.into_iter().filter(|layer| layer.enabled) {
                let catalog = match state.catalog.get_item(layer.catalog_item_id).await {
                    Ok(Some(item)) => item,
                    Ok(None) => continue,
                    Err(e) => {
                        return xml_exception(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "NoApplicableCode",
                            &e.to_string(),
                        )
                    }
                };
                layers_by_id.insert(
                    layer.id.clone(),
                    CapabilitiesLayer {
                        id: layer.id,
                        name: layer.name,
                        title: layer.title,
                        abstract_text: layer.description,
                        queryable: layer.queryable,
                        native_crs: catalog.native_crs.0,
                        native_extent: catalog.native_extent,
                        supported_crs: layer.supported_crs.into_iter().map(|crs| crs.0).collect(),
                        min_scale_denominator: layer.min_scale_denominator,
                        max_scale_denominator: layer.max_scale_denominator,
                    },
                );
            }
            let layer_groups = match state.catalog.list_layer_groups().await {
                Ok(groups) => groups,
                Err(e) => {
                    return xml_exception(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "NoApplicableCode",
                        &e.to_string(),
                    )
                }
            };
            let mut grouped_layer_ids = HashSet::new();
            let groups = layer_groups
                .into_iter()
                .filter(|group| group.enabled)
                .filter_map(|group| {
                    let layers = group
                        .members
                        .into_iter()
                        .filter_map(|member| {
                            let layer = layers_by_id.get(&member.published_layer_id)?.clone();
                            grouped_layer_ids.insert(member.published_layer_id);
                            Some(layer)
                        })
                        .collect::<Vec<_>>();
                    (!layers.is_empty()).then_some(CapabilitiesGroup {
                        name: group.name,
                        title: group.title,
                        abstract_text: group.description,
                        layers,
                    })
                })
                .collect::<Vec<_>>();
            let mut layers = layers_by_id
                .into_values()
                .filter(|layer| !grouped_layer_ids.contains(&layer.id))
                .collect::<Vec<_>>();
            layers.sort_by(|a, b| a.name.cmp(&b.name));
            let service_url = std::env::var("WMS_PUBLIC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3000/wms".into());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
                capabilities_xml(&service_url, &layers, &groups),
            )
                .into_response()
        }
        Ok(request) if request.operation == Operation::GetMap => {
            serve_wms_map(state, request.parameters).await
        }
        Ok(request) => xml_exception(
            StatusCode::NOT_IMPLEMENTED,
            "OperationNotSupported",
            &format!("{:?} is not implemented yet", request.operation),
        ),
        Err(error) => xml_exception(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            &error.to_string(),
        ),
    }
}

async fn serve_wms_map(state: AppState, parameters: HashMap<String, String>) -> Response {
    let required = |name: &'static str| {
        parameters
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("missing required parameter: {name}"))
    };
    let layer_name = match required("LAYERS") {
        Ok(value) if !value.contains(',') => value,
        Ok(_) => {
            return xml_exception(
                StatusCode::BAD_REQUEST,
                "LayerNotDefined",
                "Stage 4 currently supports one layer per GetMap request",
            )
        }
        Err(error) => {
            return xml_exception(StatusCode::BAD_REQUEST, "MissingParameterValue", &error)
        }
    };
    if parameters
        .get("STYLES")
        .is_some_and(|styles| !styles.is_empty() && !styles.eq_ignore_ascii_case("default"))
    {
        return xml_exception(
            StatusCode::BAD_REQUEST,
            "StyleNotDefined",
            "only the published default style is supported",
        );
    }
    let parse_u32 = |name: &'static str| -> Result<u32, String> {
        required(name)?
            .parse::<u32>()
            .map_err(|_| format!("{name} must be a positive integer"))
            .and_then(|value| {
                (value > 0 && value <= 8192)
                    .then_some(value)
                    .ok_or_else(|| format!("{name} must be between 1 and 8192"))
            })
    };
    let (width, height) = match (parse_u32("WIDTH"), parse_u32("HEIGHT")) {
        (Ok(width), Ok(height)) => (width, height),
        (Err(error), _) | (_, Err(error)) => {
            return xml_exception(StatusCode::BAD_REQUEST, "InvalidDimensionValue", &error)
        }
    };
    // OpenLayers ImageWMS sends physical image dimensions on HiDPI displays and,
    // for MapServer-compatible requests, exposes the device scale through
    // MAP_RESOLUTION (90 dpi at 1x). Keep scale selection based on CSS pixels,
    // while still rendering the larger physical image with scaled symbolizers.
    let pixel_ratio = wms_pixel_ratio(&parameters);
    let logical_width = ((width as f32) / pixel_ratio).round().max(1.0) as u32;
    let logical_height = ((height as f32) / pixel_ratio).round().max(1.0) as u32;
    let _wms_request_timer = WmsRequestTimer {
        metrics: state.metrics.clone(),
        snapshot: WmsRequestSnapshot {
            timestamp: unix_seconds(),
            layer: layer_name.to_owned(),
            width,
            height,
            logical_width,
            logical_height,
            pixel_ratio,
            total_ms: 0.0,
        },
        started: Instant::now(),
    };
    let crs = match parameters.get("CRS").or_else(|| parameters.get("SRS")) {
        Some(crs) if !crs.trim().is_empty() => crs.clone(),
        _ => {
            return xml_exception(
                StatusCode::BAD_REQUEST,
                "MissingParameterValue",
                "CRS is required",
            )
        }
    };
    let values = match required("BBOX").and_then(|bbox| {
        let values = bbox
            .split(',')
            .map(|value| {
                value
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| "BBOX contains a non-number".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        (values.len() == 4)
            .then_some(values)
            .ok_or_else(|| "BBOX must contain four numbers".to_owned())
    }) {
        Ok(values) => values,
        Err(error) => {
            return xml_exception(StatusCode::BAD_REQUEST, "InvalidParameterValue", &error)
        }
    };
    let bbox = if crs.eq_ignore_ascii_case("EPSG:4326") {
        GdalBoundingBox {
            min_x: values[1],
            min_y: values[0],
            max_x: values[3],
            max_y: values[2],
        }
    } else {
        GdalBoundingBox {
            min_x: values[0],
            min_y: values[1],
            max_x: values[2],
            max_y: values[3],
        }
    };
    let image_format = match parameters.get("FORMAT").map(|value| value.as_str()) {
        Some(value) if value.eq_ignore_ascii_case("image/jpeg") => ImageFormat::Jpeg,
        Some(value) if value.eq_ignore_ascii_case("image/png") => ImageFormat::Png,
        Some(_) => {
            return xml_exception(
                StatusCode::BAD_REQUEST,
                "InvalidFormat",
                "FORMAT must be image/png or image/jpeg",
            )
        }
        None => ImageFormat::Png,
    };
    let transparent = image_format == ImageFormat::Png
        && parameters
            .get("TRANSPARENT")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let layers = match state.catalog.list_published_layers().await {
        Ok(layers) => layers,
        Err(error) => {
            return xml_exception(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                &error.to_string(),
            )
        }
    };
    let groups = match state.catalog.list_layer_groups().await {
        Ok(groups) => groups,
        Err(error) => {
            return xml_exception(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                &error.to_string(),
            )
        }
    };
    if let Some(group) = groups
        .into_iter()
        .find(|group| group.enabled && group.name == layer_name)
    {
        let request_scale = scale_denominator(&crs, &bbox, logical_width, logical_height);
        let rendered = match render_group_members(
            &state,
            &layers,
            &group.members,
            crs.clone(),
            bbox,
            width,
            height,
            request_scale,
            0,
            pixel_ratio,
            true,
        )
        .await
        {
            Ok(rendered) => rendered,
            Err(error) => {
                return xml_exception(StatusCode::UNPROCESSABLE_ENTITY, "NoApplicableCode", &error)
            }
        };
        let background = if transparent {
            [0, 0, 0, 0]
        } else {
            [255, 255, 255, 255]
        };
        return match compose_images(width, height, background, rendered, image_format) {
            Ok(image) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, image.content_type),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                image.bytes,
            )
                .into_response(),
            Err(error) => xml_exception(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                &error,
            ),
        };
    }
    let Some(layer) = layers
        .into_iter()
        .find(|layer| layer.enabled && layer.name == layer_name)
    else {
        return xml_exception(
            StatusCode::NOT_FOUND,
            "LayerNotDefined",
            "published layer was not found",
        );
    };
    if !layer.supported_crs.is_empty()
        && !layer
            .supported_crs
            .iter()
            .any(|supported| supported.0.eq_ignore_ascii_case(&crs))
    {
        return xml_exception(
            StatusCode::BAD_REQUEST,
            "InvalidCRS",
            "CRS is not enabled for this layer",
        );
    }
    let request_scale = scale_denominator(&crs, &bbox, logical_width, logical_height);
    if !scale_is_visible(
        layer.min_scale_denominator,
        layer.max_scale_denominator,
        request_scale,
    ) {
        let background = if transparent {
            map_core::Rgba {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 0,
            }
        } else {
            map_core::Rgba {
                red: 255,
                green: 255,
                blue: 255,
                alpha: 255,
            }
        };
        let format = match image_format {
            ImageFormat::Png => RenderImageFormat::Png,
            ImageFormat::Jpeg => RenderImageFormat::Jpeg,
        };
        return match blank_image(width, height, background, format) {
            Ok(image) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, image.content_type),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                image.bytes,
            )
                .into_response(),
            Err(error) => xml_exception(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                &error.to_string(),
            ),
        };
    }
    let simplify_tolerance = configured_simplify_tolerance(
        layer.simplify_factor,
        vector_resolution(&bbox, logical_width, logical_height),
    );
    let item = match state.catalog.get_item(layer.catalog_item_id).await {
        Ok(Some(item)) => item,
        Ok(None) => {
            return xml_exception(
                StatusCode::NOT_FOUND,
                "LayerNotDefined",
                "catalog item was not found",
            )
        }
        Err(error) => {
            return xml_exception(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                &error.to_string(),
            )
        }
    };
    let Some(path) = item.source_config.get("path").and_then(Value::as_str) else {
        return xml_exception(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NoApplicableCode",
            "source configuration is missing path",
        );
    };
    let source = GdalDataSource {
        path: path.into(),
        layer: item
            .source_config
            .get("layer")
            .and_then(Value::as_str)
            .map(str::to_owned),
        encoding: item
            .source_config
            .get("encoding")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    if item.source_kind == SourceKind::Vector {
        ensure_vector_source_prepared(
            &state,
            path,
            source.layer.as_deref(),
            source.encoding.as_deref(),
        )
        .await;
    }
    let rendering_style = match vector_rendering_style(&state, &layer, transparent).await {
        Ok(style) => scale_rendering_style(style, pixel_ratio),
        Err(error) => {
            return xml_exception(StatusCode::UNPROCESSABLE_ENTITY, "StyleNotDefined", &error)
        }
    };
    let source_kind = item.source_kind;
    let vector_rasterizer = state.vector_rasterizer.clone();
    match tokio::task::spawn_blocking(move || match source_kind {
        SourceKind::Raster => GdalEngine.render(&RasterRenderRequest {
            source,
            target_crs: crs,
            bbox,
            width,
            height,
            resampling: Resampling::Bilinear,
            format: image_format,
            nodata: Some(0.0),
        }),
        SourceKind::Vector => {
            let mut request = render_vector_request(
                source,
                crs,
                bbox,
                width,
                height,
                0,
                simplify_tolerance,
                image_format,
                rendering_style,
            );
            request.scale_denominator = request_scale;
            request.allow_view_reuse = true;
            vector_rasterizer
                .render(&request)
                .map(|image| cangling_gdal_core::RenderedImage {
                    bytes: image.bytes,
                    content_type: image.content_type,
                })
                .map_err(|error| cangling_gdal_core::GdalError::InvalidRequest(error.to_string()))
        }
    })
    .await
    {
        Ok(Ok(image)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, image.content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            image.bytes,
        )
            .into_response(),
        Ok(Err(error)) => xml_exception(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NoApplicableCode",
            &error.to_string(),
        ),
        Err(error) => xml_exception(
            StatusCode::INTERNAL_SERVER_ERROR,
            "NoApplicableCode",
            &error.to_string(),
        ),
    }
}

fn wms_pixel_ratio(parameters: &HashMap<String, String>) -> f32 {
    parameters
        .get("PIXELRATIO")
        .and_then(|value| value.parse::<f32>().ok())
        .or_else(|| {
            parameters
                .get("MAP_RESOLUTION")
                .and_then(|value| value.parse::<f32>().ok())
                .map(|dpi| dpi / 90.0)
        })
        .filter(|ratio| ratio.is_finite())
        .unwrap_or(1.0)
        .clamp(1.0, 4.0)
}

async fn render_group_member(
    state: &AppState,
    layer: &PublishedLayer,
    member: &LayerGroupMember,
    target_crs: String,
    bbox: GdalBoundingBox,
    width: u32,
    height: u32,
    request_scale: f64,
    buffer_pixels: u32,
    symbol_scale: f32,
    allow_view_reuse: bool,
) -> Result<Option<RasterizedRgba>, String> {
    let min_scale = member.min_scale_denominator.or(layer.min_scale_denominator);
    let max_scale = member.max_scale_denominator.or(layer.max_scale_denominator);
    if !scale_is_visible(min_scale, max_scale, request_scale) {
        return Ok(None);
    }
    let style_id = member
        .style_id
        .as_deref()
        .or(layer.default_style_id.as_deref());
    let rendering_style = scale_rendering_style(
        vector_rendering_style_with_id(state, style_id, true).await?,
        symbol_scale,
    );
    if !rendering_style.rules.iter().any(|rule| {
        scale_is_visible(
            rule.minimum_scale_denominator,
            rule.maximum_scale_denominator,
            request_scale,
        )
    }) {
        return Ok(None);
    }
    let item = state
        .catalog
        .get_item(layer.catalog_item_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("成员图层 `{}` 的目录项不存在", layer.name))?;
    let path = item
        .source_config
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("成员图层 `{}` 缺少数据源路径", layer.name))?;
    let source = GdalDataSource {
        path: path.into(),
        layer: item
            .source_config
            .get("layer")
            .and_then(Value::as_str)
            .map(str::to_owned),
        encoding: item
            .source_config
            .get("encoding")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    if item.source_kind == SourceKind::Vector {
        ensure_vector_source_prepared(
            state,
            path,
            source.layer.as_deref(),
            source.encoding.as_deref(),
        )
        .await;
    }
    let simplify = configured_simplify_tolerance(
        member.simplify_factor.or(layer.simplify_factor),
        vector_resolution(
            &bbox,
            ((width as f32) / symbol_scale).round().max(1.0) as u32,
            ((height as f32) / symbol_scale).round().max(1.0) as u32,
        ),
    );
    let source_kind = item.source_kind;
    let vector_rasterizer = state.vector_rasterizer.clone();
    tokio::task::spawn_blocking(move || match source_kind {
        SourceKind::Raster => GdalEngine
            .render(&RasterRenderRequest {
                source,
                target_crs,
                bbox,
                width,
                height,
                resampling: Resampling::Bilinear,
                format: ImageFormat::Png,
                nodata: Some(0.0),
            })
            .and_then(|image| {
                let rgba = image::load_from_memory(&image.bytes)
                    .map_err(|error| {
                        cangling_gdal_core::GdalError::InvalidRequest(error.to_string())
                    })?
                    .to_rgba8();
                let (width, height) = rgba.dimensions();
                Ok(Some(RasterizedRgba {
                    width,
                    height,
                    pixels: rgba.into_raw(),
                }))
            })
            .map_err(|error| error.to_string()),
        SourceKind::Vector => {
            let mut request = render_vector_request(
                source,
                target_crs,
                bbox,
                width,
                height,
                buffer_pixels,
                simplify,
                ImageFormat::Png,
                rendering_style,
            );
            request.scale_denominator = request_scale;
            request.allow_view_reuse = allow_view_reuse;
            vector_rasterizer
                .render_rgba(&request)
                .map(Some)
                .map_err(|error| error.to_string())
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

#[allow(clippy::too_many_arguments)]
async fn render_group_members(
    state: &AppState,
    layers: &[PublishedLayer],
    members: &[LayerGroupMember],
    target_crs: String,
    bbox: GdalBoundingBox,
    width: u32,
    height: u32,
    request_scale: f64,
    buffer_pixels: u32,
    symbol_scale: f32,
    allow_view_reuse: bool,
) -> Result<Vec<(RasterizedRgba, f64)>, String> {
    let mut jobs = tokio::task::JoinSet::new();
    for (order, member) in members.iter().enumerate() {
        let Some(layer) = layers
            .iter()
            .find(|layer| layer.enabled && layer.id == member.published_layer_id)
        else {
            continue;
        };
        if !layer.supported_crs.is_empty()
            && !layer
                .supported_crs
                .iter()
                .any(|supported| supported.0.eq_ignore_ascii_case(&target_crs))
        {
            continue;
        }
        let state = state.clone();
        let layer = layer.clone();
        let member = member.clone();
        let target_crs = target_crs.clone();
        jobs.spawn(async move {
            let opacity = member.opacity;
            let rendered = render_group_member(
                &state,
                &layer,
                &member,
                target_crs,
                bbox,
                width,
                height,
                request_scale,
                buffer_pixels,
                symbol_scale,
                allow_view_reuse,
            )
            .await;
            (order, opacity, rendered)
        });
    }
    let mut rendered = Vec::new();
    while let Some(result) = jobs.join_next().await {
        let (order, opacity, image) = result.map_err(|error| error.to_string())?;
        if let Some(image) = image? {
            rendered.push((order, image, opacity));
        }
    }
    rendered.sort_by_key(|(order, _, _)| *order);
    Ok(rendered
        .into_iter()
        .map(|(_, image, opacity)| (image, opacity))
        .collect())
}
fn xml_exception(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        service_exception(code, message),
    )
        .into_response()
}

async fn wmts(
    State(state): State<AppState>,
    Query(parameters): Query<HashMap<String, String>>,
) -> Response {
    let request = match WmtsRequest::from_query(parameters) {
        Ok(request) => request,
        Err(error) => {
            return wmts_error(
                StatusCode::BAD_REQUEST,
                "InvalidParameterValue",
                "REQUEST",
                &error.to_string(),
            )
        }
    };
    match request.operation {
        WmtsOperation::GetCapabilities => {
            let published = match state.catalog.list_published_layers().await {
                Ok(layers) => layers,
                Err(error) => {
                    return wmts_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "NoApplicableCode",
                        "",
                        &error.to_string(),
                    )
                }
            };
            let mut layers = published
                .into_iter()
                .filter(|layer| layer.enabled)
                .map(|layer| WmtsCapabilitiesLayer {
                    identifier: layer.name,
                    title: layer.title,
                })
                .collect::<Vec<_>>();
            match state.catalog.list_layer_groups().await {
                Ok(groups) => layers.extend(groups.into_iter().filter(|group| group.enabled).map(
                    |group| WmtsCapabilitiesLayer {
                        identifier: group.name,
                        title: group.title,
                    },
                )),
                Err(error) => {
                    return wmts_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "NoApplicableCode",
                        "",
                        &error.to_string(),
                    )
                }
            }
            let service_url = std::env::var("WMTS_PUBLIC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3000/wmts".into());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
                wmts_capabilities_xml(&service_url, &layers),
            )
                .into_response()
        }
        WmtsOperation::GetTile => {
            let tile = match request.tile() {
                Ok(tile) => tile,
                Err(error) => {
                    return wmts_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidParameterValue",
                        "GetTile",
                        &error.to_string(),
                    )
                }
            };
            serve_wmts_tile(state, tile).await
        }
    }
}

async fn serve_wmts_tile(state: AppState, tile: TileRequest) -> Response {
    let request_started = Instant::now();
    let output_size = 256 * u32::from(tile.pixel_ratio);
    state.metrics.wmts_requests.fetch_add(1, Ordering::Relaxed);
    if tile.style != "default" {
        return wmts_error(
            StatusCode::BAD_REQUEST,
            "StyleNotDefined",
            "STYLE",
            "only the default style is currently supported",
        );
    }
    let layers = match state.catalog.list_published_layers().await {
        Ok(layers) => layers,
        Err(error) => {
            return wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "LAYER",
                &error.to_string(),
            )
        }
    };
    let groups = match state.catalog.list_layer_groups().await {
        Ok(groups) => groups,
        Err(error) => {
            return wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "LAYER",
                &error.to_string(),
            )
        }
    };
    if let Some(group) = groups
        .into_iter()
        .find(|group| group.enabled && group.name == tile.layer)
    {
        return serve_wmts_group_tile(state, tile, group, layers).await;
    }
    let Some(layer) = layers
        .into_iter()
        .find(|layer| layer.enabled && layer.name == tile.layer)
    else {
        return wmts_error(
            StatusCode::NOT_FOUND,
            "InvalidParameterValue",
            "LAYER",
            "published layer was not found",
        );
    };
    let item = match state.catalog.get_item(layer.catalog_item_id).await {
        Ok(Some(item)) => item,
        Ok(None) => {
            return wmts_error(
                StatusCode::NOT_FOUND,
                "NoApplicableCode",
                "LAYER",
                "catalog item was not found",
            )
        }
        Err(error) => {
            return wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "LAYER",
                &error.to_string(),
            )
        }
    };
    let Some(source_path) = item.source_config.get("path").and_then(Value::as_str) else {
        return wmts_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "NoApplicableCode",
            "LAYER",
            "source configuration is missing path",
        );
    };
    let source_layer = item
        .source_config
        .get("layer")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source_encoding = item
        .source_config
        .get("encoding")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source_kind = item.source_kind;
    let rendering_style = match vector_rendering_style(&state, &layer, true).await {
        Ok(style) => style,
        Err(error) => {
            return wmts_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "NoApplicableCode",
                "STYLE",
                &error,
            )
        }
    };
    let bounds = match tile_bounds(tile.zoom, tile.row, tile.col) {
        Ok(bounds) => bounds,
        Err(error) => {
            return wmts_error(
                StatusCode::BAD_REQUEST,
                "TileOutOfRange",
                "TILEMATRIX",
                &error.to_string(),
            )
        }
    };
    let bbox = GdalBoundingBox {
        min_x: bounds.min_x,
        min_y: bounds.min_y,
        max_x: bounds.max_x,
        max_y: bounds.max_y,
    };
    let request_scale = scale_denominator("EPSG:3857", &bbox, 256, 256);
    if !scale_is_visible(
        layer.min_scale_denominator,
        layer.max_scale_denominator,
        request_scale,
    ) {
        return match blank_png(
            output_size,
            output_size,
            map_core::Rgba {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 0,
            },
        ) {
            Ok(bytes) => png_tile_response(bytes),
            Err(error) => wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "GetTile",
                &error.to_string(),
            ),
        };
    }
    let simplify_tolerance =
        configured_simplify_tolerance(layer.simplify_factor, vector_resolution(&bbox, 256, 256));
    let wmts_cache = active_wmts_cache(&state);
    let cache_key = WmtsCacheKey {
        database: match &wmts_cache {
            WmtsCache::CanglingTile {
                database_id: Some(id),
                ..
            } => id.clone(),
            _ => sha256_hex(layer.app_id.as_bytes()),
        },
        item: pixel_ratio_cache_item(&layer_cache_item_id(&layer), tile.pixel_ratio),
        zoom: tile.zoom,
        col: tile.col,
        row: tile.row,
    };
    let cache_started = Instant::now();
    match cache_get(&wmts_cache, &cache_key).await {
        Ok(Some(bytes)) => {
            let cache_elapsed = cache_started.elapsed();
            state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            state.metrics.record_lookup(&layer, true, cache_elapsed);
            state
                .metrics
                .record_completed(true, request_started.elapsed());
            tracing::debug!(
                layer = %layer.name,
                zoom = tile.zoom,
                col = tile.col,
                row = tile.row,
                cache_hit = true,
                cache_ms = duration_ms(cache_elapsed),
                total_ms = request_started.elapsed().as_secs_f64() * 1000.0,
                "WMTS tile served"
            );
            return png_tile_response(bytes);
        }
        Ok(None) => {
            let cache_elapsed = cache_started.elapsed();
            state.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
            state.metrics.record_lookup(&layer, false, cache_elapsed);
            tracing::debug!(
                layer = %layer.name,
                zoom = tile.zoom,
                col = tile.col,
                row = tile.row,
                cache_hit = false,
                cache_ms = duration_ms(cache_elapsed),
                "WMTS cache miss"
            );
        }
        Err(error) => tracing::warn!(%error, "WMTS cache read failed; rendering tile"),
    }
    let flight = tile_flight(&state, &cache_key);
    let flight_started = Instant::now();
    let _flight_guard = flight.lock().await;
    let flight_wait_ms = flight_started.elapsed().as_secs_f64() * 1000.0;
    // Only one request for a cache key is allowed to render. Followers wait on
    // the keyed lock and consume the image written by the leader.
    if let Ok(Some(bytes)) = cache_get(&wmts_cache, &cache_key).await {
        state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        state
            .metrics
            .coalesced_requests
            .fetch_add(1, Ordering::Relaxed);
        state.metrics.record_coalesced(&layer.name);
        state
            .metrics
            .record_completed(true, request_started.elapsed());
        tracing::debug!(
            layer = %layer.name,
            zoom = tile.zoom,
            col = tile.col,
            row = tile.row,
            cache_hit = true,
            coalesced = true,
            flight_wait_ms,
            total_ms = request_started.elapsed().as_secs_f64() * 1000.0,
            "WMTS duplicate request served from cache"
        );
        return png_tile_response(bytes);
    }

    let permit_started = Instant::now();
    let _render_permit = match state.render_limit.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => {
            return wmts_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "NoApplicableCode",
                "GetTile",
                &format!("render queue is unavailable: {error}"),
            )
        }
    };
    let queue_ms = permit_started.elapsed().as_secs_f64() * 1000.0;
    // A request for the same tile may have completed while this one waited in
    // the render queue. Recheck the shared cache to avoid duplicate GDAL work.
    if let Ok(Some(bytes)) = cache_get(&wmts_cache, &cache_key).await {
        state.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        state
            .metrics
            .coalesced_requests
            .fetch_add(1, Ordering::Relaxed);
        state.metrics.record_coalesced(&layer.name);
        state
            .metrics
            .record_completed(true, request_started.elapsed());
        tracing::debug!(
            layer = %layer.name,
            zoom = tile.zoom,
            col = tile.col,
            row = tile.row,
                cache_hit = true,
                coalesced = true,
                flight_wait_ms,
                queue_ms,
            "WMTS queued request served from cache"
        );
        return png_tile_response(bytes);
    }
    if source_kind == SourceKind::Vector {
        ensure_vector_source_prepared(
            &state,
            source_path,
            source_layer.as_deref(),
            source_encoding.as_deref(),
        )
        .await;
    }
    let source_path = source_path.to_owned();
    let vector_rasterizer = state.vector_rasterizer.clone();
    let preparation_elapsed = request_started.elapsed();
    let render_started = Instant::now();
    match tokio::task::spawn_blocking(move || {
        render_wmts_tile(
            &source_path,
            source_layer.as_deref(),
            source_encoding.as_deref(),
            source_kind,
            bounds,
            simplify_tolerance,
            rendering_style,
            vector_rasterizer.as_ref(),
            tile.pixel_ratio,
        )
    })
    .await
    {
        Ok(Ok(bytes)) => {
            let render_elapsed = render_started.elapsed();
            let render_micros = render_elapsed.as_micros() as u64;
            state.metrics.rendered_tiles.fetch_add(1, Ordering::Relaxed);
            state
                .metrics
                .render_time_micros
                .fetch_add(render_micros, Ordering::Relaxed);
            let rendered_bytes = bytes.len();
            let cache_write_started = Instant::now();
            if let Err(error) =
                cache_put(&wmts_cache, &cache_key, &layer.app_id, &layer.title, &bytes).await
            {
                tracing::warn!(%error, "WMTS cache write failed; returning rendered tile");
            }
            let cache_write_elapsed = cache_write_started.elapsed();
            let cache_write_ms = duration_ms(cache_write_elapsed);
            state
                .metrics
                .cache_write_time_micros
                .fetch_add(cache_write_elapsed.as_micros() as u64, Ordering::Relaxed);
            let total_elapsed = request_started.elapsed();
            state.metrics.record_rendered(
                &layer,
                &tile,
                rendered_bytes,
                preparation_elapsed,
                render_elapsed,
                cache_write_elapsed,
                total_elapsed,
            );
            state.metrics.record_completed(false, total_elapsed);
            tracing::info!(
                layer = %layer.name,
                zoom = tile.zoom,
                col = tile.col,
                row = tile.row,
                cache_hit = false,
                bytes = rendered_bytes,
                flight_wait_ms,
                queue_ms,
                cache_write_ms,
                total_ms = request_started.elapsed().as_secs_f64() * 1000.0,
                "WMTS tile rendered"
            );
            png_tile_response(bytes)
        }
        Ok(Err(error)) => {
            state
                .metrics
                .render_failures
                .fetch_add(1, Ordering::Relaxed);
            state.metrics.record_failure(&layer.name);
            wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "GetTile",
                &error,
            )
        }
        Err(error) => {
            state
                .metrics
                .render_failures
                .fetch_add(1, Ordering::Relaxed);
            state.metrics.record_failure(&layer.name);
            wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "GetTile",
                &error.to_string(),
            )
        }
    }
}

async fn serve_wmts_group_tile(
    state: AppState,
    tile: TileRequest,
    group: LayerGroup,
    layers: Vec<PublishedLayer>,
) -> Response {
    let output_size = 256 * u32::from(tile.pixel_ratio);
    let wmts_cache = active_wmts_cache(&state);
    let styles = match state.catalog.list_styles().await {
        Ok(styles) => styles,
        Err(error) => {
            return wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "LAYER",
                &error.to_string(),
            )
        }
    };
    let bounds = match tile_bounds(tile.zoom, tile.row, tile.col) {
        Ok(bounds) => bounds,
        Err(error) => {
            return wmts_error(
                StatusCode::BAD_REQUEST,
                "TileOutOfRange",
                "TILEMATRIX",
                &error.to_string(),
            )
        }
    };
    let bbox = GdalBoundingBox {
        min_x: bounds.min_x,
        min_y: bounds.min_y,
        max_x: bounds.max_x,
        max_y: bounds.max_y,
    };
    let cache_key = WmtsCacheKey {
        database: match &wmts_cache {
            WmtsCache::CanglingTile {
                database_id: Some(id),
                ..
            } => id.clone(),
            _ => sha256_hex(group.app_id.as_bytes()),
        },
        item: pixel_ratio_cache_item(
            &group_cache_item_id(&group, &layers, &styles),
            tile.pixel_ratio,
        ),
        zoom: tile.zoom,
        col: tile.col,
        row: tile.row,
    };
    if let Ok(Some(bytes)) = cache_get(&wmts_cache, &cache_key).await {
        return png_tile_response(bytes);
    }
    let flight = tile_flight(&state, &cache_key);
    let _flight_guard = flight.lock().await;
    if let Ok(Some(bytes)) = cache_get(&wmts_cache, &cache_key).await {
        return png_tile_response(bytes);
    }
    let _permit = match state.render_limit.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => {
            return wmts_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "NoApplicableCode",
                "GetTile",
                &format!("render queue is unavailable: {error}"),
            )
        }
    };
    let request_scale = scale_denominator("EPSG:3857", &bbox, 256, 256);
    let rendered = match render_group_members(
        &state,
        &layers,
        &group.members,
        "EPSG:3857".into(),
        bbox,
        output_size,
        output_size,
        request_scale,
        2 * u32::from(tile.pixel_ratio),
        f32::from(tile.pixel_ratio),
        false,
    )
    .await
    {
        Ok(rendered) => rendered,
        Err(error) => {
            return wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "GetTile",
                &error,
            )
        }
    };
    let bytes = match compose_images(
        output_size,
        output_size,
        [0, 0, 0, 0],
        rendered,
        ImageFormat::Png,
    ) {
        Ok(image) => image.bytes,
        Err(error) => {
            return wmts_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "NoApplicableCode",
                "GetTile",
                &error,
            )
        }
    };
    if let Err(error) =
        cache_put(&wmts_cache, &cache_key, &group.app_id, &group.title, &bytes).await
    {
        tracing::warn!(%error, group = %group.name, "WMTS group cache write failed");
    }
    png_tile_response(bytes)
}

fn group_cache_item_id(
    group: &LayerGroup,
    layers: &[PublishedLayer],
    styles: &[StyleRecord],
) -> String {
    let mut identity = format!("{}:{}", group.id, group.enabled);
    for member in &group.members {
        let layer_identity = layers
            .iter()
            .find(|layer| layer.id == member.published_layer_id)
            .map(layer_cache_item_id)
            .unwrap_or_default();
        identity.push_str(&format!(
            "|{}:{}:{:?}:{:?}:{:?}:{}",
            member.published_layer_id,
            member.style_id.as_deref().unwrap_or(""),
            member.min_scale_denominator,
            member.max_scale_denominator,
            member.simplify_factor,
            member.opacity
        ));
        identity.push_str(&layer_identity);
        let style_id = member.style_id.as_deref().or_else(|| {
            layers
                .iter()
                .find(|layer| layer.id == member.published_layer_id)
                .and_then(|layer| layer.default_style_id.as_deref())
        });
        if let Some(style) = style_id.and_then(|id| styles.iter().find(|style| style.id == id)) {
            identity.push_str(&format!("@{}:{}", style.revision, style.content_hash));
        }
    }
    sha256_hex(identity.as_bytes())
}

async fn ensure_vector_source_prepared(
    state: &AppState,
    source_path: &str,
    source_layer: Option<&str>,
    source_encoding: Option<&str>,
) {
    let key = format!(
        "{}\0{}\0{}",
        source_path,
        source_layer.unwrap_or(""),
        source_encoding.unwrap_or("")
    );
    {
        let mut prepared = state.prepared_vector_sources.lock().await;
        if !prepared.insert(key) {
            return;
        }
    }
    let source = GdalDataSource {
        path: source_path.into(),
        layer: source_layer.map(str::to_owned),
        encoding: source_encoding.map(str::to_owned),
    };
    let path = source.path.display().to_string();
    match tokio::task::spawn_blocking(move || GdalEngine.ensure_vector_spatial_index(&source)).await
    {
        Ok(Ok(status)) => {
            tracing::info!(source = %path, ?status, "vector spatial index prepared");
        }
        Ok(Err(error)) => {
            tracing::warn!(source = %path, %error, "failed to prepare vector spatial index; continuing without it");
        }
        Err(error) => {
            tracing::warn!(source = %path, %error, "spatial index task failed; continuing without it");
        }
    }
    // Do not retry on every tile when the source is read-only or the driver
    // does not support indexes. A process restart will attempt preparation again.
}

fn render_wmts_tile(
    source_path: &str,
    source_layer: Option<&str>,
    source_encoding: Option<&str>,
    source_kind: SourceKind,
    bounds: ogc_wmts::TileBounds,
    simplify_tolerance: Option<f64>,
    rendering_style: VectorRenderingStyle,
    vector_rasterizer: &dyn VectorRasterizer,
    pixel_ratio: u8,
) -> Result<Vec<u8>, String> {
    let output_size = 256 * u32::from(pixel_ratio);
    let rendering_style = scale_rendering_style(rendering_style, f32::from(pixel_ratio));
    let source = GdalDataSource {
        path: source_path.into(),
        layer: source_layer.map(str::to_owned),
        encoding: source_encoding.map(str::to_owned),
    };
    let bbox = GdalBoundingBox {
        min_x: bounds.min_x,
        min_y: bounds.min_y,
        max_x: bounds.max_x,
        max_y: bounds.max_y,
    };
    let result = match source_kind {
        SourceKind::Raster => GdalEngine.render(&RasterRenderRequest {
            source,
            target_crs: "EPSG:3857".into(),
            bbox,
            width: output_size,
            height: output_size,
            resampling: Resampling::Bilinear,
            format: ImageFormat::Png,
            nodata: Some(0.0),
        }),
        SourceKind::Vector => vector_rasterizer
            .render(&render_vector_request(
                source,
                "EPSG:3857".into(),
                bbox,
                output_size,
                output_size,
                2 * u32::from(pixel_ratio),
                simplify_tolerance,
                ImageFormat::Png,
                rendering_style,
            ))
            .map(|image| cangling_gdal_core::RenderedImage {
                bytes: image.bytes,
                content_type: image.content_type,
            })
            .map_err(|error| cangling_gdal_core::GdalError::InvalidRequest(error.to_string())),
    };
    result
        .map(|image| image.bytes)
        .map_err(|error| error.to_string())
}

fn wmts_error(status: StatusCode, code: &str, locator: &str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        wmts_exception_xml(code, locator, message),
    )
        .into_response()
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WmtsCacheKey {
    database: String,
    item: String,
    zoom: u8,
    col: u32,
    row: u32,
}

fn tile_flight(state: &AppState, key: &WmtsCacheKey) -> Arc<Mutex<()>> {
    let mut flights = state
        .tile_flights
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Weak values avoid retaining a lock for every tile ever requested. Clean
    // expired keys whenever a new miss enters the flight table.
    flights.retain(|_, flight| flight.strong_count() > 0);
    if let Some(flight) = flights.get(key).and_then(Weak::upgrade) {
        return flight;
    }
    let flight = Arc::new(Mutex::new(()));
    flights.insert(key.clone(), Arc::downgrade(&flight));
    flight
}

fn png_tile_response(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            // WMTS storage remains cached server-side. The browser must revalidate so
            // clearing a layer cache or changing source attributes cannot leave a
            // viewport composed from different tile generations.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
    )
        .into_response()
}

async fn cache_get(cache: &WmtsCache, key: &WmtsCacheKey) -> Result<Option<Vec<u8>>, String> {
    match cache {
        WmtsCache::None => Ok(None),
        WmtsCache::Filesystem { root } => match std::fs::read(filesystem_cache_path(root, key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        },
        WmtsCache::CanglingTile {
            base_url, client, ..
        } => {
            let request = hyper::Request::get(cangling_tile_url(base_url, key))
                .body(Full::new(Bytes::new()))
                .map_err(|error| error.to_string())?;
            let response = client
                .request(request)
                .await
                .map_err(|error| error.to_string())?;
            if response.status() == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !response.status().is_success() {
                return Err(format!("cangling-tile GET returned {}", response.status()));
            }
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|error| error.to_string())?
                .to_bytes()
                .to_vec();
            Ok((!bytes.is_empty()).then_some(bytes))
        }
    }
}

async fn cache_put(
    cache: &WmtsCache,
    key: &WmtsCacheKey,
    database_name: &str,
    layer_name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    match cache {
        WmtsCache::None => Ok(()),
        WmtsCache::Filesystem { root } => {
            let path = filesystem_cache_path(root, key);
            std::fs::create_dir_all(path.parent().ok_or("invalid cache path")?)
                .map_err(|error| error.to_string())?;
            std::fs::write(path, bytes).map_err(|error| error.to_string())
        }
        WmtsCache::CanglingTile {
            base_url,
            token,
            client,
            ..
        } => {
            let mut builder = hyper::Request::put(cangling_tile_url(base_url, key))
                .header(header::CONTENT_TYPE, "image/png")
                .header("x-tile-database-name", percent_encode(database_name))
                .header("x-tile-layer-name", percent_encode(layer_name));
            if !token.is_empty() {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let request = builder
                .body(Full::new(Bytes::copy_from_slice(bytes)))
                .map_err(|error| error.to_string())?;
            let response = client
                .request(request)
                .await
                .map_err(|error| error.to_string())?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("cangling-tile PUT returned {}", response.status()))
            }
        }
    }
}

fn filesystem_cache_path(root: &std::path::Path, key: &WmtsCacheKey) -> std::path::PathBuf {
    root.join(&key.database)
        .join(&key.item)
        .join(key.zoom.to_string())
        .join(key.col.to_string())
        .join(format!("{}.png", key.row))
}

fn cangling_tile_url(base_url: &str, key: &WmtsCacheKey) -> String {
    format!(
        "{base_url}/tiles/{}/{}/{}/{}/{}.png",
        key.database, key.item, key.zoom, key.col, key.row
    )
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_all_compiled_rules_to_ordered_render_rules() {
        let style = compile_geocss(
            "hazard",
            "* { fill: #336699; fill-opacity: 0.5; } [level >= 3] { stroke: #ffffff; }",
        )
        .unwrap();
        let rendering_style = rendering_style_from_style(&style, true).unwrap();
        assert_eq!(rendering_style.rules.len(), 2);
        assert!(matches!(
            rendering_style.rules[0].paint.fill,
            Some(VectorColor::Literal(map_core::Rgba { alpha: 128, .. }))
        ));
        assert!(rendering_style.rules[1].filter.is_some());
        assert!(matches!(
            rendering_style.rules[1].paint.stroke,
            Some(VectorColor::Literal(map_core::Rgba { red: 255, .. }))
        ));
        assert_eq!(rendering_style.background, [0, 0, 0, 0]);
    }

    #[test]
    fn uses_stroke_color_when_rule_has_no_fill() {
        let style =
            compile_geocss("outline", "* { stroke: #ff8000; stroke-opacity: 0.25; }").unwrap();
        let rendering_style = rendering_style_from_style(&style, false).unwrap();
        assert!(rendering_style.rules[0].paint.fill.is_none());
        assert_eq!(
            rendering_style.rules[0].paint.stroke,
            Some(VectorColor::Literal(map_core::Rgba {
                red: 255,
                green: 128,
                blue: 0,
                alpha: 64
            }))
        );
        assert_eq!(rendering_style.rules[0].paint.stroke_width, 1.0);
        assert_eq!(rendering_style.background, [255, 255, 255, 255]);
    }

    #[test]
    fn uses_gray_fill_when_fill_image_is_missing() {
        let style = compile_geocss(
            "missing-pattern",
            "* { fill-image: url('definitely-missing-pattern.png'); fill-opacity: 0.5; }",
        )
        .unwrap();
        let rendering_style = rendering_style_from_style(&style, true).unwrap();
        assert!(rendering_style.rules[0].paint.fill_image.is_none());
        assert_eq!(
            rendering_style.rules[0].paint.fill,
            Some(VectorColor::Literal(map_core::Rgba {
                red: 176,
                green: 176,
                blue: 176,
                alpha: 128,
            }))
        );
    }

    #[test]
    fn calculates_ogc_scale_denominator_for_projected_and_geographic_crs() {
        let bbox = GdalBoundingBox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 256.0,
            max_y: 256.0,
        };
        assert!((scale_denominator("EPSG:3857", &bbox, 256, 256) - 3571.428).abs() < 0.01);
        assert!(scale_denominator("EPSG:4326", &bbox, 256, 256) > 300_000_000.0);
    }

    #[test]
    fn applies_configured_scale_visibility_and_simplification() {
        assert!(scale_is_visible(None, None, 50_000.0));
        assert!(scale_is_visible(Some(10_000.0), Some(100_000.0), 50_000.0));
        assert!(!scale_is_visible(Some(10_000.0), Some(100_000.0), 5_000.0));
        assert!(!scale_is_visible(
            Some(10_000.0),
            Some(100_000.0),
            100_000.0
        ));
        assert_eq!(configured_simplify_tolerance(None, 10.0), None);
        assert_eq!(configured_simplify_tolerance(Some(0.0), 10.0), None);
        assert_eq!(configured_simplify_tolerance(Some(0.5), 10.0), Some(5.0));
    }

    #[test]
    fn clears_only_the_selected_filesystem_layer_cache() {
        let root = std::env::temp_dir().join(format!(
            "cangling-map-cache-clear-test-{}",
            std::process::id()
        ));
        let target = root.join("database/item");
        let tile = target.join("1/2/3.png");
        let other = root.join("database/other/1/2/3.png");
        std::fs::create_dir_all(tile.parent().unwrap()).unwrap();
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&tile, b"tile").unwrap();
        std::fs::write(&other, b"other").unwrap();

        assert_eq!(clear_filesystem_layer_cache(&target).unwrap(), 1);
        assert!(!tile.exists());
        assert!(other.exists());

        std::fs::remove_dir_all(root).unwrap();
    }
}

fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}
