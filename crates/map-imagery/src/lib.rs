use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::SqlitePoolOptions, FromRow, QueryBuilder, Row, Sqlite, SqlitePool, Transaction,
};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const WEB_MERCATOR_MAX_LATITUDE: f64 = 85.051_128_779_806_6;
const MAX_JOB_CHUNKS: usize = 250_000;
const TILE_INDEX_PAGE_BITS: u64 = 32_768;
const TILE_INDEX_PAGE_BYTES: usize = (TILE_INDEX_PAGE_BITS / 8) as usize;
const TILE_INDEX_LOCKS: usize = 64;

#[derive(Debug, Error)]
pub enum ImageryError {
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid or expired upload lease")]
    InvalidLease,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileFormat {
    Png,
    Jpeg,
    Webp,
}

impl TileFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ImageryError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "webp" => Ok(Self::Webp),
            _ => Err(ImageryError::Invalid(format!(
                "unsupported imagery tile format: {value}"
            ))),
        }
    }
}

impl std::fmt::Display for TileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobMode {
    Server,
    Browser,
    Hybrid,
}

impl JobMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Browser => "browser",
            Self::Hybrid => "hybrid",
        }
    }

    fn parse(value: &str) -> Result<Self, ImageryError> {
        match value {
            "server" => Ok(Self::Server),
            "browser" => Ok(Self::Browser),
            "hybrid" => Ok(Self::Hybrid),
            _ => Err(ImageryError::Invalid(format!(
                "unknown imagery job mode: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, ImageryError> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(ImageryError::Invalid(format!(
                "unknown imagery job status: {value}"
            ))),
        }
    }

    pub fn accepts_uploads(self) -> bool {
        self == Self::Running
    }

    fn can_transition_to(self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Pending, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Paused | Self::Completed | Self::Failed | Self::Cancelled
                )
                | (Self::Paused, Self::Running | Self::Cancelled)
                | (Self::Failed, Self::Running | Self::Cancelled)
        ) || self == target
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryInput {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source_url_template: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: [f64; 4],
    pub format: TileFormat,
    #[serde(default)]
    pub revocable: bool,
}

impl RepositoryInput {
    pub fn source_url_templates(&self) -> Vec<&str> {
        split_source_url_templates(&self.source_url_template)
    }

