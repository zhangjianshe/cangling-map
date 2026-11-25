//! Standard, versioned read routes and same-origin management of local datasets.
use crate::AppState;
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::OnceLock};
type Error = (StatusCode, String);
fn bad(e: impl ToString) -> Error {
    (StatusCode::BAD_REQUEST, e.to_string())
}
fn internal(e: impl ToString) -> Error {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
fn root() -> PathBuf {
    std::env::var_os("TILES3D_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data/3dtiles"))
}
// Pools release idle connections; the directory database is the sole publication authority.
async fn store(id: &str, create: bool) -> Result<map_catalog::SqliteCatalog, Error> {
    map_3dtiles::valid_id(id).map_err(bad)?;
    static STORES: OnceLock<
        tokio::sync::Mutex<std::collections::HashMap<PathBuf, map_catalog::SqliteCatalog>>,
    > = OnceLock::new();
    let mut stores = STORES.get_or_init(Default::default).lock().await;
    let directory = root().join(id);
    if create {
        tokio::fs::create_dir_all(root()).await.map_err(internal)?;
        // Validate existing ancestors before creating anything below a dataset directory.
        if directory.exists() {
            map_3dtiles::safe_path(&root(), id).map_err(bad)?;
        }
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(internal)?;
        map_3dtiles::safe_path(&root(), id).map_err(bad)?;
        tokio::fs::create_dir_all(directory.join("source"))
            .await
            .map_err(internal)?;
    }
    map_3dtiles::safe_path(&root(), id)
        .map_err(|_| (StatusCode::NOT_FOUND, "数据集目录不存在".into()))?;
    let path = directory.join("dataset.sqlite");
    if !create && !path.exists() {
        return Err((StatusCode::NOT_FOUND, "数据集未登记".into()));
    }
    // SQLite sidecars are also inside the trusted dataset directory.
    for name in ["dataset.sqlite", "dataset.sqlite-wal", "dataset.sqlite-shm"] {
        if tokio::fs::symlink_metadata(directory.join(name))
            .await
            .is_ok()
        {
            map_3dtiles::safe_path(&root(), &format!("{id}/{name}")).map_err(bad)?;
        }
    }
    if let Some(db) = stores.get(&path) {
        return Ok(db.clone());
    }
    let db = map_catalog::SqliteCatalog::open_tiles3d(&path)
        .await
        .map_err(internal)?;
    stores.insert(path, db.clone());
    Ok(db)
}
pub(crate) async fn migrate(catalog: &map_catalog::SqliteCatalog) -> anyhow::Result<()> {
    for dataset in catalog.list_tiles3d().await? {
        let local = store(&dataset.id, true)
            .await
            .map_err(|(_, e)| anyhow::anyhow!(e))?;
        local.import_tiles3d(catalog, &dataset).await?;
    }
    Ok(())
}
fn writes(headers: &HeaderMap) -> Result<(), Error> {
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !matches!(v, "same-origin" | "none"))
    {
        return Err((StatusCode::FORBIDDEN, "请从管理页面操作".into()));
    }
    Ok(())
}
async fn dataset(_s: &AppState, id: &str) -> Result<map_catalog::Tiles3dDataset, Error> {
    map_3dtiles::valid_id(id).map_err(bad)?;
    store(id, false)
        .await?
        .get_tiles3d(id)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "数据集不存在".into()))
}
pub(crate) async fn list(State(_s): State<AppState>) -> Result<Json<Value>, Error> {
    let mut datasets = Vec::new();
    if root().exists() {
        let mut entries = tokio::fs::read_dir(root()).await.map_err(internal)?;
        while let Some(entry) = entries.next_entry().await.map_err(internal)? {
            let id = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type().await.map_err(internal)?.is_dir()
                || map_3dtiles::valid_id(&id).is_err()
                || !entry.path().join("dataset.sqlite").exists()
            {
                continue;
            }
            if let Some(d) = store(&id, false)
                .await?
                .get_tiles3d(&id)
                .await
                .map_err(internal)?
            {
                datasets.push(d);
            }
        }
    }
    datasets.sort_by(|a, b| (&a.title, &a.id).cmp(&(&b.title, &b.id)));
    Ok(Json(json!({"datasets":datasets})))
}
#[derive(Deserialize)]
pub(crate) struct Registration {
    title: String,
}
pub(crate) async fn register(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Registration>,
) -> Result<Json<Value>, Error> {
    writes(&headers)?;
    map_3dtiles::valid_id(&id).map_err(bad)?;
    let title = body.title.trim();
    if title.is_empty() || title.chars().count() > 128 {
        return Err(bad("名称须为1–128字符"));
    }
    store(&id, true)
        .await?
        .register_tiles3d(&id, title)
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({"dataset":dataset(&s,&id).await?,"source":root().join(&id).join("source")}),
    ))
}
pub(crate) async fn inspect(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    writes(&headers)?;
    dataset(&s, &id).await?;
    let source = map_3dtiles::safe_path(&root(), &format!("{id}/source")).map_err(bad)?;
    let _permit = permit()?;
    let manifest = tokio::task::spawn_blocking(move || map_3dtiles::inspect(&source, None))
        .await
        .map_err(internal)?
        .map_err(bad)?;
    Ok(Json(json!(manifest)))
}
fn permit() -> Result<tokio::sync::SemaphorePermit<'static>, Error> {
    static LIMIT: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    LIMIT
        .get_or_init(|| tokio::sync::Semaphore::new(2))
        .try_acquire()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "有数据集正在校验或发布，请稍后重试".into(),
            )
        })
}
pub(crate) async fn publish(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    writes(&headers)?;
    dataset(&s, &id).await?;
    let _permit = permit()?;
    let target = id.clone();
    let manifest = tokio::task::spawn_blocking(move || map_3dtiles::publish(&root(), &target))
        .await
        .map_err(internal)?
        .map_err(bad)?;
    store(&id, false)
        .await?
        .publish_tiles3d(&id, &manifest.revision, &json!(manifest))
        .await
        .map_err(internal)?;
    Ok(Json(
        json!({"dataset":dataset(&s,&id).await?,"manifest":manifest}),
    ))
}
pub(crate) async fn disable(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, Error> {
    writes(&headers)?;
    dataset(&s, &id).await?;
    store(&id, false)
        .await?
        .disable_tiles3d(&id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({"dataset":dataset(&s,&id).await?})))
}
pub(crate) async fn detail(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Error> {
    let d = dataset(&s, &id).await?;
    let manifest = if let Some(ref rev) = d.revision {
        store(&id, false)
            .await?
            .tiles3d_manifest(&id, rev)
            .await
            .map_err(internal)?
    } else {
        None
    };
    Ok(Json(
        json!({"dataset":d,"manifest":manifest,"source":root().join(id).join("source")}),
    ))
}
pub(crate) async fn entry(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, Error> {
    let d = dataset(&s, &id).await?;
    if !d.enabled {
        return Err((StatusCode::NOT_FOUND, "数据集未发布".into()));
    }
    let rev = d
        .revision
        .ok_or((StatusCode::NOT_FOUND, "数据集未发布".into()))?;
    // Return version-qualified content URIs: clients may resolve against the requested
    // URL rather than a redirect's final URL (including Cesium Resource).
    let full = map_3dtiles::safe_path(&root(), &format!("{id}/versions/{rev}/tileset.json"))
        .map_err(bad)?;
    let manifest = store(&id, false)
        .await?
        .tiles3d_manifest(&id, &rev)
        .await
        .map_err(internal)?
        .ok_or_else(|| internal("版本清单不存在"))?;
    let info = &manifest["files"]["tileset.json"];
    let data = tokio::fs::read(full).await.map_err(internal)?;
    use sha2::Digest;
    if info["bytes"].as_u64() != Some(data.len() as u64)
        || info["sha256"].as_str() != Some(format!("{:x}", sha2::Sha256::digest(&data)).as_str())
    {
        return Err(internal("发布入口完整性校验失败"));
    }
    let mut document: Value = serde_json::from_slice(&data).map_err(internal)?;
    fn qualify(tile: &mut Value, prefix: &str) {
        if let Some(content) = tile.get_mut("content") {
            qualify_content(content, prefix);
        }
        if let Some(contents) = tile.get_mut("contents").and_then(Value::as_array_mut) {
            for content in contents {
                qualify_content(content, prefix);
            }
        }
        if let Some(children) = tile.get_mut("children").and_then(Value::as_array_mut) {
            for child in children {
                qualify(child, prefix);
            }
        }
    }
    fn qualify_content(content: &mut Value, prefix: &str) {
        for key in ["uri", "url"] {
            if let Some(uri) = content[key].as_str() {
                content[key] = json!(format!("{prefix}{uri}"));
            }
        }
    }
    qualify(&mut document["root"], &format!("versions/{rev}/"));
    let mut response = Json(document).into_response();
    response
        .headers_mut()
        .insert("x-tiles3d-cache", "DISABLED".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}
pub(crate) async fn asset(
    State(s): State<AppState>,
    Path((id, revision, path)): Path<(String, String, String)>,
    request: Request,
) -> Result<Response, Error> {
    let d = dataset(&s, &id).await?;
    if !d.enabled {
        return Err((StatusCode::NOT_FOUND, "数据集已停用".into()));
    }
    if revision.len() != 64 || !revision.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(bad("版本无效"));
    }
    map_3dtiles::relative(&path).map_err(bad)?;
    let info = store(&id, false)
        .await?
        .tiles3d_file(&id, &revision, &path)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "文件未发布".into()))?;
    let full = map_3dtiles::safe_path(&root(), &format!("{id}/versions/{revision}/{path}"))
        .map_err(|_| (StatusCode::NOT_FOUND, "文件不存在".into()))?;
    let etag = format!(
        "\"{}\"",
        info["sha256"]
            .as_str()
            .ok_or_else(|| internal("文件元数据损坏"))?
    );
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(&etag)
    {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
        )
            .into_response());
    }
    let mut service = tower_http::services::ServeFile::new(full);
    let mut response = service
        .try_call(request)
        .await
        .map_err(internal)?
        .map(Body::new);
    response
        .headers_mut()
        .insert("x-tiles3d-cache", "DISABLED".parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        info["mime"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .parse()
            .map_err(internal)?,
    );
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().map_err(internal)?);
    // Revalidate even immutable versions so disabling takes effect in browser caches.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    Ok(response)
}
