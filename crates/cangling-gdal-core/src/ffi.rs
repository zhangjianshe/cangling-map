#![allow(unsafe_code)]

use std::{
    collections::VecDeque,
    ffi::{c_char, CStr, CString},
    path::{Path, PathBuf},
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{Instant, UNIX_EPOCH},
};

use gdal::{vector::LayerAccess, Dataset};
use sha2::{Digest, Sha256};

use crate::{
    open_vector_source, GdalError, RasterRenderRequest, RenderedImage, VectorRenderRequest,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceCacheKey {
    path: PathBuf,
    layer: Option<String>,
    encoding: Option<String>,
    files: Vec<(String, u64, u128)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectionCacheKey {
    source: SourceCacheKey,
    target_crs: String,
    simplify_tolerance: Option<u64>,
    bounds: Option<[u64; 4]>,
}

struct SourceCacheEntry {
    key: SourceCacheKey,
    dataset: Dataset,
}

struct ProjectionCacheEntry {
    key: ProjectionCacheKey,
    dataset: Option<Dataset>,
    storage_path: Option<CString>,
    estimated_bytes: usize,
}

impl ProjectionCacheEntry {
    fn dataset(&self) -> &Dataset {
        self.dataset
            .as_ref()
            .expect("projected dataset is available")
    }
}

impl Drop for ProjectionCacheEntry {
    fn drop(&mut self) {
        drop(self.dataset.take());
        if let Some(path) = &self.storage_path {
            unsafe {
                gdal_sys::VSIUnlink(path.as_ptr());
            }
        }
    }
}

static VECTOR_SOURCE_POOL: OnceLock<Mutex<VecDeque<SourceCacheEntry>>> = OnceLock::new();
static VECTOR_PROJECTION_CACHE: OnceLock<Mutex<VecDeque<ProjectionCacheEntry>>> = OnceLock::new();
static SOURCE_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static SOURCE_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static PROJECTION_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static PROJECTION_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static PROJECTION_CACHE_EVICTIONS: AtomicU64 = AtomicU64::new(0);
static PERSISTENT_PROJECTION_HITS: AtomicU64 = AtomicU64::new(0);
static PERSISTENT_PROJECTION_MISSES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn render(request: &RasterRenderRequest) -> Result<RenderedImage, GdalError> {
    let source = Dataset::open(&request.source.path)?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let stem = format!("/vsimem/cangling-gdal-{}-{id}", std::process::id());
    let warped_path = CString::new(format!("{stem}.tif")).map_err(invalid_string)?;
    let image_path = CString::new(format!("{stem}.image")).map_err(invalid_string)?;
    let cleanup = VsiCleanup::pair(&warped_path, &image_path);

    let mut warp_args = vec![
        "-of".to_owned(),
        "GTiff".to_owned(),
        "-t_srs".to_owned(),
        request.target_crs.clone(),
        "-te".to_owned(),
        request.bbox.min_x.to_string(),
        request.bbox.min_y.to_string(),
        request.bbox.max_x.to_string(),
        request.bbox.max_y.to_string(),
        "-ts".to_owned(),
        request.width.to_string(),
        request.height.to_string(),
        "-r".to_owned(),
        request.resampling.gdal_name().to_owned(),
    ];
    if let Some(nodata) = request.nodata {
        warp_args.extend(["-dstnodata".to_owned(), nodata.to_string()]);
    }

    let warped = unsafe { warp(&source, &warped_path, &warp_args)? };
    let translated = unsafe {
        translate(
            &warped,
            &image_path,
            &["-of".to_owned(), request.format.driver().to_owned()],
        )?
    };
    drop(translated);
    drop(warped);

    let bytes = unsafe { read_vsimem(&image_path)? };
    drop(cleanup);
    Ok(RenderedImage {
        bytes,
        content_type: request.format.content_type(),
    })
}

pub(crate) fn render_vector(request: &VectorRenderRequest) -> Result<RenderedImage, GdalError> {
    with_projected_vector(request, crate::tiny_skia_renderer::render_projected)
}

pub(crate) fn with_projected_vector<T>(
    request: &VectorRenderRequest,
    render: impl Fn(&Dataset, &VectorRenderRequest) -> Result<T, GdalError>,
) -> Result<T, GdalError> {
    let source_key = source_cache_key(request)?;
    // Persistent entries are complete target-CRS datasets. Keeping request
    // bounds and pixel-derived simplification out of their identity gives one
    // reusable, spatially indexed file per source/CRS. Geometry simplification
    // happens after the indexed target-CRS query, where its tolerance has the
    // correct unit and cannot corrupt geographic source coordinates.
    let persistent = persistent_projection_enabled();
    let projection_bounds = (!persistent)
        .then(|| projection_cache_bounds(request))
        .flatten();
    let simplify_tolerance = (!persistent)
        .then_some(request.simplify_tolerance)
        .flatten();
    let projection_key = ProjectionCacheKey {
        source: source_key.clone(),
        target_crs: request.target_crs.clone(),
        simplify_tolerance: simplify_tolerance.map(f64::to_bits),
        bounds: projection_bounds.map(|bounds| bounds.map(f64::to_bits)),
    };
    if let Some(entry) = take_projection(&projection_key) {
        let rendered = render(entry.dataset(), request);
        put_projection(entry);
        tracing::debug!("GDAL projected dataset cache hit");
        return rendered;
    }

    let persistent_path = persistent_projection_path(&projection_key)?;
    if let Some(path) = persistent_path.as_deref().filter(|path| path.is_file()) {
        match Dataset::open(path) {
            Ok(projected) => {
                PERSISTENT_PROJECTION_HITS.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(path = %path.display(), "GDAL persistent projection cache hit");
                let rendered = render(&projected, request);
                let estimated_bytes = projection_file_size(path, &projected);
                put_projection(ProjectionCacheEntry {
                    key: projection_key,
                    dataset: Some(projected),
                    storage_path: None,
                    estimated_bytes,
                });
                return rendered;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "invalid persistent projection cache entry; rebuilding");
                let _ = std::fs::remove_file(path);
            }
        }
    }
    if persistent_path.is_some() {
        PERSISTENT_PROJECTION_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    let (projected, storage_path) = project_vector_source(
        request,
        source_key,
        projection_bounds,
        simplify_tolerance,
        persistent_path.as_deref(),
    )?;
    let rendered = render(&projected, request);
    let estimated_bytes = persistent_path.as_deref().map_or_else(
        || {
            estimate_projection_bytes(
                &projection_key.source,
                &projected,
                projection_key.bounds.is_some(),
            )
        },
        |path| projection_file_size(path, &projected),
    );
    put_projection(ProjectionCacheEntry {
        key: projection_key,
        dataset: Some(projected),
        storage_path,
        estimated_bytes,
    });
    rendered
}

fn project_vector_source(
    request: &VectorRenderRequest,
    source_key: SourceCacheKey,
    projection_bounds: Option<[f64; 4]>,
    simplify_tolerance: Option<f64>,
    persistent_path: Option<&Path>,
) -> Result<(Dataset, Option<CString>), GdalError> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let full_projection = projection_bounds.is_none();
    let file_projection = persistent_path.is_some() || full_projection;
    let temporary_path = persistent_path.map(|path| {
        path.with_file_name(format!(
            ".{}.{}-{id}.tmp.fgb",
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("projection"),
            std::process::id()
        ))
    });
    let destination = CString::new(if let Some(path) = temporary_path.as_deref() {
        path.to_string_lossy().into_owned()
    } else if full_projection {
        format!(
            "/vsimem/cangling-gdal-vector-{}-{id}.fgb",
            std::process::id()
        )
    } else {
        format!("cangling-gdal-vector-{}-{id}", std::process::id())
    })
    .map_err(invalid_string)?;
    let mut translate_args = vec!["-f".to_owned()];
    if file_projection {
        translate_args.extend([
            "FlatGeobuf".to_owned(),
            "-lco".to_owned(),
            "SPATIAL_INDEX=YES".to_owned(),
        ]);
    } else {
        translate_args.push("MEM".to_owned());
    }
    translate_args.extend(["-t_srs".to_owned(), request.target_crs.clone()]);
    // Shapefiles can declare Polygon/LineString while containing multi-part
    // features. Promote both source forms to their multi geometry equivalent
    // so GDAL does not abort the cached projection on the first multi feature.
    translate_args.extend(["-nlt".to_owned(), "PROMOTE_TO_MULTI".to_owned()]);
    if let Some(bounds) = projection_bounds {
        translate_args.extend([
            "-spat_srs".to_owned(),
            request.target_crs.clone(),
            "-spat".to_owned(),
            bounds[0].to_string(),
            bounds[1].to_string(),
            bounds[2].to_string(),
            bounds[3].to_string(),
        ]);
    }
    if let Some(tolerance) = simplify_tolerance {
        translate_args.extend(["-simplify".to_owned(), tolerance.to_string()]);
    }
    let projection_started = Instant::now();
    let (source, source_cache_hit) = match take_source(&source_key) {
        Some(source) => (source, true),
        None => (
            SourceCacheEntry {
                key: source_key,
                dataset: open_vector_source(&request.source)?,
            },
            false,
        ),
    };
    let projected = unsafe { vector_translate(&source.dataset, &destination, &translate_args) };
    put_source(source);
    let projected = projected?;
    // FlatGeobuf writes its packed R-tree and final header while the output
    // dataset is being closed. Reopen it read-only before any spatial query;
    // querying through the translate output handle can otherwise return no
    // intersecting features.
    let projected = if file_projection {
        drop(projected);
        if let (Some(temporary), Some(final_path)) = (temporary_path.as_deref(), persistent_path) {
            std::fs::rename(temporary, final_path).map_err(|error| {
                let _ = std::fs::remove_file(temporary);
                GdalError::InvalidRequest(format!(
                    "cannot publish persistent projection cache `{}`: {error}",
                    final_path.display()
                ))
            })?;
            Dataset::open(final_path)?
        } else {
            let path = destination
                .to_str()
                .map_err(|error| GdalError::InvalidRequest(error.to_string()))?;
            Dataset::open(path)?
        }
    } else {
        projected
    };
    tracing::debug!(
        projection_ms = projection_started.elapsed().as_secs_f64() * 1000.0,
        source_cache_hit,
        spatial_index = file_projection,
        persistent = persistent_path.is_some(),
        buffer_pixels = request.buffer_pixels,
        simplify_tolerance = request.simplify_tolerance,
        "GDAL vector projection prepared"
    );
    Ok((
        projected,
        (full_projection && persistent_path.is_none()).then_some(destination),
    ))
}

fn persistent_projection_path(key: &ProjectionCacheKey) -> Result<Option<PathBuf>, GdalError> {
    let cache_path = std::env::var_os("VECTOR_PROJECTION_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("MAP_CACHE_ROOT")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("cache"))
                .join("vector-projection")
        });
    if cache_path.as_os_str().is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(&cache_path).map_err(|error| {
        GdalError::InvalidRequest(format!(
            "cannot create persistent projection cache `{}`: {error}",
            cache_path.display()
        ))
    })?;
    Ok(Some(cache_path.join(format!(
        "{}.fgb",
        projection_cache_fingerprint(key)
    ))))
}