    fn validate(&self) -> Result<(), ImageryError> {
        if self.name.trim().is_empty() {
            return Err(ImageryError::Invalid("repository name is required".into()));
        }
        let sources = self.source_url_templates();
        if sources.is_empty() || sources.len() > 16 {
            return Err(ImageryError::Invalid(
                "one to sixteen source URL templates are required".into(),
            ));
        }
        for source in sources {
            if !(source.starts_with("http://") || source.starts_with("https://"))
                || !["{z}", "{x}", "{y}"]
                    .iter()
                    .all(|placeholder| source.contains(placeholder))
            {
                return Err(ImageryError::Invalid(
                    "every source URL template must be http(s) and contain {z}, {x}, {y}".into(),
                ));
            }
        }
        if self.min_zoom > self.max_zoom || self.max_zoom > 30 {
            return Err(ImageryError::Invalid(
                "zoom range must satisfy min_zoom <= max_zoom <= 30".into(),
            ));
        }
        let [min_x, min_y, max_x, max_y] = self.bounds;
        if !self.bounds.iter().all(|value| value.is_finite()) || min_x >= max_x || min_y >= max_y {
            return Err(ImageryError::Invalid(
                "repository bounds are invalid".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source_url_template: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: [f64; 4],
    pub format: TileFormat,
    pub tile_database: String,
    pub tile_item: String,
    pub revocable: bool,
    pub version: u64,
    pub created_at: String,
    pub updated_at: String,
}

impl Repository {
    pub fn source_url_templates(&self) -> Vec<&str> {
        split_source_url_templates(&self.source_url_template)
    }
}

fn split_source_url_templates(value: &str) -> Vec<&str> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInput {
    pub repository_id: String,
    pub mode: JobMode,
    #[serde(default)]
    pub min_zoom: Option<u8>,
    #[serde(default)]
    pub max_zoom: Option<u8>,
    #[serde(default)]
    pub bounds: Option<[f64; 4]>,
    #[serde(default)]
    pub total_tiles: u64,
    #[serde(default)]
    pub leader_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub repository_id: String,
    pub mode: JobMode,
    pub status: JobStatus,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub bounds: [f64; 4],
    pub total_tiles: u64,
    pub completed_tiles: u64,
    pub failed_tiles: u64,
    pub skipped_tiles: u64,
    pub leader_term: u64,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub job_id: String,
    pub zoom: u8,
    pub min_col: u32,
    pub max_col: u32,
    pub min_row: u32,
    pub max_row: u32,
    pub status: String,
    pub assigned_instance: Option<String>,
    pub lease_id: Option<String>,
    pub lease_expires_at: Option<String>,
    pub attempts: u32,
    pub planned_tiles: u64,
    pub completed_tiles: u64,
    pub failed_tiles: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkStatusCount {
    pub status: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    pub job: Job,
    pub chunk_statuses: Vec<ChunkStatusCount>,
    pub latest_chunk_updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImagerySnapshot {
    pub schema_version: u32,
    pub generated_at: String,
    #[serde(default)]
    pub leader_id: Option<String>,
    #[serde(default)]
    pub leader_term: u64,
    pub outbox_watermark: Option<String>,
    pub repositories: Vec<Repository>,
    pub jobs: Vec<Job>,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEventInput {
    pub event_id: String,
    pub event_type: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub leader_term: u64,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEvent {
    pub event_id: String,
    pub event_type: String,
    pub source: Option<String>,
    pub leader_term: u64,
    pub payload: Value,
    pub attempts: u32,
    pub received_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileReceipt {
    pub inserted: bool,
    pub job_id: String,
    pub zoom: u8,
    pub col: u32,
    pub row: u32,
    pub format: TileFormat,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TileCoordinate {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileStoredEvent {
    pub job_id: String,
    pub repository_id: String,
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub format: TileFormat,
    pub size_bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub leader_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobCommandEvent {
    pub job_id: String,
    pub repository_id: String,
    pub mode: JobMode,
    pub leader_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkWorkEvent {
    pub chunk_id: String,
    pub job_id: String,
    pub repository_id: String,
    pub z: u8,
    pub min_x: u32,
    pub max_x: u32,
    pub min_y: u32,
    pub max_y: u32,
    #[serde(default)]
    pub planned_tiles: u64,
    pub leader_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkResultEvent {
    pub chunk_id: String,
    pub job_id: String,
    pub repository_id: String,
    pub completed_tiles: u64,
    pub failed_tiles: u64,
    pub leader_term: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub page_size: u32,
    pub total: u64,
    pub total_pages: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEvent {
    pub id: String,
    pub event_type: String,
    pub aggregate_type: String,
    pub aggregate_id: String,
    pub payload: Value,
    pub attempts: u32,
    pub created_at: String,
}

#[derive(Clone)]
pub struct ImageryStore {
    pool: SqlitePool,
    index_pool: SqlitePool,
    index_locks: Arc<[Mutex<()>; TILE_INDEX_LOCKS]>,
}

impl ImageryStore {
    pub async fn connect(url: &str) -> Result<Self, ImageryError> {
        Self::connect_with_index(url, "sqlite://imagery-index.db?mode=rwc").await
    }

    pub async fn connect_with_index(url: &str, index_url: &str) -> Result<Self, ImageryError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let index_pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(index_url)
            .await?;
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&index_pool)
            .await?;
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&index_pool)
            .await?;
        sqlx::migrate!("./index-migrations")
            .run(&index_pool)
            .await?;
        let store = Self::with_pools(pool, index_pool);
        store.import_legacy_index_once().await?;
        Ok(store)
    }

    pub async fn in_memory() -> Result<Self, ImageryError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let index_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::migrate!("./index-migrations")
            .run(&index_pool)
            .await?;
        let store = Self::with_pools(pool, index_pool);
        store.import_legacy_index_once().await?;
        Ok(store)
    }

    fn with_pools(pool: SqlitePool, index_pool: SqlitePool) -> Self {
        Self {
            pool,
            index_pool,
            index_locks: Arc::new(std::array::from_fn(|_| Mutex::new(()))),
        }
    }

    async fn import_legacy_index_once(&self) -> Result<(), ImageryError> {
        let imported = sqlx::query_scalar::<_, String>(
            "SELECT value FROM imagery_index_metadata WHERE key='legacy-main-db-imported'",
        )
        .fetch_optional(&self.index_pool)
        .await?
        .is_some();
        if imported {
            return Ok(());
        }

        let mut last_repository = String::new();
        let mut last_zoom = -1_i64;
        let mut last_page = -1_i64;
        loop {
            let rows = sqlx::query(
                "SELECT repository_id,zoom,page,bits
                 FROM imagery_tile_index_pages
                 WHERE repository_id>?
                    OR (repository_id=? AND zoom>?)
                    OR (repository_id=? AND zoom=? AND page>?)
                 ORDER BY repository_id,zoom,page
                 LIMIT 256",
            )
            .bind(&last_repository)
            .bind(&last_repository)
            .bind(last_zoom)
            .bind(&last_repository)
            .bind(last_zoom)
            .bind(last_page)
            .fetch_all(&self.pool)
            .await?;
            if rows.is_empty() {
                break;
            }

            let mut tx = self.index_pool.begin().await?;
            for row in &rows {
                let repository_id: String = row.try_get("repository_id")?;
                let zoom: i64 = row.try_get("zoom")?;
                let page: i64 = row.try_get("page")?;
                let bits: Vec<u8> = row.try_get("bits")?;
                if bits.len() != TILE_INDEX_PAGE_BYTES || !(0..=30).contains(&zoom) {
                    return Err(ImageryError::Conflict(format!(
                        "legacy repository tile index page is invalid: {repository_id}/{zoom}/{page}"
                    )));
                }
                let full = bitmap_page_is_complete(&bits, zoom as u8, page);
                sqlx::query(
                    "INSERT INTO imagery_tile_index_pages(repository_id,zoom,page,state,bits)
                     VALUES(?,?,?,?,?)
                     ON CONFLICT(repository_id,zoom,page) DO NOTHING",
                )
                .bind(&repository_id)
                .bind(zoom)
                .bind(page)
                .bind(if full { "full" } else { "partial" })
                .bind((!full).then_some(bits))
                .execute(&mut *tx)
                .await?;
                last_repository = repository_id;
                last_zoom = zoom;
                last_page = page;
            }
            tx.commit().await?;
        }

        // The copied rows are no longer read by the new runtime. Removing
        // them prevents the task database from retaining a second live copy;
        // operators may run VACUUM during maintenance to reclaim old pages.
        sqlx::query("DELETE FROM imagery_tile_index_pages")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO imagery_index_metadata(key,value)
             VALUES('legacy-main-db-imported',CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=CURRENT_TIMESTAMP",
        )
        .execute(&self.index_pool)
        .await?;
        Ok(())
    }

    pub async fn list_repositories(
        &self,
        page: u32,
        page_size: u32,
        query: Option<&str>,
    ) -> Result<Paged<Repository>, ImageryError> {
        let page = page.max(1);
        let page_size = page_size.clamp(5, 200);
        let query = query.map(str::trim).filter(|value| !value.is_empty());
        let pattern = query.map(|value| format!("%{value}%"));
        let total = if let Some(pattern) = &pattern {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM imagery_repositories WHERE name LIKE ? OR description LIKE ?",
            )
            .bind(pattern)
            .bind(pattern)
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM imagery_repositories")
                .fetch_one(&self.pool)
                .await?
        } as u64;
        let offset = u64::from(page.saturating_sub(1)) * u64::from(page_size);
        let rows = if let Some(pattern) = &pattern {
            sqlx::query_as::<_, RepositoryRow>(&format!(
                "{SELECT_REPOSITORY} WHERE name LIKE ? OR description LIKE ? ORDER BY updated_at DESC LIMIT ? OFFSET ?"
            ))
            .bind(pattern)
            .bind(pattern)
            .bind(i64::from(page_size))
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, RepositoryRow>(&format!(
                "{SELECT_REPOSITORY} ORDER BY updated_at DESC LIMIT ? OFFSET ?"
            ))
            .bind(i64::from(page_size))
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(Paged {
            items: rows
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            page,
            page_size,
            total,
            total_pages: total.div_ceil(u64::from(page_size)).max(1),
        })
    }

    pub async fn get_repository(&self, id: &str) -> Result<Option<Repository>, ImageryError> {
        sqlx::query_as::<_, RepositoryRow>(&format!("{SELECT_REPOSITORY} WHERE id=?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }

    pub async fn create_repository(
        &self,
        input: &RepositoryInput,
    ) -> Result<Repository, ImageryError> {
        input.validate()?;
        let id = Uuid::new_v4().to_string();
        // A repository owns its cangling-tile database, so a compact UUID is
        // already collision-safe and the single dataset can use a readable ID.
        // Existing repositories retain their historical SHA-256 identifiers.
        let tile_database = id.replace('-', "");
        let tile_item = "tiles".to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO imagery_repositories(id,name,description,source_url_template,min_zoom,max_zoom,min_x,min_y,max_x,max_y,format,tile_database,tile_item,revocable) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&id)
            .bind(input.name.trim())
            .bind(input.description.as_deref().map(str::trim).filter(|v| !v.is_empty()))
            .bind(input.source_url_template.trim())
            .bind(i64::from(input.min_zoom))
            .bind(i64::from(input.max_zoom))
            .bind(input.bounds[0])
            .bind(input.bounds[1])
            .bind(input.bounds[2])
            .bind(input.bounds[3])
            .bind(input.format.extension())
            .bind(&tile_database)
            .bind(&tile_item)
            .bind(i64::from(input.revocable))
            .execute(&mut *tx)
            .await
            .map_err(map_unique_conflict)?;
        push_outbox(
            &mut tx,
            "imagery.repository.created",
            "repository",
            &id,
            json!({"repository_id": id, "version": 1}),
        )
        .await?;
        tx.commit().await?;
        self.get_repository(&id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(id))
    }

    pub async fn update_repository(
        &self,
        id: &str,
        input: &RepositoryInput,
    ) -> Result<Repository, ImageryError> {
        input.validate()?;
        let existing = self
            .get_repository(id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(id.to_owned()))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE imagery_repositories SET name=?,description=?,source_url_template=?,min_zoom=?,max_zoom=?,min_x=?,min_y=?,max_x=?,max_y=?,format=?,tile_database=?,tile_item=?,revocable=?,version=version+1,updated_at=CURRENT_TIMESTAMP WHERE id=?")
            .bind(input.name.trim())
            .bind(input.description.as_deref().map(str::trim).filter(|v| !v.is_empty()))
            .bind(input.source_url_template.trim())
            .bind(i64::from(input.min_zoom))
            .bind(i64::from(input.max_zoom))
            .bind(input.bounds[0])
            .bind(input.bounds[1])
            .bind(input.bounds[2])
            .bind(input.bounds[3])
            .bind(input.format.extension())
            .bind(&existing.tile_database)
            .bind(&existing.tile_item)
            .bind(i64::from(input.revocable))
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(map_unique_conflict)?;
        push_outbox(
            &mut tx,
            "imagery.repository.updated",
            "repository",
            id,
            json!({"repository_id": id, "version": existing.version + 1}),
        )
        .await?;
        tx.commit().await?;
        self.get_repository(id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(id.to_owned()))
    }

    pub async fn delete_repository(&self, id: &str) -> Result<bool, ImageryError> {
        let jobs =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM imagery_jobs WHERE repository_id=?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        if jobs > 0 {
            return Err(ImageryError::Conflict(
                "repository is referenced by imagery jobs".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query("DELETE FROM imagery_repositories WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        if deleted {
            push_outbox(
                &mut tx,
                "imagery.repository.deleted",
                "repository",
                id,
                json!({"repository_id": id}),
            )
            .await?;
        }
        tx.commit().await?;
        if deleted {
            sqlx::query("DELETE FROM imagery_tile_index_pages WHERE repository_id=?")
                .bind(id)
                .execute(&self.index_pool)
                .await?;
        }
        Ok(deleted)
    }

    pub async fn create_job(&self, input: &JobInput) -> Result<Job, ImageryError> {
        let repository = self
            .get_repository(&input.repository_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(input.repository_id.clone()))?;
        let min_zoom = input.min_zoom.unwrap_or(repository.min_zoom);
        let max_zoom = input.max_zoom.unwrap_or(repository.max_zoom);
        let bounds = input.bounds.unwrap_or(repository.bounds);
        validate_job_scope(&repository, min_zoom, max_zoom, bounds)?;
        let id = Uuid::new_v4().to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO imagery_jobs(id,repository_id,mode,status,min_zoom,max_zoom,min_x,min_y,max_x,max_y,total_tiles,leader_term) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&id)
            .bind(&input.repository_id)
            .bind(input.mode.as_str())
            .bind(JobStatus::Running.as_str())
            .bind(i64::from(min_zoom))
            .bind(i64::from(max_zoom))
            .bind(bounds[0])
            .bind(bounds[1])
            .bind(bounds[2])
            .bind(bounds[3])
            .bind(input.total_tiles as i64)
            .bind(input.leader_term as i64)
            .execute(&mut *tx)
            .await?;
        push_outbox(
            &mut tx,
            "imagery.job.created",
            "job",
            &id,
            json!({"job_id": id, "repository_id": input.repository_id, "mode": input.mode, "min_zoom": min_zoom, "max_zoom": max_zoom, "bounds": bounds, "leader_term": input.leader_term}),
        )
        .await?;
        tx.commit().await?;
        self.get_job(&id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(id))
    }

    pub async fn list_jobs(
        &self,
        page: u32,
        page_size: u32,
        repository_id: Option<&str>,
    ) -> Result<Paged<Job>, ImageryError> {
        let page = page.max(1);
        let page_size = page_size.clamp(5, 200);
        let total = if let Some(repository_id) = repository_id {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM imagery_jobs WHERE repository_id=?")
                .bind(repository_id)
                .fetch_one(&self.pool)
                .await?
        } else {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM imagery_jobs")
                .fetch_one(&self.pool)
                .await?
        } as u64;
        let offset = u64::from(page.saturating_sub(1)) * u64::from(page_size);
        let rows = if let Some(repository_id) = repository_id {
            sqlx::query_as::<_, JobRow>(&format!(
                "{SELECT_JOB} WHERE repository_id=? ORDER BY created_at DESC LIMIT ? OFFSET ?"
            ))
            .bind(repository_id)
            .bind(i64::from(page_size))
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, JobRow>(&format!(
                "{SELECT_JOB} ORDER BY created_at DESC LIMIT ? OFFSET ?"
            ))
            .bind(i64::from(page_size))
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(Paged {
            items: rows
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            page,
            page_size,
            total,
            total_pages: total.div_ceil(u64::from(page_size)).max(1),
        })
    }

    pub async fn get_job(&self, id: &str) -> Result<Option<Job>, ImageryError> {
        sqlx::query_as::<_, JobRow>(&format!("{SELECT_JOB} WHERE id=?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }

    /// Delete task-local state without removing repository-level tile index
    /// bits. Tiles already committed to cangling-tile remain part of the
    /// repository and are skipped by future overlapping jobs.
    pub async fn delete_job(&self, id: &str) -> Result<bool, ImageryError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM imagery_outbox
             WHERE aggregate_type='chunk'
               AND aggregate_id IN (SELECT id FROM imagery_chunks WHERE job_id=?)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM imagery_outbox WHERE aggregate_type='job' AND aggregate_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM imagery_jobs WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(deleted)
    }

    pub async fn list_job_chunks(&self, job_id: &str) -> Result<Vec<Chunk>, ImageryError> {
        sqlx::query_as::<_, ChunkRow>(&format!(
            "{SELECT_CHUNK} WHERE job_id=? ORDER BY zoom,min_row,min_col,id"
        ))
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    pub async fn list_job_chunks_page(
        &self,
        job_id: &str,
        page: u32,
        page_size: u32,
        status: Option<&str>,
    ) -> Result<Paged<Chunk>, ImageryError> {
        if self.get_job(job_id).await?.is_none() {
            return Err(ImageryError::NotFound(job_id.to_owned()));
        }
        let page = page.max(1);
        let page_size = page_size.clamp(5, 500);
        let status = status.map(str::trim).filter(|value| !value.is_empty());
        let total = if let Some(status) = status {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM imagery_chunks WHERE job_id=? AND status=?",
            )
            .bind(job_id)
            .bind(status)
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM imagery_chunks WHERE job_id=?")
                .bind(job_id)
                .fetch_one(&self.pool)
                .await?
        } as u64;
        let offset = i64::from((page - 1).saturating_mul(page_size));
        let rows = if let Some(status) = status {
            sqlx::query_as::<_, ChunkRow>(&format!(
                "{SELECT_CHUNK} WHERE job_id=? AND status=? ORDER BY zoom,min_row,min_col,id LIMIT ? OFFSET ?"
            ))
            .bind(job_id)
            .bind(status)
            .bind(i64::from(page_size))
            .bind(offset)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, ChunkRow>(&format!(
                "{SELECT_CHUNK} WHERE job_id=? ORDER BY zoom,min_row,min_col,id LIMIT ? OFFSET ?"
            ))
            .bind(job_id)
            .bind(i64::from(page_size))
            .bind(offset)
            .fetch_all(&self.pool)
            .await?
        };
        let items = rows
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Paged {
            items,
            page,
            page_size,
            total,
            total_pages: total.div_ceil(u64::from(page_size)),
        })
    }

    pub async fn job_progress(&self, job_id: &str) -> Result<JobProgress, ImageryError> {
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        let chunk_statuses = sqlx::query_as::<_, ChunkStatusCountRow>(
            "SELECT status,COUNT(*) AS count FROM imagery_chunks
             WHERE job_id=? GROUP BY status ORDER BY status",
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| ChunkStatusCount {
            status: row.status,
            count: row.count as u64,
        })
        .collect();
        let latest_chunk_updated_at = sqlx::query_scalar::<_, Option<String>>(
            "SELECT MAX(updated_at) FROM imagery_chunks WHERE job_id=?",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(JobProgress {
            job,
            chunk_statuses,
            latest_chunk_updated_at,
        })
    }

    /// Create deterministic XYZ work ranges for a job exactly once. The
    /// database transaction updates the exact tile total and emits one Outbox
    /// work event per server-executable chunk.
    pub async fn ensure_job_chunks(
        &self,
        job_id: &str,
        max_tiles_per_chunk: u32,
    ) -> Result<Vec<Chunk>, ImageryError> {
        if !(1..=65_536).contains(&max_tiles_per_chunk) {
            return Err(ImageryError::Invalid(
                "max_tiles_per_chunk must be between 1 and 65536".into(),
            ));
        }
        let existing = self.list_job_chunks(job_id).await?;
        if !existing.is_empty() {
            return Ok(existing);
        }
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        let repository = self
            .get_repository(&job.repository_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job.repository_id.clone()))?;
        validate_job_scope(&repository, job.min_zoom, job.max_zoom, job.bounds)?;
        let plans = plan_chunks(&job, max_tiles_per_chunk)?;
        let index_is_empty = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM imagery_tile_index_pages WHERE repository_id=?",
        )
        .bind(&repository.id)
        .fetch_one(&self.index_pool)
        .await?
            == 0;
        let mut missing_plans = Vec::with_capacity(plans.len());
        for plan in plans {
            let missing = if index_is_empty {
                plan.tile_count()
            } else {
                self.count_missing_repository_tiles_in_range(
                    &repository.id,
                    plan.zoom,
                    plan.min_col,
                    plan.max_col,
                    plan.min_row,
                    plan.max_row,
                )
                .await?
            };
            if missing > 0 {
                missing_plans.push((plan, missing));
            }
        }
        let total_tiles = missing_plans
            .iter()
            .try_fold(0_u64, |total, (_, missing)| {
                total.checked_add(*missing).ok_or_else(|| {
                    ImageryError::Invalid("imagery job tile count exceeds u64".into())
                })
            })?;
        let total_tiles = i64::try_from(total_tiles).map_err(|_| {
            ImageryError::Invalid("imagery job tile count exceeds SQLite integer range".into())
        })?;
        // Hybrid jobs begin in browser-first mode. Their pending chunks are
        // published only when the explicit takeover operation is requested.
        let publish_work = job.mode == JobMode::Server;
        let initial_status = if publish_work { "queued" } else { "pending" };
        let initial_attempts = i64::from(publish_work);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE imagery_jobs SET total_tiles=?,status=CASE WHEN ?=0 THEN 'completed' ELSE status END,updated_at=CURRENT_TIMESTAMP WHERE id=?",
        )
        .bind(total_tiles)
        .bind(total_tiles)
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
        for (plan, planned_tiles) in missing_plans {
            // The stable identifier makes a retried command idempotent even
            // when it is redelivered after a process restart.
            let id = sha256_hex(
                format!(
                    "imagery-chunk\0{job_id}\0{}\0{}\0{}\0{}\0{}",
                    plan.zoom, plan.min_col, plan.max_col, plan.min_row, plan.max_row
                )
                .as_bytes(),
            );
            let inserted = sqlx::query("INSERT OR IGNORE INTO imagery_chunks(id,job_id,zoom,min_col,max_col,min_row,max_row,status,attempts,planned_tiles) VALUES(?,?,?,?,?,?,?,?,?,?)")
                .bind(&id)
                .bind(job_id)
                .bind(i64::from(plan.zoom))
                .bind(i64::from(plan.min_col))
                .bind(i64::from(plan.max_col))
                .bind(i64::from(plan.min_row))
                .bind(i64::from(plan.max_row))
                .bind(initial_status)
                .bind(initial_attempts)
                .bind(i64::try_from(planned_tiles).map_err(|_| ImageryError::Invalid("chunk planned tile count exceeds SQLite range".into()))?)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                == 1;
            if publish_work && inserted {
                push_outbox(
                    &mut tx,
                    "imagery.chunk.created",
                    "chunk",
                    &id,
                    serde_json::to_value(ChunkWorkEvent {
                        chunk_id: id.clone(),
                        job_id: job.id.clone(),
                        repository_id: repository.id.clone(),
                        z: plan.zoom,
                        min_x: plan.min_col,
                        max_x: plan.max_col,
                        min_y: plan.min_row,
                        max_y: plan.max_row,
                        planned_tiles,
                        leader_term: job.leader_term,
                    })?,
                )
                .await?;
            }
        }
        tx.commit().await?;
        self.list_job_chunks(job_id).await
    }

    /// Move browser-first hybrid chunks into the durable server work queue.
    /// The status transition and Outbox writes share one transaction, so a
    /// repeated takeover request cannot publish the same pending chunk twice.
    pub async fn takeover_hybrid_job(&self, job_id: &str) -> Result<u64, ImageryError> {
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        if job.mode != JobMode::Hybrid {
            return Err(ImageryError::Conflict(
                "only hybrid imagery jobs can be taken over".into(),
            ));
        }
        if job.status != JobStatus::Running {
            return Err(ImageryError::Conflict(
                "hybrid imagery job is not running".into(),
            ));
        }
        let repository = self
            .get_repository(&job.repository_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job.repository_id.clone()))?;
        let mut tx = self.pool.begin().await?;
        let chunks = sqlx::query_as::<_, ChunkRow>(&format!(
            "{SELECT_CHUNK} WHERE job_id=? AND status='pending' ORDER BY zoom,min_row,min_col,id"
        ))
        .bind(job_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut queued = 0_u64;
        for row in chunks {
            let chunk: Chunk = row.try_into()?;
            let changed = sqlx::query(
                "UPDATE imagery_chunks SET status='queued',attempts=attempts+1,updated_at=CURRENT_TIMESTAMP
                 WHERE id=? AND status='pending'",
            )
            .bind(&chunk.id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
            if !changed {
                continue;
            }
            push_outbox(
                &mut tx,
                "imagery.chunk.created",
                "chunk",
                &chunk.id,
                serde_json::to_value(ChunkWorkEvent {
                    chunk_id: chunk.id.clone(),
                    job_id: job.id.clone(),
                    repository_id: repository.id.clone(),
                    z: chunk.zoom,
                    min_x: chunk.min_col,
                    max_x: chunk.max_col,
                    min_y: chunk.min_row,
                    max_y: chunk.max_row,
                    planned_tiles: chunk.planned_tiles,
                    leader_term: job.leader_term,
                })?,
            )
            .await?;
            queued += 1;
        }
        tx.commit().await?;
        Ok(queued)
    }

    pub async fn complete_browser_chunk(
        &self,
        job_id: &str,
        chunk_id: &str,
    ) -> Result<Chunk, ImageryError> {
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        if !matches!(job.mode, JobMode::Browser | JobMode::Hybrid) {
            return Err(ImageryError::Conflict(
                "only browser and hybrid jobs accept browser chunk completion".into(),
            ));
        }
        if !matches!(job.status, JobStatus::Running | JobStatus::Completed) {
            return Err(ImageryError::Conflict(
                "imagery job cannot complete browser chunks in its current state".into(),
            ));
        }
        let chunk: Chunk =
            sqlx::query_as::<_, ChunkRow>(&format!("{SELECT_CHUNK} WHERE id=? AND job_id=?"))
                .bind(chunk_id)
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?
                .map(TryInto::try_into)
                .transpose()?
                .ok_or_else(|| ImageryError::NotFound(chunk_id.to_owned()))?;
        if chunk.status == "completed" {
            return Ok(chunk);
        }
        if matches!(chunk.status.as_str(), "failed" | "cancelled") {
            return Err(ImageryError::Conflict(format!(
                "chunk cannot transition from {} to completed",
                chunk.status
            )));
        }
        let repository = self
            .get_repository(&job.repository_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job.repository_id))?;
        let range_tiles = (u64::from(chunk.max_col - chunk.min_col) + 1)
            .checked_mul(u64::from(chunk.max_row - chunk.min_row) + 1)
            .ok_or_else(|| ImageryError::Invalid("chunk tile count exceeds u64".into()))?;
        let missing = self
            .count_missing_repository_tiles_in_range(
                &repository.id,
                chunk.zoom,
                chunk.min_col,
                chunk.max_col,
                chunk.min_row,
                chunk.max_row,
            )
            .await?;
        if missing != 0 {
            return Err(ImageryError::Conflict(format!(
                "chunk still has {missing} missing tiles out of {range_tiles} coordinates"
            )));
        }
        sqlx::query(
            "UPDATE imagery_chunks
             SET status='completed',completed_tiles=?,failed_tiles=0,updated_at=CURRENT_TIMESTAMP
             WHERE id=? AND job_id=? AND status NOT IN ('completed','failed','cancelled')",
        )
        .bind(i64::try_from(chunk.planned_tiles).map_err(|_| {
            ImageryError::Invalid("chunk tile count exceeds SQLite integer range".into())
        })?)
        .bind(chunk_id)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE imagery_jobs SET updated_at=CURRENT_TIMESTAMP WHERE id=?")
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        let completed: Chunk =
            sqlx::query_as::<_, ChunkRow>(&format!("{SELECT_CHUNK} WHERE id=? AND job_id=?"))
                .bind(chunk_id)
                .bind(job_id)
                .fetch_one(&self.pool)
                .await?
                .try_into()?;
        if completed.status != "completed" {
            return Err(ImageryError::Conflict(format!(
                "chunk concurrently transitioned to {}",
                completed.status
            )));
        }
        sqlx::query(
            "UPDATE imagery_jobs SET
               completed_tiles=(SELECT COALESCE(SUM(completed_tiles),0) FROM imagery_chunks WHERE job_id=?),
               status=CASE WHEN NOT EXISTS (
                 SELECT 1 FROM imagery_chunks WHERE job_id=? AND status NOT IN ('completed','cancelled')
               ) THEN 'completed' ELSE status END,
               updated_at=CURRENT_TIMESTAMP
             WHERE id=?",
        )
        .bind(job_id)
        .bind(job_id)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        Ok(completed)
    }

    /// Refresh browser activity without creating a durable business event.
    /// The timestamp is used only by the current leader's automatic hybrid
    /// takeover coordinator.
    pub async fn heartbeat_browser_job(&self, job_id: &str) -> Result<Job, ImageryError> {
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        if !matches!(job.mode, JobMode::Browser | JobMode::Hybrid) {
            return Err(ImageryError::Conflict(
                "only browser and hybrid jobs accept browser heartbeats".into(),
            ));
        }
        if job.status != JobStatus::Running {
            return Err(ImageryError::Conflict("imagery job is not running".into()));
        }
        sqlx::query(
            "UPDATE imagery_jobs SET updated_at=CURRENT_TIMESTAMP WHERE id=? AND status='running'",
        )
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        self.get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))
    }

    pub async fn stale_hybrid_job_ids(
        &self,
        idle_seconds: u64,
        limit: u32,
    ) -> Result<Vec<String>, ImageryError> {
        if idle_seconds == 0 {
            return Ok(Vec::new());
        }
        let modifier = format!("-{} seconds", idle_seconds.min(86_400 * 30));
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM imagery_jobs
             WHERE mode='hybrid' AND status='running'
               AND datetime(updated_at) <= datetime('now', ?)
               AND (
                 NOT EXISTS (SELECT 1 FROM imagery_chunks WHERE job_id=imagery_jobs.id)
                 OR EXISTS (
                   SELECT 1 FROM imagery_chunks
                   WHERE job_id=imagery_jobs.id AND status='pending'
                 )
               )
             ORDER BY updated_at,id LIMIT ?",
        )
        .bind(modifier)
        .bind(i64::from(limit.clamp(1, 1_000)))
        .fetch_all(&self.pool)
        .await
        .map_err(ImageryError::from)
    }

    pub async fn transition_job(&self, id: &str, target: JobStatus) -> Result<Job, ImageryError> {
        let existing = self
            .get_job(id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(id.to_owned()))?;
        if !existing.status.can_transition_to(target) {
            return Err(ImageryError::Conflict(format!(
                "job cannot transition from {} to {}",
                existing.status.as_str(),
                target.as_str()
            )));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE imagery_jobs SET status=?,updated_at=CURRENT_TIMESTAMP WHERE id=?")
            .bind(target.as_str())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if target == JobStatus::Cancelled {
            sqlx::query(
                "UPDATE imagery_chunks SET status='cancelled',updated_at=CURRENT_TIMESTAMP
                 WHERE job_id=? AND status NOT IN ('completed','failed','cancelled')",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        push_outbox(
            &mut tx,
            "imagery.job.status_changed",
            "job",
            id,
            json!({"job_id": id, "previous": existing.status, "status": target, "leader_term": existing.leader_term}),
        )
        .await?;
        tx.commit().await?;
        self.get_job(id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(id.to_owned()))
    }

    pub async fn emit_chunk_result(&self, result: &ChunkResultEvent) -> Result<(), ImageryError> {
        let mut tx = self.pool.begin().await?;
        let event_type = if result.error.is_some() {
            "imagery.chunk.failed"
        } else {
            "imagery.chunk.completed"
        };
        push_outbox(
            &mut tx,
            event_type,
            "chunk",
            &result.chunk_id,
            serde_json::to_value(result)?,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn apply_chunk_result(
        &self,
        result: &ChunkResultEvent,
    ) -> Result<bool, ImageryError> {
        let job = self
            .get_job(&result.job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(result.job_id.clone()))?;
        if job.repository_id != result.repository_id || job.leader_term != result.leader_term {
            return Err(ImageryError::Conflict(
                "chunk result does not match its canonical job".into(),
            ));
        }
        let status = if result.error.is_some() {
            "failed"
        } else {
            "completed"
        };
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE imagery_chunks
             SET status=?,completed_tiles=?,failed_tiles=?,updated_at=CURRENT_TIMESTAMP
             WHERE id=? AND job_id=? AND status NOT IN ('completed','failed','cancelled')",
        )
        .bind(status)
        .bind(i64::try_from(result.completed_tiles).map_err(|_| {
            ImageryError::Invalid("chunk completed tile count exceeds SQLite range".into())
        })?)
        .bind(i64::try_from(result.failed_tiles).map_err(|_| {
            ImageryError::Invalid("chunk failed tile count exceeds SQLite range".into())
        })?)
        .bind(&result.chunk_id)
        .bind(&result.job_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if changed {
            let remaining = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM imagery_chunks
                 WHERE job_id=? AND status NOT IN ('completed','failed','cancelled')",
            )
            .bind(&result.job_id)
            .fetch_one(&mut *tx)
            .await?;
            if remaining == 0 {
                let failed_chunks = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM imagery_chunks WHERE job_id=? AND status='failed'",
                )
                .bind(&result.job_id)
                .fetch_one(&mut *tx)
                .await?;
                let terminal = if failed_chunks > 0 {
                    "failed"
                } else {
                    "completed"
                };
                sqlx::query("UPDATE imagery_jobs SET status=?,error=?,updated_at=CURRENT_TIMESTAMP WHERE id=?")
                    .bind(terminal)
                    .bind(result.error.as_deref())
                    .bind(&result.job_id)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "UPDATE imagery_jobs SET
                   completed_tiles=(SELECT COALESCE(SUM(completed_tiles),0) FROM imagery_chunks WHERE job_id=?),
                   failed_tiles=(SELECT COALESCE(SUM(failed_tiles),0) FROM imagery_chunks WHERE job_id=?),
                   updated_at=CURRENT_TIMESTAMP WHERE id=?",
            )
            .bind(&result.job_id)
            .bind(&result.job_id)
            .bind(&result.job_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    pub async fn repository_tile_exists(
        &self,
        repository_id: &str,
        zoom: u8,
        col: u32,
        row: u32,
    ) -> Result<bool, ImageryError> {
        let (page, bit) = tile_index_address(zoom, col, row)?;
        let row = sqlx::query(
            "SELECT state,bits FROM imagery_tile_index_pages
             WHERE repository_id=? AND zoom=? AND page=?",
        )
        .bind(repository_id)
        .bind(i64::from(zoom))
        .bind(page)
        .fetch_optional(&self.index_pool)
        .await?;
        match row {
            Some(row) if row.try_get::<String, _>("state")? == "full" => Ok(true),
            Some(row) => {
                let bits: Option<Vec<u8>> = row.try_get("bits")?;
                Ok(bits.is_some_and(|bits| bitmap_get(&bits, bit)))
            }
            None => Ok(false),
        }
    }

    pub async fn missing_tiles_for_chunk(
        &self,
        job_id: &str,
        chunk_id: &str,
    ) -> Result<Vec<TileCoordinate>, ImageryError> {
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        let chunk: Chunk =
            sqlx::query_as::<_, ChunkRow>(&format!("{SELECT_CHUNK} WHERE id=? AND job_id=?"))
                .bind(chunk_id)
                .bind(job_id)
                .fetch_optional(&self.pool)
                .await?
                .map(TryInto::try_into)
                .transpose()?
                .ok_or_else(|| ImageryError::NotFound(chunk_id.to_owned()))?;
        self.missing_tiles_in_range(
            &job.repository_id,
            chunk.zoom,
            chunk.min_col,
            chunk.max_col,
            chunk.min_row,
            chunk.max_row,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn count_missing_repository_tiles_in_range(
        &self,
        repository_id: &str,
        zoom: u8,
        min_col: u32,
        max_col: u32,
        min_row: u32,
        max_row: u32,
    ) -> Result<u64, ImageryError> {
        Ok(self
            .missing_tiles_in_range(repository_id, zoom, min_col, max_col, min_row, max_row)
            .await?
            .len() as u64)
    }

    #[allow(clippy::too_many_arguments)]
    async fn missing_tiles_in_range(
        &self,
        repository_id: &str,
        zoom: u8,
        min_col: u32,
        max_col: u32,
        min_row: u32,
        max_row: u32,
    ) -> Result<Vec<TileCoordinate>, ImageryError> {
        let mut addresses = Vec::new();
        let mut page_ids = HashSet::new();
        for y in min_row..=max_row {
            for x in min_col..=max_col {
                let (page, bit) = tile_index_address(zoom, x, y)?;
                page_ids.insert(page);
                addresses.push((x, y, page, bit));
            }
        }
        let page_ids: Vec<i64> = page_ids.into_iter().collect();
        let mut pages = HashMap::<i64, IndexPage>::new();
        for batch in page_ids.chunks(400) {
            let mut query = QueryBuilder::<Sqlite>::new(
                "SELECT page,state,bits FROM imagery_tile_index_pages WHERE repository_id=",
            );
            query
                .push_bind(repository_id)
                .push(" AND zoom=")
                .push_bind(i64::from(zoom))
                .push(" AND page IN (");
            let mut separated = query.separated(",");
            for page in batch {
                separated.push_bind(page);
            }
            separated.push_unseparated(")");
            for row in query.build().fetch_all(&self.index_pool).await? {
                let state: String = row.try_get("state")?;
                let page = if state == "full" {
                    IndexPage::Full
                } else {
                    let bits: Vec<u8> = row.try_get("bits")?;
                    if bits.len() != TILE_INDEX_PAGE_BYTES {
                        return Err(ImageryError::Conflict(format!(
                            "repository tile index page has invalid size: {}",
                            bits.len()
                        )));
                    }
                    IndexPage::Partial(bits)
                };
                pages.insert(row.try_get("page")?, page);
            }
        }
        Ok(addresses
            .into_iter()
            .filter_map(|(x, y, page, bit)| {
                (!pages.get(&page).is_some_and(|page| page.contains(bit)))
                    .then_some(TileCoordinate { z: zoom, x, y })
            })
            .collect())
    }

    async fn set_repository_tile_index(
        &self,
        repository_id: &str,
        zoom: u8,
        col: u32,
        row: u32,
    ) -> Result<(), ImageryError> {
        let (page, bit) = tile_index_address(zoom, col, row)?;
        let mut tx = self.index_pool.begin().await?;
        // Take SQLite's writer lock before reading the page. The in-process
        // striped mutex avoids local contention; this write-first transaction
        // also prevents lost bits if multiple processes share the index DB.
        sqlx::query(
            "INSERT OR IGNORE INTO imagery_tile_index_pages(repository_id,zoom,page,state,bits)
             VALUES(?,?,?,'partial',?)",
        )
        .bind(repository_id)
        .bind(i64::from(zoom))
        .bind(page)
        .bind(vec![0_u8; TILE_INDEX_PAGE_BYTES])
        .execute(&mut *tx)
        .await?;
        let existing = sqlx::query(
            "SELECT state,bits FROM imagery_tile_index_pages
             WHERE repository_id=? AND zoom=? AND page=?",
        )
        .bind(repository_id)
        .bind(i64::from(zoom))
        .bind(page)
        .fetch_one(&mut *tx)
        .await?;
        if existing.get::<String, _>("state") == "full" {
            tx.commit().await?;
            return Ok(());
        }
        let mut bits = existing
            .try_get::<Option<Vec<u8>>, _>("bits")?
            .unwrap_or_else(|| vec![0_u8; TILE_INDEX_PAGE_BYTES]);
        if bits.len() != TILE_INDEX_PAGE_BYTES {
            return Err(ImageryError::Conflict(format!(
                "repository tile index page has invalid size: {}",
                bits.len()
            )));
        }
        let byte = usize::from(bit / 8);
        bits[byte] |= 1_u8 << (bit % 8);
        let full = bitmap_page_is_complete(&bits, zoom, page);
        sqlx::query(
            "UPDATE imagery_tile_index_pages
             SET state=?,bits=?,updated_at=CURRENT_TIMESTAMP
             WHERE repository_id=? AND zoom=? AND page=?",
        )
        .bind(if full { "full" } else { "partial" })
        .bind((!full).then_some(bits))
        .bind(repository_id)
        .bind(i64::from(zoom))
        .bind(page)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_tile_stored(
        &self,
        job_id: &str,
        zoom: u8,
        col: u32,
        row: u32,
        format: TileFormat,
        bytes: &[u8],
    ) -> Result<TileReceipt, ImageryError> {
        let digest = sha256_hex(bytes);
        self.record_tile_receipt(
            job_id,
            zoom,
            col,
            row,
            format,
            bytes.len() as u64,
            &digest,
            true,
        )
        .await
    }

    /// Apply a durable tile-stored result received from Broker. This updates
    /// canonical progress without producing another Outbox event, which would
    /// otherwise create an event loop. Coordinate + format uniqueness and the
    /// digest make redelivery safe across ACK loss and leader failover.
    pub async fn apply_tile_stored_event(
        &self,
        event: &TileStoredEvent,
    ) -> Result<TileReceipt, ImageryError> {
        let sha256 = event.sha256.trim();
        if event.size_bytes == 0
            || event.size_bytes > i64::MAX as u64
            || sha256.len() != 64
            || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ImageryError::Invalid(
                "tile event requires a non-zero size and a SHA-256 hex digest".into(),
            ));
        }
        let job = self
            .get_job(&event.job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(event.job_id.clone()))?;
        if job.repository_id != event.repository_id {
            return Err(ImageryError::Conflict(format!(
                "tile event repository {} does not match job repository {}",
                event.repository_id, job.repository_id
            )));
        }
        self.record_tile_receipt(
            &event.job_id,
            event.z,
            event.x,
            event.y,
            event.format,
            event.size_bytes,
            sha256,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_tile_receipt(
        &self,
        job_id: &str,
        zoom: u8,
        col: u32,
        row: u32,
        format: TileFormat,
        size_bytes: u64,
        digest: &str,
        emit_outbox: bool,
    ) -> Result<TileReceipt, ImageryError> {
        if let Some(existing) = self
            .get_tile_receipt(job_id, zoom, col, row, format)
            .await?
        {
            if existing.sha256 == digest && existing.size_bytes == size_bytes {
                return Ok(existing);
            }
            return Err(ImageryError::Conflict(
                "tile coordinate was already recorded with different content".into(),
            ));
        }
        let job = self
            .get_job(job_id)
            .await?
            .ok_or_else(|| ImageryError::NotFound(job_id.to_owned()))?;
        if !job_contains_tile(&job, zoom, col, row) {
            return Err(ImageryError::Invalid(
                "tile coordinate is outside the job scope".into(),
            ));
        }
        let accepts_late_broker_result =
            !emit_outbox && matches!(job.status, JobStatus::Completed | JobStatus::Failed);
        if !job.status.accepts_uploads() && !accepts_late_broker_result {
            return Err(ImageryError::Conflict(
                "job is not accepting tile uploads".into(),
            ));
        }
        let (index_page, _) = tile_index_address(zoom, col, row)?;
        let lock = tile_index_lock(&job.repository_id, zoom, index_page);
        let _index_guard = self.index_locks[lock].lock().await;
        self.set_repository_tile_index(&job.repository_id, zoom, col, row)
            .await?;
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO imagery_tile_receipts(job_id,zoom,col,row,format,size_bytes,sha256) VALUES(?,?,?,?,?,?,?)")
            .bind(job_id)
            .bind(i64::from(zoom))
            .bind(i64::from(col))
            .bind(i64::from(row))
            .bind(format.extension())
            .bind(size_bytes as i64)
            .bind(digest)
            .execute(&mut *tx)
            .await?
            .rows_affected() == 1;
        let mut receipt_size = size_bytes;
        let mut receipt_digest = digest.to_owned();
        if inserted && job.status.accepts_uploads() {
            sqlx::query("UPDATE imagery_jobs SET completed_tiles=completed_tiles+1,status=CASE WHEN total_tiles>0 AND completed_tiles+1>=total_tiles THEN 'completed' ELSE status END,updated_at=CURRENT_TIMESTAMP WHERE id=?")
                .bind(job_id)
                .execute(&mut *tx)
                .await?;
        }
        if inserted && emit_outbox {
            push_outbox(
                &mut tx,
                "imagery.tile.stored",
                "job",
                job_id,
                json!({"job_id": job_id, "repository_id": job.repository_id, "z": zoom, "x": col, "y": row, "format": format, "size_bytes": size_bytes, "sha256": digest, "leader_term": job.leader_term}),
            )
            .await?;
        } else {
            let stored = sqlx::query_as::<_, TileReceiptRow>(
                "SELECT size_bytes,sha256 FROM imagery_tile_receipts WHERE job_id=? AND zoom=? AND col=? AND row=? AND format=?",
            )
            .bind(job_id)
            .bind(i64::from(zoom))
            .bind(i64::from(col))
            .bind(i64::from(row))
            .bind(format.extension())
            .fetch_one(&mut *tx)
            .await?;
            if stored.sha256 != digest || stored.size_bytes != size_bytes as i64 {
                return Err(ImageryError::Conflict(
                    "tile coordinate was concurrently recorded with different content".into(),
                ));
            }
            receipt_size = stored.size_bytes as u64;
            receipt_digest = stored.sha256;
        }
        tx.commit().await?;
        Ok(TileReceipt {
            inserted,
            job_id: job_id.to_owned(),
            zoom,
            col,
            row,
            format,
            size_bytes: receipt_size,
            sha256: receipt_digest,
        })
    }

    pub async fn get_tile_receipt(
        &self,
        job_id: &str,
        zoom: u8,
        col: u32,
        row: u32,
        format: TileFormat,
    ) -> Result<Option<TileReceipt>, ImageryError> {
        let stored = sqlx::query_as::<_, TileReceiptRow>(
            "SELECT size_bytes,sha256 FROM imagery_tile_receipts WHERE job_id=? AND zoom=? AND col=? AND row=? AND format=?",
        )
        .bind(job_id)
        .bind(i64::from(zoom))
        .bind(i64::from(col))
        .bind(i64::from(row))
        .bind(format.extension())
        .fetch_optional(&self.pool)
        .await?;
        Ok(stored.map(|stored| TileReceipt {
            inserted: false,
            job_id: job_id.to_owned(),
            zoom,
            col,
            row,
            format,
            size_bytes: stored.size_bytes as u64,
            sha256: stored.sha256,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn count_tile_receipts_in_range(
        &self,
        job_id: &str,
        zoom: u8,
        min_col: u32,
        max_col: u32,
        min_row: u32,
        max_row: u32,
        format: TileFormat,
    ) -> Result<u64, ImageryError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM imagery_tile_receipts
             WHERE job_id=? AND zoom=? AND col BETWEEN ? AND ?
             AND row BETWEEN ? AND ? AND format=?",
        )
        .bind(job_id)
        .bind(i64::from(zoom))
        .bind(i64::from(min_col))
        .bind(i64::from(max_col))
        .bind(i64::from(min_row))
        .bind(i64::from(max_row))
        .bind(format.extension())
        .fetch_one(&self.pool)
        .await? as u64)
    }

    pub async fn pending_outbox(&self, limit: u32) -> Result<Vec<OutboxEvent>, ImageryError> {
        sqlx::query_as::<_, OutboxRow>(
            "SELECT id,event_type,aggregate_type,aggregate_id,payload,attempts,created_at FROM imagery_outbox WHERE status='pending' AND (next_attempt_at IS NULL OR next_attempt_at<=CURRENT_TIMESTAMP) ORDER BY created_at,id LIMIT ?",
        )
        .bind(i64::from(limit.clamp(1, 1000)))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    pub async fn has_unpublished_outbox(
        &self,
        aggregate_type: &str,
        aggregate_id: &str,
    ) -> Result<bool, ImageryError> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM imagery_outbox
             WHERE status='pending' AND aggregate_type=? AND aggregate_id=?",
        )
        .bind(aggregate_type)
        .bind(aggregate_id)
        .fetch_one(&self.pool)
        .await?
            > 0)
    }

    pub async fn mark_outbox_published(&self, id: &str) -> Result<bool, ImageryError> {
        Ok(sqlx::query("UPDATE imagery_outbox SET status='published',published_at=CURRENT_TIMESTAMP WHERE id=? AND status='pending'")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected() == 1)
    }

    pub async fn mark_outbox_failed(&self, id: &str) -> Result<bool, ImageryError> {
        Ok(sqlx::query("UPDATE imagery_outbox SET attempts=attempts+1,next_attempt_at=datetime(CURRENT_TIMESTAMP,'+5 seconds') WHERE id=? AND status='pending'")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected() == 1)
    }

    /// Persist an incoming Broker event exactly once. The durable pending row
    /// is deliberately separate from applying it, so a crash leaves work that
    /// can be retried instead of losing the event.
    pub async fn record_inbox_event(&self, event: &InboxEventInput) -> Result<bool, ImageryError> {
        let event_id = event.event_id.trim();
        let event_type = event.event_type.trim();
        if event_id.is_empty() || event_type.is_empty() {
            return Err(ImageryError::Invalid(
                "inbox event_id and event_type are required".into(),
            ));
        }
        Ok(sqlx::query(
            "INSERT OR IGNORE INTO imagery_inbox(event_id,event_type,source,leader_term,payload) VALUES(?,?,?,?,?)",
        )
        .bind(event_id)
        .bind(event_type)
        .bind(event.source.as_deref().map(str::trim).filter(|value| !value.is_empty()))
        .bind(event.leader_term as i64)
        .bind(serde_json::to_string(&event.payload)?)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn pending_inbox(&self, limit: u32) -> Result<Vec<InboxEvent>, ImageryError> {
        sqlx::query_as::<_, InboxRow>(
            "SELECT event_id,event_type,source,leader_term,payload,attempts,processed_at FROM imagery_inbox WHERE status='pending' AND (next_attempt_at IS NULL OR next_attempt_at<=CURRENT_TIMESTAMP) ORDER BY processed_at,event_id LIMIT ?",
        )
        .bind(i64::from(limit.clamp(1, 1000)))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    pub async fn mark_inbox_applied(&self, event_id: &str) -> Result<bool, ImageryError> {
        Ok(sqlx::query("UPDATE imagery_inbox SET status='applied',applied_at=CURRENT_TIMESTAMP,last_error=NULL WHERE event_id=? AND status='pending'")
            .bind(event_id)
            .execute(&self.pool)
            .await?
            .rows_affected() == 1)
    }

    pub async fn mark_inbox_failed(
        &self,
        event_id: &str,
        error: &str,
    ) -> Result<bool, ImageryError> {
        Ok(sqlx::query("UPDATE imagery_inbox SET attempts=attempts+1,last_error=?,next_attempt_at=datetime(CURRENT_TIMESTAMP,'+5 seconds') WHERE event_id=? AND status='pending'")
            .bind(error)
            .bind(event_id)
            .execute(&self.pool)
            .await?
            .rows_affected() == 1)
    }

    /// Export the metadata required to bootstrap a read-only follower. Tile
    /// binaries remain authoritative in cangling-tile and are not duplicated
    /// in this snapshot.
    pub async fn export_snapshot(&self) -> Result<ImagerySnapshot, ImageryError> {
        let mut tx = self.pool.begin().await?;
        let repositories = sqlx::query_as::<_, RepositoryRow>(&format!(
            "{SELECT_REPOSITORY} ORDER BY created_at,id"
        ))
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect::<Result<Vec<_>, _>>()?;
        let jobs = sqlx::query_as::<_, JobRow>(&format!("{SELECT_JOB} ORDER BY created_at,id"))
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, _>>()?;
        let chunks =
            sqlx::query_as::<_, ChunkRow>(&format!("{SELECT_CHUNK} ORDER BY created_at,id"))
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<Vec<_>, _>>()?;
        let generated_at = sqlx::query_scalar::<_, String>("SELECT CURRENT_TIMESTAMP")
            .fetch_one(&mut *tx)
            .await?;
        let outbox_watermark = sqlx::query_scalar::<_, String>(
            "SELECT id FROM imagery_outbox ORDER BY created_at DESC,rowid DESC LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ImagerySnapshot {
            schema_version: 2,
            generated_at,
            leader_id: None,
            leader_term: 0,
            outbox_watermark,
            repositories,
            jobs,
            chunks,
        })
    }

    /// Merge a leader snapshot into a follower database. Tile receipts and
    /// local Inbox/Outbox rows are intentionally retained: cangling-tile owns
    /// binary data, while Broker redelivery reconciles completion receipts.
    pub async fn apply_snapshot(&self, snapshot: &ImagerySnapshot) -> Result<(), ImageryError> {
        if snapshot.schema_version != 2 {
            return Err(ImageryError::Invalid(format!(
                "unsupported imagery snapshot schema: {}",
                snapshot.schema_version
            )));
        }
        let mut tx = self.pool.begin().await?;
        for repository in &snapshot.repositories {
            sqlx::query("INSERT INTO imagery_repositories(id,name,description,source_url_template,min_zoom,max_zoom,min_x,min_y,max_x,max_y,format,tile_database,tile_item,revocable,version,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,description=excluded.description,source_url_template=excluded.source_url_template,min_zoom=excluded.min_zoom,max_zoom=excluded.max_zoom,min_x=excluded.min_x,min_y=excluded.min_y,max_x=excluded.max_x,max_y=excluded.max_y,format=excluded.format,tile_database=excluded.tile_database,tile_item=excluded.tile_item,revocable=excluded.revocable,version=excluded.version,created_at=excluded.created_at,updated_at=excluded.updated_at")
                .bind(&repository.id)
                .bind(&repository.name)
                .bind(&repository.description)
                .bind(&repository.source_url_template)
                .bind(i64::from(repository.min_zoom))
                .bind(i64::from(repository.max_zoom))
                .bind(repository.bounds[0])
                .bind(repository.bounds[1])
                .bind(repository.bounds[2])
                .bind(repository.bounds[3])
                .bind(repository.format.extension())
                .bind(&repository.tile_database)
                .bind(&repository.tile_item)
                .bind(i64::from(repository.revocable))
                .bind(repository.version as i64)
                .bind(&repository.created_at)
                .bind(&repository.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        for job in &snapshot.jobs {
            sqlx::query("INSERT INTO imagery_jobs(id,repository_id,mode,status,min_zoom,max_zoom,min_x,min_y,max_x,max_y,total_tiles,completed_tiles,failed_tiles,skipped_tiles,leader_term,error,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET repository_id=excluded.repository_id,mode=excluded.mode,status=excluded.status,min_zoom=excluded.min_zoom,max_zoom=excluded.max_zoom,min_x=excluded.min_x,min_y=excluded.min_y,max_x=excluded.max_x,max_y=excluded.max_y,total_tiles=excluded.total_tiles,completed_tiles=excluded.completed_tiles,failed_tiles=excluded.failed_tiles,skipped_tiles=excluded.skipped_tiles,leader_term=excluded.leader_term,error=excluded.error,created_at=excluded.created_at,updated_at=excluded.updated_at")
                .bind(&job.id)
                .bind(&job.repository_id)
                .bind(job.mode.as_str())
                .bind(job.status.as_str())
                .bind(i64::from(job.min_zoom))
                .bind(i64::from(job.max_zoom))
                .bind(job.bounds[0])
                .bind(job.bounds[1])
                .bind(job.bounds[2])
                .bind(job.bounds[3])
                .bind(job.total_tiles as i64)
                .bind(job.completed_tiles as i64)
                .bind(job.failed_tiles as i64)
                .bind(job.skipped_tiles as i64)
                .bind(job.leader_term as i64)
                .bind(&job.error)
                .bind(&job.created_at)
                .bind(&job.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        for chunk in &snapshot.chunks {
            sqlx::query("INSERT INTO imagery_chunks(id,job_id,zoom,min_col,max_col,min_row,max_row,status,assigned_instance,lease_id,lease_expires_at,attempts,planned_tiles,completed_tiles,failed_tiles,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET job_id=excluded.job_id,zoom=excluded.zoom,min_col=excluded.min_col,max_col=excluded.max_col,min_row=excluded.min_row,max_row=excluded.max_row,status=excluded.status,assigned_instance=excluded.assigned_instance,lease_id=excluded.lease_id,lease_expires_at=excluded.lease_expires_at,attempts=excluded.attempts,planned_tiles=excluded.planned_tiles,completed_tiles=excluded.completed_tiles,failed_tiles=excluded.failed_tiles,created_at=excluded.created_at,updated_at=excluded.updated_at")
                .bind(&chunk.id)
                .bind(&chunk.job_id)
                .bind(i64::from(chunk.zoom))
                .bind(i64::from(chunk.min_col))
                .bind(i64::from(chunk.max_col))
                .bind(i64::from(chunk.min_row))
                .bind(i64::from(chunk.max_row))
                .bind(&chunk.status)
                .bind(&chunk.assigned_instance)
                .bind(&chunk.lease_id)
                .bind(&chunk.lease_expires_at)
                .bind(i64::from(chunk.attempts))
                .bind(chunk.planned_tiles as i64)
                .bind(chunk.completed_tiles as i64)
                .bind(chunk.failed_tiles as i64)
                .bind(&chunk.created_at)
                .bind(&chunk.updated_at)
                .execute(&mut *tx)
                .await?;
        }
        for (key, value) in [
            ("leader_id", snapshot.leader_id.clone().unwrap_or_default()),
            ("leader_term", snapshot.leader_term.to_string()),
            ("generated_at", snapshot.generated_at.clone()),
            (
                "outbox_watermark",
                snapshot.outbox_watermark.clone().unwrap_or_default(),
            ),
        ] {
            sqlx::query("INSERT INTO imagery_replication_state(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=CURRENT_TIMESTAMP")
                .bind(key)
                .bind(value)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadLease {
    pub token: String,
    pub job_id: String,
    pub repository_id: String,
    pub formats: Vec<TileFormat>,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeaseClaims {
    version: u8,
    job_id: String,
    repository_id: String,
    formats: Vec<TileFormat>,
    expires_at: u64,
}

#[derive(Clone)]
pub struct LeaseSigner {
    key: Vec<u8>,
}

impl LeaseSigner {
    pub fn new(secret: impl AsRef<[u8]>) -> Result<Self, ImageryError> {
        let secret = secret.as_ref();
        if secret.len() < 24 {
            return Err(ImageryError::Invalid(
                "IMAGERY_LEASE_SECRET must contain at least 24 bytes".into(),
            ));
        }
        Ok(Self {
            key: Sha256::digest(secret).to_vec(),
        })
    }

    pub fn issue(
        &self,
        job: &Job,
        repository: &Repository,
        ttl_seconds: u64,
    ) -> Result<UploadLease, ImageryError> {
        if !job.status.accepts_uploads() || job.repository_id != repository.id {
            return Err(ImageryError::Conflict(
                "job is not eligible for an upload lease".into(),
            ));
        }
        let expires_at = unix_seconds().saturating_add(ttl_seconds.clamp(30, 3600));
        let claims = LeaseClaims {
            version: 1,
            job_id: job.id.clone(),
            repository_id: repository.id.clone(),
            formats: vec![repository.format],
            expires_at,
        };
        let payload = serde_json::to_vec(&claims)?;
        let payload_encoded = URL_SAFE_NO_PAD.encode(payload);
        let mut mac =
            HmacSha256::new_from_slice(&self.key).map_err(|_| ImageryError::InvalidLease)?;
        mac.update(payload_encoded.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        Ok(UploadLease {
            token: format!("{payload_encoded}.{signature}"),
            job_id: claims.job_id,
            repository_id: claims.repository_id,
            formats: claims.formats,
            expires_at,
        })
    }

    pub fn verify(&self, token: &str) -> Result<UploadLease, ImageryError> {
        let (payload, signature) = token.split_once('.').ok_or(ImageryError::InvalidLease)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| ImageryError::InvalidLease)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.key).map_err(|_| ImageryError::InvalidLease)?;
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| ImageryError::InvalidLease)?;
        let claims: LeaseClaims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| ImageryError::InvalidLease)?,
        )?;
        if claims.version != 1 || claims.expires_at <= unix_seconds() {
            return Err(ImageryError::InvalidLease);
        }
        Ok(UploadLease {
            token: token.to_owned(),
            job_id: claims.job_id,
            repository_id: claims.repository_id,
            formats: claims.formats,
            expires_at: claims.expires_at,
        })
    }
}

const SELECT_REPOSITORY: &str = "SELECT id,name,description,source_url_template,min_zoom,max_zoom,min_x,min_y,max_x,max_y,format,tile_database,tile_item,revocable,version,created_at,updated_at FROM imagery_repositories";
const SELECT_JOB: &str = "SELECT id,repository_id,mode,status,min_zoom,max_zoom,min_x,min_y,max_x,max_y,total_tiles,completed_tiles,failed_tiles,skipped_tiles,leader_term,error,created_at,updated_at FROM imagery_jobs";
const SELECT_CHUNK: &str = "SELECT id,job_id,zoom,min_col,max_col,min_row,max_row,status,assigned_instance,lease_id,lease_expires_at,attempts,planned_tiles,completed_tiles,failed_tiles,created_at,updated_at FROM imagery_chunks";

#[derive(FromRow)]
struct RepositoryRow {
    id: String,
    name: String,
    description: Option<String>,
    source_url_template: String,
    min_zoom: i64,
    max_zoom: i64,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    format: String,
    tile_database: String,
    tile_item: String,
    revocable: i64,
    version: i64,
    created_at: String,
    updated_at: String,
}

impl TryFrom<RepositoryRow> for Repository {
    type Error = ImageryError;

    fn try_from(row: RepositoryRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            name: row.name,
            description: row.description,
            source_url_template: row.source_url_template,
            min_zoom: row.min_zoom as u8,
            max_zoom: row.max_zoom as u8,
            bounds: [row.min_x, row.min_y, row.max_x, row.max_y],
            format: TileFormat::parse(&row.format)?,
            tile_database: row.tile_database,
            tile_item: row.tile_item,
            revocable: row.revocable != 0,
            version: row.version as u64,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(FromRow)]
struct JobRow {
    id: String,
    repository_id: String,
    mode: String,
    status: String,
    min_zoom: i64,
    max_zoom: i64,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    total_tiles: i64,
    completed_tiles: i64,
    failed_tiles: i64,
    skipped_tiles: i64,
    leader_term: i64,
    error: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(FromRow)]
struct ChunkRow {
    id: String,
    job_id: String,
    zoom: i64,
    min_col: i64,
    max_col: i64,
    min_row: i64,
    max_row: i64,
    status: String,
    assigned_instance: Option<String>,
    lease_id: Option<String>,
    lease_expires_at: Option<String>,
    attempts: i64,
    planned_tiles: i64,
    completed_tiles: i64,
    failed_tiles: i64,
    created_at: String,
    updated_at: String,
}

#[derive(FromRow)]
struct ChunkStatusCountRow {
    status: String,
    count: i64,
}

#[derive(FromRow)]
struct TileReceiptRow {
    size_bytes: i64,
    sha256: String,
}

#[derive(FromRow)]
struct OutboxRow {
    id: String,
    event_type: String,
    aggregate_type: String,
    aggregate_id: String,
    payload: String,
    attempts: i64,
    created_at: String,
}

#[derive(FromRow)]
struct InboxRow {
    event_id: String,
    event_type: String,
    source: Option<String>,
    leader_term: i64,
    payload: String,
    attempts: i64,
    processed_at: String,
}

impl TryFrom<OutboxRow> for OutboxEvent {
    type Error = ImageryError;

    fn try_from(row: OutboxRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            event_type: row.event_type,
            aggregate_type: row.aggregate_type,
            aggregate_id: row.aggregate_id,
            payload: serde_json::from_str(&row.payload)?,
            attempts: row.attempts as u32,
            created_at: row.created_at,
        })
    }
}

impl TryFrom<InboxRow> for InboxEvent {
    type Error = ImageryError;

    fn try_from(row: InboxRow) -> Result<Self, Self::Error> {
        Ok(Self {
            event_id: row.event_id,
            event_type: row.event_type,
            source: row.source,
            leader_term: row.leader_term as u64,
            payload: serde_json::from_str(&row.payload)?,
            attempts: row.attempts as u32,
            received_at: row.processed_at,
        })
    }
}

impl TryFrom<JobRow> for Job {
    type Error = ImageryError;

    fn try_from(row: JobRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            repository_id: row.repository_id,
            mode: JobMode::parse(&row.mode)?,
            status: JobStatus::parse(&row.status)?,
            min_zoom: row.min_zoom as u8,
            max_zoom: row.max_zoom as u8,
            bounds: [row.min_x, row.min_y, row.max_x, row.max_y],
            total_tiles: row.total_tiles as u64,
            completed_tiles: row.completed_tiles as u64,
            failed_tiles: row.failed_tiles as u64,
            skipped_tiles: row.skipped_tiles as u64,
            leader_term: row.leader_term as u64,
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

impl TryFrom<ChunkRow> for Chunk {
    type Error = ImageryError;

    fn try_from(row: ChunkRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            job_id: row.job_id,
            zoom: row.zoom as u8,
            min_col: row.min_col as u32,
            max_col: row.max_col as u32,
            min_row: row.min_row as u32,
            max_row: row.max_row as u32,
            status: row.status,
            assigned_instance: row.assigned_instance,
            lease_id: row.lease_id,
            lease_expires_at: row.lease_expires_at,
            attempts: row.attempts as u32,
            planned_tiles: row.planned_tiles as u64,
            completed_tiles: row.completed_tiles as u64,
            failed_tiles: row.failed_tiles as u64,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

async fn push_outbox(
    tx: &mut Transaction<'_, Sqlite>,
    event_type: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    payload: Value,
) -> Result<(), ImageryError> {
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO imagery_outbox(id,event_type,aggregate_type,aggregate_id,payload) VALUES(?,?,?,?,?)")
        .bind(id)
        .bind(event_type)
        .bind(aggregate_type)
        .bind(aggregate_id)
        .bind(serde_json::to_string(&payload)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn tile_index_address(zoom: u8, col: u32, row: u32) -> Result<(i64, u16), ImageryError> {
    if zoom > 30 || u64::from(col) >= (1_u64 << zoom) || u64::from(row) >= (1_u64 << zoom) {
        return Err(ImageryError::Invalid(
            "tile coordinate is outside the XYZ matrix".into(),
        ));
    }
    let width = 1_u64 << zoom;
    let tile_id = u64::from(row)
        .checked_mul(width)
        .and_then(|value| value.checked_add(u64::from(col)))
        .ok_or_else(|| ImageryError::Invalid("tile index exceeds u64".into()))?;
    Ok((
        (tile_id / TILE_INDEX_PAGE_BITS) as i64,
        (tile_id % TILE_INDEX_PAGE_BITS) as u16,
    ))
}

fn bitmap_get(bits: &[u8], bit: u16) -> bool {
    let byte = usize::from(bit / 8);
    let mask = 1_u8 << (bit % 8);
    bits.get(byte).is_some_and(|value| value & mask != 0)
}

fn bitmap_page_is_complete(bits: &[u8], zoom: u8, page: i64) -> bool {
    if bits.len() != TILE_INDEX_PAGE_BYTES || page < 0 || zoom > 30 {
        return false;
    }
    let total_tiles = 1_u64 << (u32::from(zoom) * 2);
    let page_start = page as u64 * TILE_INDEX_PAGE_BITS;
    let valid_bits = total_tiles
        .saturating_sub(page_start)
        .min(TILE_INDEX_PAGE_BITS) as usize;
    if valid_bits == 0 {
        return false;
    }
    let whole_bytes = valid_bits / 8;
    if bits[..whole_bytes].iter().any(|byte| *byte != u8::MAX) {
        return false;
    }
    let remaining = valid_bits % 8;
    remaining == 0 || bits[whole_bytes] & ((1_u8 << remaining) - 1) == (1_u8 << remaining) - 1
}

enum IndexPage {
    Full,
    Partial(Vec<u8>),
}

impl IndexPage {
    fn contains(&self, bit: u16) -> bool {
        match self {
            Self::Full => true,
            Self::Partial(bits) => bitmap_get(bits, bit),
        }
    }
}

fn tile_index_lock(repository_id: &str, zoom: u8, page: i64) -> usize {
    let repository_hash = repository_id.bytes().fold(0_usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(usize::from(byte))
    });
    (repository_hash ^ usize::from(zoom) ^ page as usize) % TILE_INDEX_LOCKS
}

fn map_unique_conflict(error: sqlx::Error) -> ImageryError {
    if error
        .as_database_error()
        .is_some_and(|error| error.is_unique_violation())
    {
        ImageryError::Conflict("repository name already exists".into())
    } else {
        ImageryError::Database(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkPlan {
    zoom: u8,
    min_col: u32,
    max_col: u32,
    min_row: u32,
    max_row: u32,
}

impl ChunkPlan {
    fn tile_count(self) -> u64 {
        let columns = u64::from(self.max_col - self.min_col) + 1;
        let rows = u64::from(self.max_row - self.min_row) + 1;
        columns * rows
    }
}

fn validate_job_scope(
    repository: &Repository,
    min_zoom: u8,
    max_zoom: u8,
    bounds: [f64; 4],
) -> Result<(), ImageryError> {
    if min_zoom > max_zoom || min_zoom < repository.min_zoom || max_zoom > repository.max_zoom {
        return Err(ImageryError::Invalid(format!(
            "job zoom range must be inside repository Z{}-Z{}",
            repository.min_zoom, repository.max_zoom
        )));
    }
    let [west, south, east, north] = bounds;
    if !bounds.iter().all(|value| value.is_finite())
        || west >= east
        || south >= north
        || west < repository.bounds[0]
        || south < repository.bounds[1]
        || east > repository.bounds[2]
        || north > repository.bounds[3]
    {
        return Err(ImageryError::Invalid(
            "job bounds must be valid and contained by repository bounds".into(),
        ));
    }
    Ok(())
}

pub fn job_contains_tile(job: &Job, zoom: u8, col: u32, row: u32) -> bool {
    if zoom < job.min_zoom || zoom > job.max_zoom {
        return false;
    }
    let [west, south, east, north] = job.bounds;
    col >= longitude_to_tile_x(west, zoom)
        && col <= longitude_to_tile_x(east, zoom)
        && row >= latitude_to_tile_y(north, zoom)
        && row <= latitude_to_tile_y(south, zoom)
}

fn plan_chunks(job: &Job, max_tiles_per_chunk: u32) -> Result<Vec<ChunkPlan>, ImageryError> {
    if max_tiles_per_chunk == 0 {
        return Err(ImageryError::Invalid(
            "max_tiles_per_chunk must be greater than zero".into(),
        ));
    }
    let [west, south, east, north] = job.bounds;
    let mut chunks = Vec::new();
    for zoom in job.min_zoom..=job.max_zoom {
        let min_col = longitude_to_tile_x(west, zoom);
        let max_col = longitude_to_tile_x(east, zoom);
        let min_row = latitude_to_tile_y(north, zoom);
        let max_row = latitude_to_tile_y(south, zoom);
        let total_columns = max_col - min_col + 1;
        let columns_per_chunk = total_columns.min(max_tiles_per_chunk);
        let rows_per_chunk = (max_tiles_per_chunk / columns_per_chunk).max(1);
        let mut row = min_row;
        while row <= max_row {
            let chunk_rows = rows_per_chunk.min(max_row - row + 1);
            let mut col = min_col;
            while col <= max_col {
                let remaining_columns = max_col - col + 1;
                let chunk_columns = remaining_columns.min(columns_per_chunk);
                chunks.push(ChunkPlan {
                    zoom,
                    min_col: col,
                    max_col: col + chunk_columns - 1,
                    min_row: row,
                    max_row: row + chunk_rows - 1,
                });
                if chunks.len() > MAX_JOB_CHUNKS {
                    return Err(ImageryError::Invalid(format!(
                        "imagery job requires more than {MAX_JOB_CHUNKS} chunks; reduce bounds or zoom range, or increase chunk size"
                    )));
                }
                col = col.saturating_add(chunk_columns);
            }
            row = row.saturating_add(chunk_rows);
        }
    }
    Ok(chunks)
}

fn longitude_to_tile_x(longitude: f64, zoom: u8) -> u32 {
    let tile_count = 1_u64 << zoom;
    let normalized = ((longitude.clamp(-180.0, 180.0) + 180.0) / 360.0).clamp(0.0, 1.0);
    ((normalized * tile_count as f64).floor() as u64).min(tile_count - 1) as u32
}

fn latitude_to_tile_y(latitude: f64, zoom: u8) -> u32 {
    let tile_count = 1_u64 << zoom;
    let latitude = latitude.clamp(-WEB_MERCATOR_MAX_LATITUDE, WEB_MERCATOR_MAX_LATITUDE);
    let radians = latitude.to_radians();
    let normalized = (1.0 - radians.tan().asinh() / std::f64::consts::PI) / 2.0;
    ((normalized.clamp(0.0, 1.0) * tile_count as f64).floor() as u64).min(tile_count - 1) as u32
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository_input(name: &str) -> RepositoryInput {
        RepositoryInput {
            name: name.into(),
            description: Some("test imagery".into()),
            source_url_template: "https://tiles.example/{z}/{x}/{y}.png".into(),
            min_zoom: 0,
            max_zoom: 12,
            bounds: [-180.0, -85.0, 180.0, 85.0],
            format: TileFormat::Png,
            revocable: false,
        }
    }

    #[test]
    fn repository_accepts_multiple_source_url_templates() {
        let mut input = repository_input("多源影像");
        input.source_url_template = [
            "https://mt0.example/{z}/{x}/{y}.png",
            "https://mt1.example/{z}/{x}/{y}.png",
        ]
        .join("\n");
        input.validate().unwrap();
        assert_eq!(
            input.source_url_templates(),
            vec![
                "https://mt0.example/{z}/{x}/{y}.png",
                "https://mt1.example/{z}/{x}/{y}.png"
            ]
        );

        input
            .source_url_template
            .push_str("\nftp://invalid/{z}/{x}/{y}.png");
        assert!(input.validate().is_err());
    }

    #[tokio::test]
    async fn repository_job_and_idempotent_receipt_are_transactional() {
        let store = ImageryStore::in_memory().await.unwrap();
        let repository = store
            .create_repository(&repository_input("全国影像"))
            .await
            .unwrap();
        assert_eq!(repository.tile_database.len(), 32);
        assert_eq!(repository.tile_item, "tiles");
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Browser,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 1,
                leader_term: 7,
            })
            .await
            .unwrap();
        let first = store
            .record_tile_stored(&job.id, 1, 1, 1, TileFormat::Png, b"tile")
            .await
            .unwrap();
        let duplicate = store
            .record_tile_stored(&job.id, 1, 1, 1, TileFormat::Png, b"tile")
            .await
            .unwrap();
        assert!(first.inserted);
        assert!(!duplicate.inserted);
        let completed = store.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(completed.status, JobStatus::Completed);
        assert_eq!(completed.completed_tiles, 1);
        let outbox = store.pending_outbox(100).await.unwrap();
        assert_eq!(outbox.len(), 3);
        assert!(store.mark_outbox_published(&outbox[0].id).await.unwrap());
    }

    #[tokio::test]
    async fn repository_with_jobs_cannot_be_deleted() {
        let store = ImageryStore::in_memory().await.unwrap();
        let repository = store
            .create_repository(&repository_input("protected"))
            .await
            .unwrap();
        store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 1,
            })
            .await
            .unwrap();
        assert!(matches!(
            store.delete_repository(&repository.id).await,
            Err(ImageryError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn broker_tile_event_is_idempotent_and_does_not_republish() {
        let store = ImageryStore::in_memory().await.unwrap();
        let repository = store
            .create_repository(&repository_input("broker-result"))
            .await
            .unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 2,
                leader_term: 9,
            })
            .await
            .unwrap();
        let digest = sha256_hex(b"remote-tile");
        let event = TileStoredEvent {
            job_id: job.id.clone(),
            repository_id: job.repository_id.clone(),
            z: 3,
            x: 4,
            y: 5,
            format: TileFormat::Png,
            size_bytes: 11,
            sha256: digest,
            leader_term: 9,
        };
        let first = store.apply_tile_stored_event(&event).await.unwrap();
        let duplicate = store.apply_tile_stored_event(&event).await.unwrap();
        assert!(first.inserted);
        assert!(!duplicate.inserted);
        assert_eq!(
            store
                .get_job(&job.id)
                .await
                .unwrap()
                .unwrap()
                .completed_tiles,
            1
        );
        let outbox = store.pending_outbox(100).await.unwrap();
        assert_eq!(
            outbox.len(),
            2,
            "only repository and job events are emitted"
        );
    }

    #[tokio::test]
    async fn inbox_is_durable_and_idempotent_until_applied() {
        let store = ImageryStore::in_memory().await.unwrap();
        let event = InboxEventInput {
            event_id: "event-1".into(),
            event_type: "imagery.repository.updated".into(),
            source: Some("map-0".into()),
            leader_term: 9,
            payload: json!({"repository_id": "repository-1"}),
        };
        assert!(store.record_inbox_event(&event).await.unwrap());
        assert!(!store.record_inbox_event(&event).await.unwrap());
        let pending = store.pending_inbox(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].leader_term, 9);
        assert_eq!(pending[0].payload["repository_id"], "repository-1");
        assert!(store.mark_inbox_applied("event-1").await.unwrap());
        assert!(store.pending_inbox(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn snapshot_contains_metadata_and_outbox_watermark() {
        let store = ImageryStore::in_memory().await.unwrap();
        let repository = store
            .create_repository(&repository_input("snapshot"))
            .await
            .unwrap();
        store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Hybrid,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 42,
                leader_term: 3,
            })
            .await
            .unwrap();
        let snapshot = store.export_snapshot().await.unwrap();
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(snapshot.repositories.len(), 1);
        assert_eq!(snapshot.jobs.len(), 1);
        assert_eq!(snapshot.jobs[0].leader_term, 3);
        assert!(snapshot.outbox_watermark.is_some());
    }

    #[tokio::test]
    async fn follower_snapshot_import_restores_canonical_metadata() {
        let leader = ImageryStore::in_memory().await.unwrap();
        let repository = leader
            .create_repository(&repository_input("leader-snapshot"))
            .await
            .unwrap();
        let job = leader
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Hybrid,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 2,
                leader_term: 13,
            })
            .await
            .unwrap();
        leader
            .record_tile_stored(&job.id, 2, 1, 1, TileFormat::Png, b"tile")
            .await
            .unwrap();
        let mut snapshot = leader.export_snapshot().await.unwrap();
        snapshot.leader_id = Some("cangling-map-0".into());
        snapshot.leader_term = 13;

        let follower = ImageryStore::in_memory().await.unwrap();
        follower.apply_snapshot(&snapshot).await.unwrap();
        let copied_repository = follower
            .get_repository(&repository.id)
            .await
            .unwrap()
            .unwrap();
        let copied_job = follower.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(copied_repository.name, repository.name);
        assert_eq!(copied_job.completed_tiles, 1);
        assert_eq!(copied_job.leader_term, 13);
        assert!(follower
            .get_tile_receipt(&job.id, 2, 1, 1, TileFormat::Png)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn world_at_zoom_zero_is_exactly_one_tile() {
        let mut input = repository_input("world-z0");
        input.min_zoom = 0;
        input.max_zoom = 0;
        let repository = Repository {
            id: "repository".into(),
            name: input.name,
            description: input.description,
            source_url_template: input.source_url_template,
            min_zoom: input.min_zoom,
            max_zoom: input.max_zoom,
            bounds: input.bounds,
            format: input.format,
            tile_database: String::new(),
            tile_item: String::new(),
            revocable: false,
            version: 1,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let job = Job {
            id: "job".into(),
            repository_id: repository.id.clone(),
            mode: JobMode::Browser,
            min_zoom: repository.min_zoom,
            max_zoom: repository.max_zoom,
            bounds: repository.bounds,
            status: JobStatus::Pending,
            total_tiles: 0,
            completed_tiles: 0,
            failed_tiles: 0,
            skipped_tiles: 0,
            leader_term: 1,
            error: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let plans = plan_chunks(&job, 256).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].tile_count(), 1);
        assert_eq!((plans[0].min_col, plans[0].min_row), (0, 0));
    }

    #[tokio::test]
    async fn global_repository_accepts_a_small_independent_job_scope() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("global-empty-repository");
        input.max_zoom = 19;
        let repository = store.create_repository(&input).await.unwrap();
        let requested_bounds = [112.0, 32.0, 114.0, 34.0];
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Browser,
                min_zoom: Some(7),
                max_zoom: Some(8),
                bounds: Some(requested_bounds),
                total_tiles: 0,
                leader_term: 1,
            })
            .await
            .unwrap();

        assert_eq!(job.min_zoom, 7);
        assert_eq!(job.max_zoom, 8);
        assert_eq!(job.bounds, requested_bounds);
        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks
            .iter()
            .all(|chunk| chunk.zoom == 7 || chunk.zoom == 8));
        assert!(store.get_job(&job.id).await.unwrap().unwrap().total_tiles < 100);
    }

    #[tokio::test]
    async fn job_scope_must_remain_inside_repository_scope() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("regional-repository");
        input.min_zoom = 5;
        input.max_zoom = 10;
        input.bounds = [100.0, 20.0, 120.0, 40.0];
        let repository = store.create_repository(&input).await.unwrap();

        let result = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Server,
                min_zoom: Some(4),
                max_zoom: Some(8),
                bounds: Some([99.0, 21.0, 110.0, 30.0]),
                total_tiles: 0,
                leader_term: 1,
            })
            .await;
        assert!(matches!(result, Err(ImageryError::Invalid(_))));
    }

    #[tokio::test]
    async fn overlapping_job_skips_tiles_already_present_in_repository_bitmap() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("repository-bitmap");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let first = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Browser,
                min_zoom: Some(0),
                max_zoom: Some(0),
                bounds: Some(repository.bounds),
                total_tiles: 0,
                leader_term: 1,
            })
            .await
            .unwrap();
        let first_chunks = store.ensure_job_chunks(&first.id, 16).await.unwrap();
        assert_eq!(first_chunks.len(), 1);
        assert_eq!(first_chunks[0].planned_tiles, 1);
        store
            .record_tile_stored(&first.id, 0, 0, 0, TileFormat::Png, b"tile")
            .await
            .unwrap();
        assert!(store
            .repository_tile_exists(&repository.id, 0, 0, 0)
            .await
            .unwrap());
        let index_row = sqlx::query(
            "SELECT state,bits FROM imagery_tile_index_pages
             WHERE repository_id=? AND zoom=0 AND page=0",
        )
        .bind(&repository.id)
        .fetch_one(&store.index_pool)
        .await
        .unwrap();
        assert_eq!(index_row.get::<String, _>("state"), "full");
        assert!(index_row.get::<Option<Vec<u8>>, _>("bits").is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM imagery_tile_index_pages WHERE repository_id=?",
            )
            .bind(&repository.id)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            0
        );

        let second = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Browser,
                min_zoom: Some(0),
                max_zoom: Some(0),
                bounds: Some(repository.bounds),
                total_tiles: 0,
                leader_term: 2,
            })
            .await
            .unwrap();
        assert!(store
            .ensure_job_chunks(&second.id, 16)
            .await
            .unwrap()
            .is_empty());
        let second = store.get_job(&second.id).await.unwrap().unwrap();
        assert_eq!(second.total_tiles, 0);
        assert_eq!(second.status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn legacy_bitmap_is_migrated_and_complete_pages_are_compacted() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("legacy-index");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let mut bits = vec![0_u8; TILE_INDEX_PAGE_BYTES];
        bits[0] = 1;
        sqlx::query(
            "INSERT INTO imagery_tile_index_pages(repository_id,zoom,page,bits)
             VALUES(?,0,0,?)",
        )
        .bind(&repository.id)
        .bind(bits)
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM imagery_index_metadata WHERE key='legacy-main-db-imported'")
            .execute(&store.index_pool)
            .await
            .unwrap();

        store.import_legacy_index_once().await.unwrap();

        let row = sqlx::query(
            "SELECT state,bits FROM imagery_tile_index_pages
             WHERE repository_id=? AND zoom=0 AND page=0",
        )
        .bind(&repository.id)
        .fetch_one(&store.index_pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("state"), "full");
        assert!(row.get::<Option<Vec<u8>>, _>("bits").is_none());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM imagery_tile_index_pages")
                .fetch_one(&store.pool)
                .await
                .unwrap(),
            0
        );
        assert!(store
            .repository_tile_exists(&repository.id, 0, 0, 0)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn deleting_job_removes_task_state_but_keeps_repository_bitmap() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("deletable-job");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: Some(0),
                max_zoom: Some(0),
                bounds: Some(repository.bounds),
                total_tiles: 1,
                leader_term: 3,
            })
            .await
            .unwrap();
        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();
        store
            .record_tile_stored(&job.id, 0, 0, 0, TileFormat::Png, b"tile")
            .await
            .unwrap();

        assert!(store.delete_job(&job.id).await.unwrap());
        assert!(store.get_job(&job.id).await.unwrap().is_none());
        assert!(store.list_job_chunks(&job.id).await.unwrap().is_empty());
        assert!(store
            .get_tile_receipt(&job.id, 0, 0, 0, TileFormat::Png)
            .await
            .unwrap()
            .is_none());
        assert!(!store.has_unpublished_outbox("job", &job.id).await.unwrap());
        for chunk in chunks {
            assert!(!store
                .has_unpublished_outbox("chunk", &chunk.id)
                .await
                .unwrap());
        }
        assert!(store
            .repository_tile_exists(&repository.id, 0, 0, 0)
            .await
            .unwrap());
        assert!(store
            .get_repository(&repository.id)
            .await
            .unwrap()
            .is_some());
        assert!(!store.delete_job(&job.id).await.unwrap());
        assert!(store.delete_repository(&repository.id).await.unwrap());
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM imagery_tile_index_pages WHERE repository_id=?",
            )
            .bind(&repository.id)
            .fetch_one(&store.index_pool)
            .await
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn concurrent_tile_receipts_do_not_lose_bits_from_the_same_index_page() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("concurrent-bitmap");
        input.max_zoom = 1;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: Some(1),
                max_zoom: Some(1),
                bounds: Some(repository.bounds),
                total_tiles: 4,
                leader_term: 1,
            })
            .await
            .unwrap();
        let first_store = store.clone();
        let first_job = job.id.clone();
        let first = tokio::spawn(async move {
            first_store
                .record_tile_stored(&first_job, 1, 0, 0, TileFormat::Png, b"first")
                .await
        });
        let second_store = store.clone();
        let second_job = job.id;
        let second = tokio::spawn(async move {
            second_store
                .record_tile_stored(&second_job, 1, 1, 0, TileFormat::Png, b"second")
                .await
        });
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert!(store
            .repository_tile_exists(&repository.id, 1, 0, 0)
            .await
            .unwrap());
        assert!(store
            .repository_tile_exists(&repository.id, 1, 1, 0)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn server_job_chunk_creation_is_bounded_and_idempotent() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("chunked-server-job");
        input.max_zoom = 3;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 17,
            })
            .await
            .unwrap();

        let first = store.ensure_job_chunks(&job.id, 5).await.unwrap();
        assert!(!first.is_empty());
        assert!(first.iter().all(|chunk| {
            let columns = u64::from(chunk.max_col - chunk.min_col) + 1;
            let rows = u64::from(chunk.max_row - chunk.min_row) + 1;
            columns * rows <= 5
        }));
        let outbox_after_first = store.pending_outbox(1_000).await.unwrap();
        let second = store.ensure_job_chunks(&job.id, 5).await.unwrap();
        let outbox_after_second = store.pending_outbox(1_000).await.unwrap();
        assert_eq!(first.len(), second.len());
        assert_eq!(outbox_after_first.len(), outbox_after_second.len());
        assert_eq!(
            outbox_after_first
                .iter()
                .filter(|event| event.event_type == "imagery.chunk.created")
                .count(),
            first.len()
        );
        let updated = store.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(updated.total_tiles, 85);
        let page = store
            .list_job_chunks_page(&job.id, 1, 5, Some("queued"))
            .await
            .unwrap();
        assert_eq!(page.items.len(), 5);
        assert_eq!(page.total, first.len() as u64);
        assert_eq!(page.total_pages, (first.len() as u64).div_ceil(5));
        assert!(page.items.iter().all(|chunk| chunk.status == "queued"));
    }

    #[tokio::test]
    async fn browser_job_plans_chunks_without_server_work_events() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("chunked-browser-job");
        input.max_zoom = 1;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Browser,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 1,
            })
            .await
            .unwrap();
        assert!(!store
            .ensure_job_chunks(&job.id, 2)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .pending_outbox(100)
            .await
            .unwrap()
            .iter()
            .all(|event| event.event_type != "imagery.chunk.created"));
    }

    #[tokio::test]
    async fn browser_chunk_completion_requires_every_tile_receipt_and_is_idempotent() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("browser-chunk-receipt");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Browser,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 24,
            })
            .await
            .unwrap();
        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();
        assert!(matches!(
            store.complete_browser_chunk(&job.id, &chunks[0].id).await,
            Err(ImageryError::Conflict(_))
        ));

        store
            .record_tile_stored(&job.id, 0, 0, 0, TileFormat::Png, b"tile")
            .await
            .unwrap();
        let completed = store
            .complete_browser_chunk(&job.id, &chunks[0].id)
            .await
            .unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.completed_tiles, 1);
        assert_eq!(
            store
                .complete_browser_chunk(&job.id, &chunks[0].id)
                .await
                .unwrap()
                .status,
            "completed"
        );
    }

    #[tokio::test]
    async fn hybrid_job_waits_for_idempotent_server_takeover() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("hybrid-takeover");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Hybrid,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 21,
            })
            .await
            .unwrap();

        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].status, "pending");
        assert_eq!(chunks[0].attempts, 0);
        assert!(store
            .pending_outbox(100)
            .await
            .unwrap()
            .iter()
            .all(|event| event.event_type != "imagery.chunk.created"));

        assert_eq!(store.takeover_hybrid_job(&job.id).await.unwrap(), 1);
        let queued = store.list_job_chunks(&job.id).await.unwrap();
        assert_eq!(queued[0].status, "queued");
        assert_eq!(queued[0].attempts, 1);
        let outbox_after_first = store.pending_outbox(100).await.unwrap();
        assert_eq!(
            outbox_after_first
                .iter()
                .filter(|event| event.event_type == "imagery.chunk.created")
                .count(),
            1
        );

        assert_eq!(store.takeover_hybrid_job(&job.id).await.unwrap(), 0);
        assert_eq!(
            store.pending_outbox(100).await.unwrap().len(),
            outbox_after_first.len()
        );
        sqlx::query("UPDATE imagery_jobs SET updated_at=datetime('now','-30 seconds') WHERE id=?")
            .bind(&job.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store
            .stale_hybrid_job_ids(10, 100)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn browser_heartbeat_defers_stale_hybrid_takeover() {
        let store = ImageryStore::in_memory().await.unwrap();
        let repository = store
            .create_repository(&repository_input("hybrid-heartbeat"))
            .await
            .unwrap();
        let hybrid = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Hybrid,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 23,
            })
            .await
            .unwrap();
        sqlx::query("UPDATE imagery_jobs SET updated_at=datetime('now','-30 seconds') WHERE id=?")
            .bind(&hybrid.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.stale_hybrid_job_ids(10, 100).await.unwrap(),
            vec![hybrid.id.clone()]
        );

        store.heartbeat_browser_job(&hybrid.id).await.unwrap();
        assert!(store
            .stale_hybrid_job_ids(10, 100)
            .await
            .unwrap()
            .is_empty());

        let server = store
            .create_job(&JobInput {
                repository_id: repository.id,
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 23,
            })
            .await
            .unwrap();
        assert!(matches!(
            store.heartbeat_browser_job(&server.id).await,
            Err(ImageryError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn cancelled_job_cancels_chunks_and_rejects_late_results() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("cancelled-chunks");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 22,
            })
            .await
            .unwrap();
        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();

        store
            .transition_job(&job.id, JobStatus::Cancelled)
            .await
            .unwrap();
        assert_eq!(
            store.list_job_chunks(&job.id).await.unwrap()[0].status,
            "cancelled"
        );
        let result = ChunkResultEvent {
            chunk_id: chunks[0].id.clone(),
            job_id: job.id.clone(),
            repository_id: repository.id.clone(),
            completed_tiles: 1,
            failed_tiles: 0,
            leader_term: 22,
            error: None,
        };
        assert!(!store.apply_chunk_result(&result).await.unwrap());
        assert_eq!(
            store.get_job(&job.id).await.unwrap().unwrap().status,
            JobStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn chunk_completion_is_applied_once() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("chunk-result");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 8,
            })
            .await
            .unwrap();
        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();
        let result = ChunkResultEvent {
            chunk_id: chunks[0].id.clone(),
            job_id: job.id.clone(),
            repository_id: repository.id.clone(),
            completed_tiles: 1,
            failed_tiles: 0,
            leader_term: 8,
            error: None,
        };
        assert!(store.apply_chunk_result(&result).await.unwrap());
        assert!(!store.apply_chunk_result(&result).await.unwrap());
        assert_eq!(
            store.list_job_chunks(&job.id).await.unwrap()[0].status,
            "completed"
        );
        let progress = store.job_progress(&job.id).await.unwrap();
        // A successful terminal chunk result completes the job even if its
        // per-tile Broker events arrive later. Late events still populate the
        // repository bitmap without double-counting job progress.
        assert_eq!(progress.job.status, JobStatus::Completed);
        let late_tile = TileStoredEvent {
            job_id: job.id.clone(),
            repository_id: repository.id.clone(),
            z: 0,
            x: 0,
            y: 0,
            format: TileFormat::Png,
            size_bytes: 4,
            sha256: sha256_hex(b"tile"),
            leader_term: 8,
        };
        assert!(
            store
                .apply_tile_stored_event(&late_tile)
                .await
                .unwrap()
                .inserted
        );
        assert!(store
            .repository_tile_exists(&repository.id, 0, 0, 0)
            .await
            .unwrap());
        assert_eq!(
            store
                .get_job(&job.id)
                .await
                .unwrap()
                .unwrap()
                .completed_tiles,
            1
        );
        assert_eq!(
            progress.chunk_statuses,
            vec![ChunkStatusCount {
                status: "completed".into(),
                count: 1,
            }]
        );
        assert!(progress.latest_chunk_updated_at.is_some());
    }

