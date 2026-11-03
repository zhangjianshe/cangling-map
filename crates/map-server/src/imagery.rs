use std::{convert::Infallible, time::Duration};

use axum::{
    body::Bytes as AxumBytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{sse::Event, sse::KeepAlive, IntoResponse, Response, Sse},
    routing::{get, post, put},
    Json, Router,
};
use bytes::Bytes;
use futures_util::stream::unfold;
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use map_imagery::{
    ImageryError, JobInput, JobStatus, Paged, Repository, RepositoryInput, TileFormat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{percent_encode, ApiError, AppState};

pub(crate) const MAX_TILE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ImageryTileClient {
    base_url: String,
    token: String,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl ImageryTileClient {
    pub(crate) fn from_env() -> Result<Self, String> {
        let base_url = std::env::var("CANGLING_TILE_URL")
            .or_else(|_| std::env::var("TILE_CACHE_URL"))
            .unwrap_or_else(|_| "http://127.0.0.1:7601".into())
            .trim()
            .trim_end_matches('/')
            .to_owned();
        if !base_url.starts_with("http://") {
            return Err("CANGLING_TILE_URL currently requires an http:// URL".into());
        }
        Ok(Self {
            base_url,
            token: std::env::var("CANGLING_TILE_AUTH_TOKEN")
                .or_else(|_| std::env::var("CANGLING_TILE_TOKEN"))
                .or_else(|_| std::env::var("TILE_CACHE_TOKEN"))
                .unwrap_or_default(),
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    pub(crate) async fn put(
        &self,
        repository: &Repository,
        zoom: u8,
        col: u32,
        row: u32,
        format: TileFormat,
        bytes: &[u8],
    ) -> Result<(), String> {
        let url = format!(
            "{}/tiles/{}/{}/{}/{}/{}.{}",
            self.base_url,
            repository.tile_database,
            repository.tile_item,
            zoom,
            col,
            row,
            format.extension()
        );
        let mut builder = hyper::Request::put(url)
            .header(header::CONTENT_TYPE, format.content_type())
            .header("x-tile-database-name", percent_encode(&repository.name))
            .header("x-tile-layer-name", percent_encode(&repository.name));
        if !self.token.is_empty() {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        }
        let request = builder
            .body(Full::new(Bytes::copy_from_slice(bytes)))
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|error| error.to_string())?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map(|body| String::from_utf8_lossy(&body.to_bytes()).into_owned())
            .unwrap_or_default();
        Err(format!(
            "cangling-tile PUT returned {status}: {}",
            body.trim()
        ))
    }
}

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/imagery/leadership", get(leadership_status))
        .route("/api/imagery/worker-metrics", get(worker_metrics))
        .route("/api/imagery/snapshot", get(export_snapshot))
        .route(
            "/api/imagery/repositories",
            get(list_repositories).post(create_repository),
        )
        .route(
            "/api/imagery/repositories/{id}",
            get(get_repository)
                .put(update_repository)
                .delete(delete_repository),
        )
        .route("/api/imagery/jobs", get(list_jobs).post(create_job))
        .route("/api/imagery/jobs/{id}", get(get_job).delete(delete_job))
        .route("/api/imagery/jobs/{id}/chunks", get(list_job_chunks))
        .route(
            "/api/imagery/jobs/{id}/chunks/{chunk_id}/complete",
            post(complete_browser_chunk),
        )
        .route(
            "/api/imagery/jobs/{id}/chunks/{chunk_id}/missing",
            get(list_missing_chunk_tiles),
        )
        .route("/api/imagery/jobs/{id}/events", get(job_events))
        .route("/api/imagery/jobs/{id}/heartbeat", post(heartbeat_job))
        .route("/api/imagery/jobs/{id}/takeover", post(takeover_job))
        .route("/api/imagery/jobs/{id}/pause", post(pause_job))
        .route("/api/imagery/jobs/{id}/resume", post(resume_job))
        .route("/api/imagery/jobs/{id}/cancel", post(cancel_job))
        .route("/api/imagery/jobs/{id}/leases", post(issue_lease))
        .route("/api/imagery/uploads/{z}/{x}/{y_ext}", put(upload_tile))
}

#[derive(Debug, Default, Deserialize)]
struct PageQuery {
    page: Option<u32>,
    page_size: Option<u32>,
    q: Option<String>,
    repository_id: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateJobRequest {
    repository_id: String,
    mode: map_imagery::JobMode,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    bounds: Option<[f64; 4]>,
    #[serde(default)]
    total_tiles: u64,
}

#[derive(Debug, Serialize)]
struct RepositoryView {
    #[serde(flatten)]
    repository: Repository,
    source_url_templates: Vec<String>,
    browser_url: String,
    storage_path: String,
}

impl From<Repository> for RepositoryView {
    fn from(repository: Repository) -> Self {
        let source_url_templates = repository
            .source_url_templates()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let browser_url = repository_browser_url(&repository);
        let storage_path = repository_storage_path(&repository);
        Self {
            repository,
            source_url_templates,
            browser_url,
            storage_path,
        }
    }
}

fn repository_browser_url(repository: &Repository) -> String {
    let base =
        std::env::var("CANGLING_TILE_PUBLIC_URL").unwrap_or_else(|_| "/cangling-tile".into());
    repository_browser_url_with_base(repository, &base)
}

fn repository_browser_url_with_base(repository: &Repository, base: &str) -> String {
    format!(
        "{}/tiles/{}/{}/{{z}}/{{x}}/{{y}}.{}",
        base.trim().trim_end_matches('/'),
        repository.tile_database,
        repository.tile_item,
        repository.format.extension()
    )
}

fn repository_storage_path(repository: &Repository) -> String {
    let root =
        std::env::var("CANGLING_TILE_STORAGE_ROOT").unwrap_or_else(|_| "/app/tiledata".into());
    repository_storage_path_with_root(repository, &root)
}

fn repository_storage_path_with_root(repository: &Repository, root: &str) -> String {
    let (database_prefix, database_suffix) = shard_parts(&repository.tile_database);
    let (item_prefix, item_suffix) = shard_parts(&repository.tile_item);
    format!(
        "{}/{}/{}/{}/{}",
        root.trim().trim_end_matches('/'),
        database_prefix,
        database_suffix,
        item_prefix,
        item_suffix
    )
}

fn shard_parts(value: &str) -> (&str, &str) {
    value.get(..4).zip(value.get(4..)).unwrap_or((value, ""))
}

async fn list_repositories(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Response {
    match state
        .imagery
        .list_repositories(
            query.page.unwrap_or(1),
            query.page_size.unwrap_or(20),
            query.q.as_deref(),
        )
        .await
    {
        Ok(page) => Json(Paged {
            items: page.items.into_iter().map(RepositoryView::from).collect(),
            page: page.page,
            page_size: page.page_size,
            total: page.total,
            total_pages: page.total_pages,
        })
        .into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn get_repository(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.imagery.get_repository(&id).await {
        Ok(Some(repository)) => Json(RepositoryView::from(repository)).into_response(),
        Ok(None) => imagery_error(ImageryError::NotFound(id)),
        Err(error) => imagery_error(error),
    }
}

async fn create_repository(
    State(state): State<AppState>,
    Json(input): Json<RepositoryInput>,
) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    match state.imagery.create_repository(&input).await {
        Ok(repository) => {
            (StatusCode::CREATED, Json(RepositoryView::from(repository))).into_response()
        }
        Err(error) => imagery_error(error),
    }
}

async fn update_repository(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<RepositoryInput>,
) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    match state.imagery.update_repository(&id, &input).await {
        Ok(repository) => Json(RepositoryView::from(repository)).into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn delete_repository(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    match state.imagery.delete_repository(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => imagery_error(ImageryError::NotFound(id)),
        Err(error) => imagery_error(error),
    }
}

async fn list_jobs(State(state): State<AppState>, Query(query): Query<PageQuery>) -> Response {
    match state
        .imagery
        .list_jobs(
            query.page.unwrap_or(1),
            query.page_size.unwrap_or(20),
            query.repository_id.as_deref(),
        )
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn get_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.imagery.get_job(&id).await {
        Ok(Some(job)) => Json(job).into_response(),
        Ok(None) => imagery_error(ImageryError::NotFound(id)),
        Err(error) => imagery_error(error),
    }
}

async fn delete_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    match state.imagery.delete_job(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => imagery_error(ImageryError::NotFound(id)),
        Err(error) => imagery_error(error),
    }
}

async fn list_job_chunks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Response {
    match state
        .imagery
        .list_job_chunks_page(
            &id,
            query.page.unwrap_or(1),
            query.page_size.unwrap_or(100),
            query.status.as_deref(),
        )
        .await
    {
        Ok(chunks) => Json(chunks).into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn complete_browser_chunk(
    State(state): State<AppState>,
    Path((id, chunk_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    let token = match headers
        .get("x-imagery-lease")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => token,
        None => return imagery_error(ImageryError::InvalidLease),
    };
    let lease = match state.imagery_lease_signer.verify(token) {
        Ok(lease) if lease.job_id == id => lease,
        Ok(_) => return imagery_error(ImageryError::InvalidLease),
        Err(error) => return imagery_error(error),
    };
    let job = match state.imagery.get_job(&id).await {
        Ok(Some(job)) if job.repository_id == lease.repository_id => job,
        Ok(Some(_)) | Ok(None) => return imagery_error(ImageryError::InvalidLease),
        Err(error) => return imagery_error(error),
    };
    if job.id != lease.job_id {
        return imagery_error(ImageryError::InvalidLease);
    }
    match state.imagery.complete_browser_chunk(&id, &chunk_id).await {
        Ok(chunk) => Json(chunk).into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn job_events(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.imagery.get_job(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return imagery_error(ImageryError::NotFound(id)),
        Err(error) => return imagery_error(error),
    }
    let store = state.imagery.clone();
    let interval = tokio::time::interval(Duration::from_secs(1));
    let stream = unfold(
        (store, id, None::<String>, false, interval),
        |(store, job_id, mut previous, done, mut interval)| async move {
            if done {
                return None;
            }
            loop {
                interval.tick().await;
                match store.job_progress(&job_id).await {
                    Ok(progress) => {
                        let payload = match serde_json::to_string(&progress) {
                            Ok(payload) => payload,
                            Err(error) => {
                                let event = Event::default()
                                    .event("error")
                                    .data(format!("could not serialize job progress: {error}"));
                                return Some((
                                    Ok::<Event, Infallible>(event),
                                    (store, job_id, previous, true, interval),
                                ));
                            }
                        };
                        if previous.as_deref() == Some(payload.as_str()) {
                            continue;
                        }
                        previous = Some(payload.clone());
                        let terminal = matches!(
                            progress.job.status,
                            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled
                        );
                        let event_id = format!(
                            "{}:{}:{}:{}",
                            progress.job.updated_at,
                            progress.job.completed_tiles,
                            progress.job.failed_tiles,
                            progress.latest_chunk_updated_at.as_deref().unwrap_or("-")
                        );
                        let event = Event::default()
                            .event(if terminal { "terminal" } else { "progress" })
                            .id(event_id)
                            .data(payload);
                        return Some((
                            Ok::<Event, Infallible>(event),
                            (store, job_id, previous, terminal, interval),
                        ));
                    }
                    Err(error) => {
                        let event = Event::default().event("error").data(error.to_string());
                        return Some((
                            Ok::<Event, Infallible>(event),
                            (store, job_id, previous, true, interval),
                        ));
                    }
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

async fn create_job(
    State(state): State<AppState>,
    Json(input): Json<CreateJobRequest>,
) -> Response {
    let leader_term = match require_imagery_leader(&state).await {
        Ok(term) => term,
        Err(response) => return response,
    };
    let input = JobInput {
        repository_id: input.repository_id,
        mode: input.mode,
        min_zoom: input.min_zoom,
        max_zoom: input.max_zoom,
        bounds: input.bounds,
        total_tiles: input.total_tiles,
        leader_term,
    };
    let plan_inline = input.mode == map_imagery::JobMode::Browser
        || state.imagery_leadership.snapshot().await.mode == "standalone";
    match state.imagery.create_job(&input).await {
        Ok(job) => {
            // Browser jobs need ranges immediately for lease assignment, while
            // standalone mode has no Broker command consumer. Server/hybrid
            // jobs in cluster mode are planned from the durable command.
            if plan_inline {
                if let Err(error) = state
                    .imagery
                    .ensure_job_chunks(&job.id, super::leadership::configured_chunk_tiles())
                    .await
                {
                    return imagery_error(error);
                }
                return match state.imagery.get_job(&job.id).await {
                    Ok(Some(planned)) => (StatusCode::CREATED, Json(planned)).into_response(),
                    Ok(None) => imagery_error(ImageryError::NotFound(job.id)),
                    Err(error) => imagery_error(error),
                };
            }
            (StatusCode::CREATED, Json(job)).into_response()
        }
        Err(error) => imagery_error(error),
    }
}

async fn list_missing_chunk_tiles(
    State(state): State<AppState>,
    Path((id, chunk_id)): Path<(String, String)>,
) -> Response {
    match state.imagery.missing_tiles_for_chunk(&id, &chunk_id).await {
        Ok(tiles) => Json(tiles).into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn pause_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    transition_job(&state, &id, JobStatus::Paused).await
}

async fn resume_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    transition_job(&state, &id, JobStatus::Running).await
}

async fn cancel_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    transition_job(&state, &id, JobStatus::Cancelled).await
}

async fn heartbeat_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    match state.imagery.heartbeat_browser_job(&id).await {
        Ok(job) => Json(job).into_response(),
        Err(error) => imagery_error(error),
    }
}

#[derive(Debug, Serialize)]
struct TakeoverResponse {
    job_id: String,
    queued_chunks: u64,
}

async fn takeover_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    let job = match state.imagery.get_job(&id).await {
        Ok(Some(job)) => job,
        Ok(None) => return imagery_error(ImageryError::NotFound(id)),
        Err(error) => return imagery_error(error),
    };
    if job.mode != map_imagery::JobMode::Hybrid {
        return imagery_error(ImageryError::Conflict(
            "only hybrid imagery jobs can be taken over".into(),
        ));
    }
    if job.status != JobStatus::Running {
        return imagery_error(ImageryError::Conflict(
            "hybrid imagery job is not running".into(),
        ));
    }
    if state.imagery_leadership.snapshot().await.mode == "standalone" {
        return imagery_error(ImageryError::Conflict(
            "hybrid server takeover requires CANGLING_BROKER_HTTP_URL".into(),
        ));
    }
    // The job command and an operator takeover request may race. Planning is
    // deterministic and idempotent, so make sure ranges exist before moving
    // the still-pending work into Broker's durable queue.
    if let Err(error) = state
        .imagery
        .ensure_job_chunks(&id, super::leadership::configured_chunk_tiles())
        .await
    {
        return imagery_error(error);
    }
    match state.imagery.takeover_hybrid_job(&id).await {
        Ok(queued_chunks) => Json(TakeoverResponse {
            job_id: id,
            queued_chunks,
        })
        .into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn transition_job(state: &AppState, id: &str, target: JobStatus) -> Response {
    if let Err(response) = require_imagery_leader(state).await {
        return response;
    }
    match state.imagery.transition_job(id, target).await {
        Ok(job) => Json(job).into_response(),
        Err(error) => imagery_error(error),
    }
}

#[derive(Debug, Default, Deserialize)]
struct LeaseRequest {
    ttl_seconds: Option<u64>,
}

async fn issue_lease(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<LeaseRequest>,
) -> Response {
    if let Err(response) = require_imagery_leader(&state).await {
        return response;
    }
    let job = match state.imagery.get_job(&id).await {
        Ok(Some(job)) => job,
        Ok(None) => return imagery_error(ImageryError::NotFound(id)),
        Err(error) => return imagery_error(error),
    };
    let repository = match state.imagery.get_repository(&job.repository_id).await {
        Ok(Some(repository)) => repository,
        Ok(None) => return imagery_error(ImageryError::NotFound(job.repository_id)),
        Err(error) => return imagery_error(error),
    };
    match state
        .imagery_lease_signer
        .issue(&job, &repository, input.ttl_seconds.unwrap_or(300))
    {
        Ok(lease) => Json(lease).into_response(),
        Err(error) => imagery_error(error),
    }
}

async fn upload_tile(
    State(state): State<AppState>,
    Path((zoom, col, y_ext)): Path<(u8, u32, String)>,
    headers: HeaderMap,
    body: AxumBytes,
) -> Response {
    let token = match headers
        .get("x-imagery-lease")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(token) => token,
        None => return imagery_error(ImageryError::InvalidLease),
    };
    let lease = match state.imagery_lease_signer.verify(token) {
        Ok(lease) => lease,
        Err(error) => return imagery_error(error),
    };
    let (row, format) = match parse_tile_path(&y_ext) {
        Ok(value) => value,
        Err(error) => return imagery_error(error),
    };
    if body.is_empty() || body.len() > MAX_TILE_BYTES {
        return imagery_error(ImageryError::Invalid(format!(
            "tile body must contain 1 to {MAX_TILE_BYTES} bytes"
        )));
    }
    if !valid_tile_coordinate(zoom, col, row) || !lease.formats.contains(&format) {
        return imagery_error(ImageryError::Invalid(
            "tile coordinate or format is outside the lease".into(),
        ));
    }
    if let Err(error) = validate_tile_bytes(format, &body) {
        return imagery_error(error);
    }
    let job = match state.imagery.get_job(&lease.job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => return imagery_error(ImageryError::InvalidLease),
        Err(error) => return imagery_error(error),
    };
    let repository = match state.imagery.get_repository(&lease.repository_id).await {
        Ok(Some(repository)) => repository,
        Ok(None) => return imagery_error(ImageryError::InvalidLease),
        Err(error) => return imagery_error(error),
    };
    if job.repository_id != repository.id
        || job.id != lease.job_id
        || format != repository.format
        || zoom < repository.min_zoom
        || zoom > repository.max_zoom
        || !map_imagery::job_contains_tile(&job, zoom, col, row)
    {
        return imagery_error(ImageryError::InvalidLease);
    }
    let digest = format!("{:x}", Sha256::digest(&body));
    match state
        .imagery
        .get_tile_receipt(&job.id, zoom, col, row, format)
        .await
    {
        Ok(Some(receipt)) if receipt.sha256 == digest => return Json(receipt).into_response(),
        Ok(Some(_)) => {
            return imagery_error(ImageryError::Conflict(
                "the tile coordinate was already committed with different content".into(),
            ))
        }
        Ok(None) => {}
        Err(error) => return imagery_error(error),
    }
    match state
        .imagery
        .repository_tile_exists(&repository.id, zoom, col, row)
        .await
    {
        Ok(true) => {
            return Json(map_imagery::TileReceipt {
                inserted: false,
                job_id: job.id,
                zoom,
                col,
                row,
                format,
                size_bytes: body.len() as u64,
                sha256: digest,
            })
            .into_response()
        }
        Ok(false) => {}
        Err(error) => return imagery_error(error),
    }
    if !job.status.accepts_uploads() {
        return imagery_error(ImageryError::Conflict(
            "job is not accepting tile uploads".into(),
        ));
    }
    if let Err(error) = state
        .imagery_tile_client
        .put(&repository, zoom, col, row, format, &body)
        .await
    {
        return (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("cangling-tile write failed: {error}"),
            }),
        )
            .into_response();
    }
    match state
        .imagery
        .record_tile_stored(&job.id, zoom, col, row, format, &body)
        .await
    {
        Ok(receipt) => {
            let status = if receipt.inserted {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status, Json(receipt)).into_response()
        }
        Err(error) => imagery_error(error),
    }
}

async fn leadership_status(State(state): State<AppState>) -> Response {
    Json(state.imagery_leadership.snapshot().await).into_response()
}

async fn worker_metrics(State(state): State<AppState>) -> Response {
    Json(state.imagery_leadership.worker_metrics()).into_response()
}

async fn export_snapshot(State(state): State<AppState>) -> Response {
    let term = match require_imagery_leader(&state).await {
        Ok(term) => term,
        Err(response) => return response,
    };
    match state.imagery.export_snapshot().await {
        Ok(mut snapshot) => {
            snapshot.leader_id = Some(state.imagery_leadership.candidate_id().to_owned());
            snapshot.leader_term = term;
            Json(snapshot).into_response()
        }
        Err(error) => imagery_error(error),
    }
}

async fn require_imagery_leader(state: &AppState) -> Result<u64, Response> {
    if let Some(term) = state.imagery_leadership.leader_term().await {
        return Ok(term);
    }
    let leadership = state.imagery_leadership.snapshot().await;
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "imagery repository writes are accepted only by the current cangling-map master",
            "leadership": leadership,
        })),
    )
        .into_response())
}

fn parse_tile_path(value: &str) -> Result<(u32, TileFormat), ImageryError> {
    let (row, extension) = value
        .rsplit_once('.')
        .ok_or_else(|| ImageryError::Invalid("tile path requires a file extension".into()))?;
    let row = row
        .parse::<u32>()
        .map_err(|_| ImageryError::Invalid("tile row is invalid".into()))?;
    Ok((row, TileFormat::parse(extension)?))
}

fn valid_tile_coordinate(zoom: u8, col: u32, row: u32) -> bool {
    zoom <= 30 && u64::from(col) < (1_u64 << zoom) && u64::from(row) < (1_u64 << zoom)
}

pub(crate) fn validate_tile_bytes(format: TileFormat, bytes: &[u8]) -> Result<(), ImageryError> {
    let valid = match format {
        TileFormat::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        TileFormat::Jpeg => bytes.starts_with(&[0xff, 0xd8]),
        TileFormat::Webp => {
            bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
        }
    };
    valid
        .then_some(())
        .ok_or_else(|| ImageryError::Invalid(format!("body is not a valid {format} tile")))
}

fn imagery_error(error: ImageryError) -> Response {
    let status = match error {
        ImageryError::InvalidLease => StatusCode::UNAUTHORIZED,
        ImageryError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ImageryError::NotFound(_) => StatusCode::NOT_FOUND,
        ImageryError::Conflict(_) => StatusCode::CONFLICT,
        ImageryError::Database(_) | ImageryError::Migration(_) | ImageryError::Json(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (
        status,
        Json(ApiError {
            error: error.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates_xyz_tile_paths() {
        assert_eq!(parse_tile_path("3.png").unwrap(), (3, TileFormat::Png));
        assert!(parse_tile_path("3.exe").is_err());
        assert!(valid_tile_coordinate(3, 7, 7));
        assert!(!valid_tile_coordinate(3, 8, 7));
    }

    #[test]
    fn rejects_content_that_does_not_match_the_extension() {
        assert!(validate_tile_bytes(TileFormat::Png, b"not png").is_err());
        assert!(validate_tile_bytes(TileFormat::Jpeg, &[0xff, 0xd8, 0xff]).is_ok());
    }

    #[test]
    fn repository_storage_path_matches_cangling_tile_sharding() {
        let repository = Repository {
            id: "repository".into(),
            name: "test".into(),
            description: None,
            source_url_template: "https://tiles.example/{z}/{x}/{y}.png".into(),
            min_zoom: 0,
            max_zoom: 19,
            bounds: [-180.0, -85.0, 180.0, 85.0],
            format: TileFormat::Png,
            tile_database: "abcd012345".into(),
            tile_item: "9876fedcba".into(),
            revocable: false,
            version: 1,
            created_at: "2026-11-02 00:00:00".into(),
            updated_at: "2026-11-02 00:00:00".into(),
        };
        assert_eq!(
            repository_storage_path_with_root(&repository, "/app/tiledata"),
            "/app/tiledata/abcd/012345/9876/fedcba"
        );
        assert_eq!(
            repository_browser_url_with_base(&repository, "/cangling-tile"),
            "/cangling-tile/tiles/abcd012345/9876fedcba/{z}/{x}/{y}.png"
        );
    }
}