fn persistent_projection_enabled() -> bool {
    std::env::var_os("VECTOR_PROJECTION_CACHE_PATH")
        .map(|path| !path.is_empty())
        .unwrap_or(true)
}

fn projection_cache_fingerprint(key: &ProjectionCacheKey) -> String {
    let mut hash = Sha256::new();
    hash.update(b"cangling-gdal-projection-v3\0");
    hash_field(&mut hash, key.source.path.to_string_lossy().as_bytes());
    hash_optional_field(&mut hash, key.source.layer.as_deref());
    hash_optional_field(&mut hash, key.source.encoding.as_deref());
    for (extension, size, modified) in &key.source.files {
        hash_field(&mut hash, extension.as_bytes());
        hash.update(size.to_le_bytes());
        hash.update(modified.to_le_bytes());
    }
    hash_field(&mut hash, key.target_crs.as_bytes());
    hash.update(key.simplify_tolerance.unwrap_or_default().to_le_bytes());
    if let Some(bounds) = key.bounds {
        hash.update([1]);
        for value in bounds {
            hash.update(value.to_le_bytes());
        }
    } else {
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

fn hash_optional_field(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_field(hash, value.as_bytes());
        }
        None => hash.update([0]),
    }
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update(value.len().to_le_bytes());
    hash.update(value);
}