    #[tokio::test]
    async fn terminal_chunk_failure_is_applied_once() {
        let store = ImageryStore::in_memory().await.unwrap();
        let mut input = repository_input("chunk-failure");
        input.max_zoom = 0;
        let repository = store.create_repository(&input).await.unwrap();
        let job = store
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 9,
            })
            .await
            .unwrap();
        let chunks = store.ensure_job_chunks(&job.id, 16).await.unwrap();
        let result = ChunkResultEvent {
            chunk_id: chunks[0].id.clone(),
            job_id: job.id.clone(),
            repository_id: repository.id,
            completed_tiles: 0,
            failed_tiles: 1,
            leader_term: 9,
            error: Some("source timeout".into()),
        };
        assert!(store.apply_chunk_result(&result).await.unwrap());
        assert!(!store.apply_chunk_result(&result).await.unwrap());
        assert_eq!(
            store.list_job_chunks(&job.id).await.unwrap()[0].status,
            "failed"
        );
        let updated = store.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(updated.failed_tiles, 1);
        assert_eq!(updated.error.as_deref(), Some("source timeout"));
        assert_eq!(updated.status, JobStatus::Failed);
    }

    #[test]
    fn signed_leases_detect_tampering() {
        let signer = LeaseSigner::new("a sufficiently long shared lease secret").unwrap();
        let repository = Repository {
            id: "repository".into(),
            name: "repository".into(),
            description: None,
            source_url_template: "https://example/{z}/{x}/{y}.png".into(),
            min_zoom: 0,
            max_zoom: 1,
            bounds: [-1.0, -1.0, 1.0, 1.0],
            format: TileFormat::Png,
            tile_database: "a".repeat(64),
            tile_item: "b".repeat(64),
            revocable: false,
            version: 1,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let job = Job {
            id: "job".into(),
            repository_id: repository.id.clone(),
            mode: JobMode::Browser,
            min_zoom: repository.min_zoom,
            max_zoom: repository.max_zoom,
            bounds: repository.bounds,
            status: JobStatus::Running,
            total_tiles: 0,
            completed_tiles: 0,
            failed_tiles: 0,
            skipped_tiles: 0,
            leader_term: 1,
            error: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let lease = signer.issue(&job, &repository, 60).unwrap();
        assert_eq!(signer.verify(&lease.token).unwrap().job_id, "job");
        assert!(signer.verify(&(lease.token + "x")).is_err());
    }
}