fn projection_file_size(path: &Path, dataset: &Dataset) -> usize {
    path.metadata()
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or_else(|| {
            dataset
                .layer(0)
                .ok()
                .map(|layer| layer.feature_count() as usize)
                .unwrap_or_default()
                .saturating_mul(512)
        })
        .max(1024 * 1024)
}

fn source_cache_key(request: &VectorRenderRequest) -> Result<SourceCacheKey, GdalError> {
    let path = request
        .source
        .path
        .canonicalize()
        .unwrap_or_else(|_| request.source.path.clone());
    let mut files = Vec::new();
    for extension in ["shp", "shx", "dbf", "prj", "cpg", "qix"] {
        let candidate = path.with_extension(extension);
        let Ok(metadata) = candidate.metadata() else {
            continue;
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        files.push((extension.to_owned(), metadata.len(), modified));
    }
    if files.is_empty() {
        let metadata = path.metadata().map_err(|error| {
            GdalError::InvalidRequest(format!(
                "cannot inspect vector source `{}`: {error}",
                path.display()
            ))
        })?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        files.push((String::new(), metadata.len(), modified));
    }
    Ok(SourceCacheKey {
        path,
        layer: request.source.layer.clone(),
        encoding: request.source.encoding.clone(),
        files,
    })
}

fn projection_cache_bounds(request: &VectorRenderRequest) -> Option<[f64; 4]> {
    if std::env::var("VECTOR_PROJECTION_SCOPE")
        .unwrap_or_else(|_| "local".to_owned())
        .eq_ignore_ascii_case("full")
    {
        return None;
    }
    let factor = std::env::var("VECTOR_PROJECTION_METATILE_FACTOR")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(4) as f64;
    let width = request.bbox.max_x - request.bbox.min_x;
    let height = request.bbox.max_y - request.bbox.min_y;
    let span_x = width * factor;
    let span_y = height * factor;
    let buffer_x = width / f64::from(request.width) * f64::from(request.buffer_pixels);
    let buffer_y = height / f64::from(request.height) * f64::from(request.buffer_pixels);
    let min_x = snapped_grid_floor(request.bbox.min_x, span_x);
    let min_y = snapped_grid_floor(request.bbox.min_y, span_y);
    Some([
        min_x - buffer_x,
        min_y - buffer_y,
        min_x + span_x + buffer_x,
        min_y + span_y + buffer_y,
    ])
}

fn snapped_grid_floor(value: f64, span: f64) -> f64 {
    let quotient = value / span;
    let nearest = quotient.round();
    let cell = if (quotient - nearest).abs() <= 1.0e-9 {
        nearest
    } else {
        quotient.floor()
    };
    cell * span
}

fn cache_capacity(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn cache_budget_bytes(name: &str, default_megabytes: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_megabytes)
        .saturating_mul(1024 * 1024)
}

fn estimate_projection_bytes(
    source: &SourceCacheKey,
    dataset: &Dataset,
    is_local_projection: bool,
) -> usize {
    let source_bytes = source
        .files
        .iter()
        .map(|(_, size, _)| *size as usize)
        .sum::<usize>();
    let feature_bytes = dataset
        .layer(0)
        .ok()
        .map(|layer| layer.feature_count() as usize)
        .unwrap_or_default()
        .saturating_mul(512);
    if is_local_projection {
        // A MEM projection clipped to one metatile contains only intersecting
        // features. Charging it for the complete source file makes a small
        // local entry look as expensive as a full-country projection and
        // causes useful neighboring metatiles to be evicted immediately.
        feature_bytes.max(1024 * 1024)
    } else {
        source_bytes.saturating_mul(3).max(feature_bytes)
    }
}

fn take_source(key: &SourceCacheKey) -> Option<SourceCacheEntry> {
    let pool = VECTOR_SOURCE_POOL.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut pool = pool.lock().expect("vector source pool poisoned");
    let Some(index) = pool.iter().position(|entry| entry.key == *key) else {
        SOURCE_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    SOURCE_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    pool.remove(index)
}

fn put_source(entry: SourceCacheEntry) {
    let pool = VECTOR_SOURCE_POOL.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut pool = pool.lock().expect("vector source pool poisoned");
    pool.push_front(entry);
    pool.truncate(cache_capacity("VECTOR_DATASET_POOL_SIZE", 4));
}

fn take_projection(key: &ProjectionCacheKey) -> Option<ProjectionCacheEntry> {
    let cache = VECTOR_PROJECTION_CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut cache = cache.lock().expect("vector projection cache poisoned");
    let Some(index) = cache.iter().position(|entry| entry.key == *key) else {
        PROJECTION_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    PROJECTION_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    cache.remove(index)
}

fn put_projection(entry: ProjectionCacheEntry) {
    let cache = VECTOR_PROJECTION_CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut cache = cache.lock().expect("vector projection cache poisoned");
    cache.push_front(entry);
    let capacity = cache_capacity("VECTOR_PROJECTION_CACHE_SIZE", 32);
    let budget = cache_budget_bytes("VECTOR_PROJECTION_CACHE_MB", 2048);
    let mut estimated_bytes = cache
        .iter()
        .map(|entry| entry.estimated_bytes)
        .sum::<usize>();
    let mut evicted = 0_usize;
    while cache.len() > capacity || (estimated_bytes > budget && cache.len() > 1) {
        if let Some(entry) = cache.pop_back() {
            estimated_bytes = estimated_bytes.saturating_sub(entry.estimated_bytes);
            evicted += 1;
        }
    }
    PROJECTION_CACHE_EVICTIONS.fetch_add(evicted as u64, Ordering::Relaxed);
    tracing::debug!(
        entries = cache.len(),
        estimated_bytes,
        budget_bytes = budget,
        evicted,
        "GDAL projected dataset cache updated"
    );
}

pub(crate) fn vector_cache_stats() -> (
    usize,
    usize,
    u64,
    u64,
    usize,
    usize,
    u64,
    u64,
    u64,
    u64,
    u64,
) {
    let source_entries = VECTOR_SOURCE_POOL
        .get()
        .map(|pool| pool.lock().expect("vector source pool poisoned").len())
        .unwrap_or_default();
    let (projection_entries, projection_bytes) = VECTOR_PROJECTION_CACHE
        .get()
        .map(|cache| {
            let cache = cache.lock().expect("vector projection cache poisoned");
            (
                cache.len(),
                cache.iter().map(|entry| entry.estimated_bytes).sum(),
            )
        })
        .unwrap_or_default();
    (
        source_entries,
        projection_entries,
        SOURCE_CACHE_HITS.load(Ordering::Relaxed),
        SOURCE_CACHE_MISSES.load(Ordering::Relaxed),
        projection_bytes,
        cache_budget_bytes("VECTOR_PROJECTION_CACHE_MB", 2048),
        PROJECTION_CACHE_HITS.load(Ordering::Relaxed),
        PROJECTION_CACHE_MISSES.load(Ordering::Relaxed),
        PROJECTION_CACHE_EVICTIONS.load(Ordering::Relaxed),
        PERSISTENT_PROJECTION_HITS.load(Ordering::Relaxed),
        PERSISTENT_PROJECTION_MISSES.load(Ordering::Relaxed),
    )
}

unsafe fn warp(
    source: &Dataset,
    destination: &CStr,
    args: &[String],
) -> Result<Dataset, GdalError> {
    let (_strings, mut argv) = c_arguments(args)?;
    let options = gdal_sys::GDALWarpAppOptionsNew(argv.as_mut_ptr(), ptr::null_mut());
    if options.is_null() {
        return Err(last_error("GDALWarpAppOptionsNew"));
    }

    let mut source_handle = source.c_dataset();
    let mut usage_error = 0;
    let result = gdal_sys::GDALWarp(
        destination.as_ptr(),
        ptr::null_mut(),
        1,
        &mut source_handle,
        options,
        &mut usage_error,
    );
    gdal_sys::GDALWarpAppOptionsFree(options);
    if result.is_null() || usage_error != 0 {
        return Err(last_error("GDALWarp"));
    }
    Ok(Dataset::from_c_dataset(result))
}

unsafe fn translate(
    source: &Dataset,
    destination: &CStr,
    args: &[String],
) -> Result<Dataset, GdalError> {
    let (_strings, mut argv) = c_arguments(args)?;
    let options = gdal_sys::GDALTranslateOptionsNew(argv.as_mut_ptr(), ptr::null_mut());
    if options.is_null() {
        return Err(last_error("GDALTranslateOptionsNew"));
    }

    let mut usage_error = 0;
    let result = gdal_sys::GDALTranslate(
        destination.as_ptr(),
        source.c_dataset(),
        options,
        &mut usage_error,
    );
    gdal_sys::GDALTranslateOptionsFree(options);
    if result.is_null() || usage_error != 0 {
        return Err(last_error("GDALTranslate"));
    }
    Ok(Dataset::from_c_dataset(result))
}

unsafe fn vector_translate(
    source: &Dataset,
    destination: &CStr,
    args: &[String],
) -> Result<Dataset, GdalError> {
    let (_strings, mut argv) = c_arguments(args)?;
    let options = gdal_sys::GDALVectorTranslateOptionsNew(argv.as_mut_ptr(), ptr::null_mut());
    if options.is_null() {
        return Err(last_error("GDALVectorTranslateOptionsNew"));
    }

    let mut source_handle = source.c_dataset();
    let mut usage_error = 0;
    let result = gdal_sys::GDALVectorTranslate(
        destination.as_ptr(),
        ptr::null_mut(),
        1,
        &mut source_handle,
        options,
        &mut usage_error,
    );
    gdal_sys::GDALVectorTranslateOptionsFree(options);
    if result.is_null() || usage_error != 0 {
        return Err(last_error("GDALVectorTranslate"));
    }
    Ok(Dataset::from_c_dataset(result))
}

unsafe fn read_vsimem(path: &CStr) -> Result<Vec<u8>, GdalError> {
    let mut length = 0;
    let data = gdal_sys::VSIGetMemFileBuffer(path.as_ptr(), &mut length, 0);
    if data.is_null() {
        return Err(last_error("VSIGetMemFileBuffer"));
    }
    Ok(std::slice::from_raw_parts(data, length as usize).to_vec())
}

fn c_arguments(args: &[String]) -> Result<(Vec<CString>, Vec<*mut c_char>), GdalError> {
    let strings = args
        .iter()
        .map(|value| CString::new(value.as_str()).map_err(invalid_string))
        .collect::<Result<Vec<_>, _>>()?;
    let mut pointers = strings
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    pointers.push(ptr::null_mut());
    Ok((strings, pointers))
}

fn invalid_string(error: std::ffi::NulError) -> GdalError {
    GdalError::InvalidRequest(error.to_string())
}

unsafe fn last_error(operation: &'static str) -> GdalError {
    let pointer = gdal_sys::CPLGetLastErrorMsg();
    let message = if pointer.is_null() {
        "unknown GDAL error".to_owned()
    } else {
        CStr::from_ptr(pointer).to_string_lossy().into_owned()
    };
    GdalError::InvalidRequest(format!("{operation} failed: {message}"))
}

struct VsiCleanup<'a> {
    paths: Vec<&'a CStr>,
}

impl<'a> VsiCleanup<'a> {
    fn pair(warped: &'a CStr, image: &'a CStr) -> Self {
        Self::paths(vec![warped, image])
    }

    fn paths(paths: Vec<&'a CStr>) -> Self {
        Self { paths }
    }
}

impl Drop for VsiCleanup<'_> {
    fn drop(&mut self) {
        for path in &self.paths {
            unsafe {
                gdal_sys::VSIUnlink(path.as_ptr());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        projection_cache_fingerprint, snapped_grid_floor, ProjectionCacheKey, SourceCacheKey,
    };
    use std::path::PathBuf;

    #[test]
    fn keeps_a_rounded_metatile_boundary_in_the_next_cell() {
        let span = 76.437_028_285_176_25;
        let boundary = 12_664_239.720_344_571;
        let quotient = boundary / span;
        let slightly_below_boundary = (quotient - 1.0e-12) * span;

        assert_eq!(snapped_grid_floor(slightly_below_boundary, span), boundary);
    }

    #[test]
    fn still_floors_values_that_are_really_inside_a_cell() {
        assert_eq!(snapped_grid_floor(24.9, 10.0), 20.0);
        assert_eq!(snapped_grid_floor(-24.9, 10.0), -30.0);
    }

    #[test]
    fn persistent_projection_key_changes_with_source_or_projection() {
        let source = SourceCacheKey {
            path: PathBuf::from("/data/china.shp"),
            layer: None,
            encoding: Some("UTF-8".to_owned()),
            files: vec![("shp".to_owned(), 128, 42)],
        };
        let key = ProjectionCacheKey {
            source: source.clone(),
            target_crs: "EPSG:3857".to_owned(),
            simplify_tolerance: None,
            bounds: None,
        };
        let fingerprint = projection_cache_fingerprint(&key);
        assert_eq!(fingerprint.len(), 64);
        assert_eq!(fingerprint, projection_cache_fingerprint(&key));

        let mut changed_source = key.clone();
        changed_source.source.files[0].2 += 1;
        assert_ne!(fingerprint, projection_cache_fingerprint(&changed_source));

        let mut changed_bounds = key;
        changed_bounds.bounds = Some([1, 2, 3, 4]);
        assert_ne!(fingerprint, projection_cache_fingerprint(&changed_bounds));
    }
}
