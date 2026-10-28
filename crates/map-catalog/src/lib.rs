use async_trait::async_trait;
use map_core::{BoundingBox, Crs};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};
use thiserror::Error;
use uuid::Uuid;

mod terrain;
pub use terrain::{DemAsset, DemDataset};
mod placenames;
pub use placenames::{PlaceName, SqlitePlaceNames};
mod administration;
pub use administration::{AdminBoundary, AdminLabel, SqliteAdmin};

const CATALOG_ID_DOMAIN: &[u8] = b"cangling-map/catalog-item/v1\0";
const PUBLISHED_LAYER_ID_DOMAIN: &[u8] = b"cangling-map/published-layer/v1\0";
const LAYER_GROUP_ID_DOMAIN: &[u8] = b"cangling-map/layer-group/v1\0";
const STYLE_ID_DOMAIN: &[u8] = b"cangling-map/style/v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogItemId(Uuid);

impl CatalogItemId {
    pub fn derive(app_id: &str, path: &str) -> Result<Self, CatalogError> {
        let app_id = app_id.trim();
        let path = normalize_path(path)?;
        if app_id.is_empty() {
            return Err(CatalogError::InvalidData("app_id is empty".into()));
        }
        let mut hash = Sha256::new();
        hash.update(CATALOG_ID_DOMAIN);
        for value in [app_id.as_bytes(), path.as_bytes()] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value);
        }
        let digest = hash.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(Uuid::from_bytes(bytes)))
    }
}

impl std::fmt::Display for CatalogItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::str::FromStr for CatalogItemId {
    type Err = CatalogError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = Uuid::parse_str(value)
            .map_err(|e| CatalogError::InvalidData(format!("invalid catalog id: {e}")))?;
        if id.get_version_num() != 8 {
            return Err(CatalogError::InvalidData(
                "catalog id must be UUIDv8".into(),
            ));
        }
        Ok(Self(id))
    }
}

fn normalize_path(path: &str) -> Result<String, CatalogError> {
    let parts = path
        .trim()
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(|p| {
            if p == ".." {
                Err(CatalogError::InvalidData(
                    "catalog path cannot contain '..'".into(),
                ))
            } else {
                Ok(p)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parts.is_empty() {
        return Err(CatalogError::InvalidData("catalog path is empty".into()));
    }
    Ok(format!("/{}", parts.join("/")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Raster,
    Vector,
}
impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Raster => "raster",
            Self::Vector => "vector",
        }
    }
    fn parse(value: &str) -> Result<Self, CatalogError> {
        match value {
            "raster" => Ok(Self::Raster),
            "vector" => Ok(Self::Vector),
            _ => Err(CatalogError::InvalidData(format!(
                "unknown source kind: {value}"
            ))),
        }
    }
}

/// A physical or logical data asset. It contains no WMS publication settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub id: Option<CatalogItemId>,
    pub app_id: String,
    pub catalog_path: String,
    pub title: String,
    pub description: Option<String>,
    pub source_kind: SourceKind,
    pub source_config: Value,
    pub native_crs: Crs,
    pub native_extent: BoundingBox,
    pub attribution: Option<String>,
}

impl CatalogItem {
    pub fn validate(&self) -> Result<(), CatalogError> {
        let expected = CatalogItemId::derive(&self.app_id, &self.catalog_path)?;
        if self.id.is_some_and(|id| id != expected) {
            return Err(CatalogError::InvalidData(
                "id does not match app_id and catalog_path".into(),
            ));
        }
        if self.title.trim().is_empty() {
            return Err(CatalogError::InvalidData("title is empty".into()));
        }
        if !self.native_extent.is_valid() {
            return Err(CatalogError::InvalidData("invalid native extent".into()));
        }
        if !self.source_config.is_object() {
            return Err(CatalogError::InvalidData(
                "source_config must be a JSON object".into(),
            ));
        }
        Ok(())
    }
}

/// WMS-facing configuration that publishes one catalog item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishedLayer {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub catalog_item_id: CatalogItemId,
    pub default_style_id: Option<String>,
    pub supported_crs: Vec<Crs>,
    #[serde(default)]
    pub min_scale_denominator: Option<f64>,
    #[serde(default)]
    pub max_scale_denominator: Option<f64>,
    #[serde(default)]
    pub simplify_factor: Option<f64>,
    pub queryable: bool,
    pub enabled: bool,
}

impl PublishedLayer {
    pub fn derive_id(app_id: &str, name: &str) -> Result<String, CatalogError> {
        let app_id = app_id.trim();
        let name = name.trim();
        if app_id.is_empty() || name.is_empty() {
            return Err(CatalogError::InvalidData(
                "app_id and published layer name are required".into(),
            ));
        }
        let mut hash = Sha256::new();
        hash.update(PUBLISHED_LAYER_ID_DOMAIN);
        for value in [app_id.as_bytes(), name.as_bytes()] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value);
        }
        let digest = hash.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Uuid::from_bytes(bytes).to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleDefinition {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub title: String,
    pub format: String,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleRecord {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub geometry_type: String,
    pub source_format: String,
    pub source_text: String,
    pub compiled_ir: Value,
    pub compiled_sld: String,
    pub compiler_version: String,
    pub revision: i64,
    pub content_hash: String,
    pub validation_error: Option<String>,
}

impl StyleRecord {
    pub fn derive_id(app_id: &str, name: &str) -> Result<String, CatalogError> {
        derive_named_id(STYLE_ID_DOMAIN, app_id, name)
    }

    pub fn validate(&self) -> Result<(), CatalogError> {
        let expected = Self::derive_id(&self.app_id, &self.name)?;
        if !self.id.is_empty() && self.id != expected {
            return Err(CatalogError::InvalidData(
                "style id does not match app_id and name".into(),
            ));
        }
        if self.title.trim().is_empty() {
            return Err(CatalogError::InvalidData("style title is required".into()));
        }
        if self.geometry_type != "polygon" || self.source_format != "geocss" {
            return Err(CatalogError::InvalidData(
                "stage 2 supports polygon GeoCSS styles only".into(),
            ));
        }
        if self.source_text.trim().is_empty()
            || self.compiled_sld.trim().is_empty()
            || !self.compiled_ir.is_object()
        {
            return Err(CatalogError::InvalidData(
                "style source and compiled artifacts are required".into(),
            ));
        }
        if self.content_hash.len() != 64
            || !self
                .content_hash
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(CatalogError::InvalidData(
                "style content_hash must be a SHA-256 hex string".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerGroup {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub members: Vec<LayerGroupMember>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerGroupMember {
    pub published_layer_id: String,
    pub style_id: Option<String>,
    pub position: i32,
    pub opacity: f64,
    #[serde(default)]
    pub min_scale_denominator: Option<f64>,
    #[serde(default)]
    pub max_scale_denominator: Option<f64>,
    #[serde(default)]
    pub simplify_factor: Option<f64>,
}

impl LayerGroup {
    pub fn derive_id(app_id: &str, name: &str) -> Result<String, CatalogError> {
        derive_named_id(LAYER_GROUP_ID_DOMAIN, app_id, name)
    }
}

fn derive_named_id(domain: &[u8], app_id: &str, name: &str) -> Result<String, CatalogError> {
    let app_id = app_id.trim();
    let name = name.trim();
    if app_id.is_empty() || name.is_empty() {
        return Err(CatalogError::InvalidData(
            "app_id and name are required".into(),
        ));
    }
    let mut hash = Sha256::new();
    hash.update(domain);
    for value in [app_id.as_bytes(), name.as_bytes()] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(bytes).to_string())
}

fn validate_scale_policy(
    minimum: Option<f64>,
    maximum: Option<f64>,
    simplify_factor: Option<f64>,
) -> Result<(), CatalogError> {
    if minimum.is_some_and(|value| !value.is_finite() || value <= 0.0)
        || maximum.is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(CatalogError::InvalidData(
            "scale denominators must be finite and greater than zero".into(),
        ));
    }
    if matches!((minimum, maximum), (Some(minimum), Some(maximum)) if minimum >= maximum) {
        return Err(CatalogError::InvalidData(
            "minimum scale denominator must be less than maximum".into(),
        ));
    }
    if simplify_factor.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return Err(CatalogError::InvalidData(
            "simplify factor must be finite and non-negative".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("catalog item not found: {0}")]
    NotFound(String),
    #[error("invalid catalog data: {0}")]
    InvalidData(String),
    #[error("catalog persistence error: {0}")]
    Persistence(#[from] sqlx::Error),
    #[error("catalog serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("catalog migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

#[async_trait]
pub trait Catalog: Send + Sync {
    async fn list_items(&self) -> Result<Vec<CatalogItem>, CatalogError>;
    async fn get_item(&self, id: CatalogItemId) -> Result<Option<CatalogItem>, CatalogError>;
    async fn put_item(&self, item: &CatalogItem) -> Result<CatalogItem, CatalogError>;
    async fn delete_item(&self, id: CatalogItemId) -> Result<bool, CatalogError>;
}

#[async_trait]
pub trait PublicationStore: Send + Sync {
    async fn list_published_layers(&self) -> Result<Vec<PublishedLayer>, CatalogError>;
    async fn list_layer_groups(&self) -> Result<Vec<LayerGroup>, CatalogError>;
    async fn get_layer_group(&self, id: &str) -> Result<Option<LayerGroup>, CatalogError>;
    async fn put_layer_group(&self, group: &LayerGroup) -> Result<LayerGroup, CatalogError>;
    async fn delete_layer_group(&self, id: &str) -> Result<bool, CatalogError>;
    async fn get_published_layer(&self, id: &str) -> Result<Option<PublishedLayer>, CatalogError>;
    async fn put_published_layer(
        &self,
        layer: &PublishedLayer,
    ) -> Result<PublishedLayer, CatalogError>;
    async fn delete_published_layer(&self, id: &str) -> Result<bool, CatalogError>;
}

#[async_trait]
pub trait StyleStore: Send + Sync {
    async fn list_styles(&self) -> Result<Vec<StyleRecord>, CatalogError>;
    async fn get_style(&self, id: &str) -> Result<Option<StyleRecord>, CatalogError>;
    async fn put_style(&self, style: &StyleRecord) -> Result<StyleRecord, CatalogError>;
    async fn delete_style(&self, id: &str) -> Result<bool, CatalogError>;
}

#[derive(Clone)]
pub struct SqliteCatalog {
    pool: SqlitePool,
}
impl SqliteCatalog {
    pub async fn connect(url: &str) -> Result<Self, CatalogError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }
    pub async fn in_memory() -> Result<Self, CatalogError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }
    pub async fn get_setting(&self, key: &str) -> Result<Option<Value>, CatalogError> {
        let value =
            sqlx::query_scalar::<_, String>("SELECT value FROM system_settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        value
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(CatalogError::from)
    }
    pub async fn put_setting(&self, key: &str, value: &Value) -> Result<(), CatalogError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(CatalogError::InvalidData("setting key is empty".into()));
        }
        sqlx::query(
            "INSERT INTO system_settings(key,value,updated_at) VALUES(?,?,CURRENT_TIMESTAMP) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=CURRENT_TIMESTAMP",
        )
        .bind(key)
        .bind(serde_json::to_string(value)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    fn hydrate(row: ItemRow) -> Result<CatalogItem, CatalogError> {
        Ok(CatalogItem {
            id: Some(row.id.parse()?),
            app_id: row.app_id,
            catalog_path: row.catalog_path,
            title: row.title,
            description: row.description,
            source_kind: SourceKind::parse(&row.source_kind)?,
            source_config: serde_json::from_str(&row.source_config)?,
            native_crs: Crs(row.native_crs),
            native_extent: BoundingBox {
                min_x: row.min_x,
                min_y: row.min_y,
                max_x: row.max_x,
                max_y: row.max_y,
            },
            attribution: row.attribution,
        })
    }
}

#[derive(FromRow)]
struct ItemRow {
    id: String,
    app_id: String,
    catalog_path: String,
    title: String,
    description: Option<String>,
    source_kind: String,
    source_config: String,
    native_crs: String,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    attribution: Option<String>,
}
const SELECT: &str="SELECT id,app_id,catalog_path,title,description,source_kind,source_config,native_crs,min_x,min_y,max_x,max_y,attribution FROM catalog_items";

#[derive(FromRow)]
struct PublishedRow {
    id: String,
    app_id: String,
    name: String,
    title: String,
    description: Option<String>,
    catalog_item_id: String,
    default_style_id: Option<String>,
    min_scale_denominator: Option<f64>,
    max_scale_denominator: Option<f64>,
    simplify_factor: Option<f64>,
    queryable: bool,
    enabled: bool,
}

#[derive(FromRow)]
struct LayerGroupRow {
    id: String,
    app_id: String,
    name: String,
    title: String,
    description: Option<String>,
    enabled: bool,
}

#[derive(FromRow)]
struct LayerGroupMemberRow {
    published_layer_id: String,
    style_id: Option<String>,
    position: i32,
    opacity: f64,
    min_scale_denominator: Option<f64>,
    max_scale_denominator: Option<f64>,
    simplify_factor: Option<f64>,
}
#[derive(FromRow)]
struct StyleRow {
    id: String,
    app_id: String,
    name: String,
    title: String,
    description: Option<String>,
    geometry_type: String,
    source_format: String,
    source_text: String,
    compiled_ir: String,
    compiled_sld: String,
    compiler_version: String,
    revision: i64,
    content_hash: String,
    validation_error: Option<String>,
}
const SELECT_STYLE: &str = "SELECT id,app_id,name,title,description,geometry_type,source_format,source_text,compiled_ir,compiled_sld,compiler_version,revision,content_hash,validation_error FROM styles";
const SELECT_PUBLISHED:&str="SELECT id,app_id,name,title,description,catalog_item_id,default_style_id,min_scale_denominator,max_scale_denominator,simplify_factor,queryable,enabled FROM published_layers";

#[async_trait]
impl Catalog for SqliteCatalog {
    async fn list_items(&self) -> Result<Vec<CatalogItem>, CatalogError> {
        sqlx::query_as::<_, ItemRow>(&format!("{SELECT} ORDER BY app_id,catalog_path"))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Self::hydrate)
            .collect()
    }
    async fn get_item(&self, id: CatalogItemId) -> Result<Option<CatalogItem>, CatalogError> {
        sqlx::query_as::<_, ItemRow>(&format!("{SELECT} WHERE id=?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .map(Self::hydrate)
            .transpose()
    }
    async fn put_item(&self, item: &CatalogItem) -> Result<CatalogItem, CatalogError> {
        item.validate()?;
        let id = CatalogItemId::derive(&item.app_id, &item.catalog_path)?;
        let path = normalize_path(&item.catalog_path)?;
        sqlx::query("INSERT INTO catalog_items(id,app_id,catalog_path,title,description,source_kind,source_config,native_crs,min_x,min_y,max_x,max_y,attribution) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,description=excluded.description,source_kind=excluded.source_kind,source_config=excluded.source_config,native_crs=excluded.native_crs,min_x=excluded.min_x,min_y=excluded.min_y,max_x=excluded.max_x,max_y=excluded.max_y,attribution=excluded.attribution,updated_at=CURRENT_TIMESTAMP")
            .bind(id.to_string()).bind(&item.app_id).bind(path).bind(&item.title).bind(&item.description).bind(item.source_kind.as_str()).bind(serde_json::to_string(&item.source_config)?).bind(&item.native_crs.0).bind(item.native_extent.min_x).bind(item.native_extent.min_y).bind(item.native_extent.max_x).bind(item.native_extent.max_y).bind(&item.attribution).execute(&self.pool).await?;
        self.get_item(id)
            .await?
            .ok_or_else(|| CatalogError::NotFound(id.to_string()))
    }
    async fn delete_item(&self, id: CatalogItemId) -> Result<bool, CatalogError> {
        let id = id.to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM layer_group_members WHERE published_layer_id IN (SELECT id FROM published_layers WHERE catalog_item_id=?)",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM published_layers WHERE catalog_item_id=?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM catalog_items WHERE id=?")
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(deleted)
    }
}

#[async_trait]
impl PublicationStore for SqliteCatalog {
    async fn list_published_layers(&self) -> Result<Vec<PublishedLayer>, CatalogError> {
        let rows =
            sqlx::query_as::<_, PublishedRow>(&format!("{SELECT_PUBLISHED} ORDER BY app_id,name"))
                .fetch_all(&self.pool)
                .await?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            result.push(self.hydrate_published(row).await?);
        }
        Ok(result)
    }
    async fn list_layer_groups(&self) -> Result<Vec<LayerGroup>, CatalogError> {
        let rows = sqlx::query_as::<_, LayerGroupRow>(
            "SELECT id,app_id,name,title,description,enabled FROM layer_groups ORDER BY app_id,name",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut groups = Vec::with_capacity(rows.len());
        for row in rows {
            let members = sqlx::query_as::<_, LayerGroupMemberRow>(
                "SELECT published_layer_id,style_id,position,opacity,min_scale_denominator,max_scale_denominator,simplify_factor FROM layer_group_members WHERE group_id=? ORDER BY position",
            )
            .bind(&row.id)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|member| LayerGroupMember {
                published_layer_id: member.published_layer_id,
                style_id: member.style_id,
                position: member.position,
                opacity: member.opacity,
                min_scale_denominator: member.min_scale_denominator,
                max_scale_denominator: member.max_scale_denominator,
                simplify_factor: member.simplify_factor,
            })
            .collect();
            groups.push(LayerGroup {
                id: row.id,
                app_id: row.app_id,
                name: row.name,
                title: row.title,
                description: row.description,
                enabled: row.enabled,
                members,
            });
        }
        Ok(groups)
    }
    async fn get_layer_group(&self, id: &str) -> Result<Option<LayerGroup>, CatalogError> {
        Ok(self
            .list_layer_groups()
            .await?
            .into_iter()
            .find(|group| group.id == id))
    }
    async fn put_layer_group(&self, group: &LayerGroup) -> Result<LayerGroup, CatalogError> {
        if group.title.trim().is_empty() {
            return Err(CatalogError::InvalidData(
                "layer group title is required".into(),
            ));
        }
        let id = LayerGroup::derive_id(&group.app_id, &group.name)?;
        let mut seen = std::collections::HashSet::new();
        for member in &group.members {
            if !seen.insert(&member.published_layer_id) {
                return Err(CatalogError::InvalidData(
                    "图层组中不能重复添加同一图层".into(),
                ));
            }
            if self
                .get_published_layer(&member.published_layer_id)
                .await?
                .is_none()
            {
                return Err(CatalogError::NotFound(member.published_layer_id.clone()));
            }
            if !member.opacity.is_finite() || !(0.0..=1.0).contains(&member.opacity) {
                return Err(CatalogError::InvalidData(
                    "图层组成员透明度必须位于 0 到 1 之间".into(),
                ));
            }
            if let Some(style_id) = member.style_id.as_deref() {
                if self.get_style(style_id).await?.is_none() {
                    return Err(CatalogError::NotFound(style_id.to_owned()));
                }
            }
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO layer_groups(id,app_id,name,title,description,enabled) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,description=excluded.description,enabled=excluded.enabled")
            .bind(&id).bind(&group.app_id).bind(&group.name).bind(&group.title).bind(&group.description).bind(group.enabled).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM layer_group_members WHERE group_id=?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for (index, member) in group.members.iter().enumerate() {
            validate_scale_policy(
                member.min_scale_denominator,
                member.max_scale_denominator,
                member.simplify_factor,
            )?;
            sqlx::query("INSERT INTO layer_group_members(group_id,published_layer_id,style_id,position,opacity,min_scale_denominator,max_scale_denominator,simplify_factor) VALUES(?,?,?,?,?,?,?,?)")
                .bind(&id).bind(&member.published_layer_id).bind(&member.style_id).bind(index as i32).bind(member.opacity).bind(member.min_scale_denominator).bind(member.max_scale_denominator).bind(member.simplify_factor).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.get_layer_group(&id)
            .await?
            .ok_or(CatalogError::NotFound(id))
    }
    async fn delete_layer_group(&self, id: &str) -> Result<bool, CatalogError> {
        Ok(sqlx::query("DELETE FROM layer_groups WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }
    async fn get_published_layer(&self, id: &str) -> Result<Option<PublishedLayer>, CatalogError> {
        let row = sqlx::query_as::<_, PublishedRow>(&format!("{SELECT_PUBLISHED} WHERE id=?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(Some(self.hydrate_published(row).await?)),
            None => Ok(None),
        }
    }
    async fn put_published_layer(
        &self,
        layer: &PublishedLayer,
    ) -> Result<PublishedLayer, CatalogError> {
        if layer.title.trim().is_empty() {
            return Err(CatalogError::InvalidData(
                "published layer title is required".into(),
            ));
        }
        if self.get_item(layer.catalog_item_id).await?.is_none() {
            return Err(CatalogError::NotFound(layer.catalog_item_id.to_string()));
        }
        validate_scale_policy(
            layer.min_scale_denominator,
            layer.max_scale_denominator,
            layer.simplify_factor,
        )?;
        if let Some(style_id) = layer.default_style_id.as_deref() {
            let style = self
                .get_style(style_id)
                .await?
                .ok_or_else(|| CatalogError::NotFound(style_id.to_owned()))?;
            if style.app_id != layer.app_id {
                return Err(CatalogError::InvalidData(
                    "published layer and default style must belong to the same app".into(),
                ));
            }
        }
        let id = PublishedLayer::derive_id(&layer.app_id, &layer.name)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO published_layers(id,app_id,name,title,description,catalog_item_id,default_style_id,min_scale_denominator,max_scale_denominator,simplify_factor,queryable,enabled) VALUES(?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,description=excluded.description,catalog_item_id=excluded.catalog_item_id,default_style_id=excluded.default_style_id,min_scale_denominator=excluded.min_scale_denominator,max_scale_denominator=excluded.max_scale_denominator,simplify_factor=excluded.simplify_factor,queryable=excluded.queryable,enabled=excluded.enabled,updated_at=CURRENT_TIMESTAMP")
            .bind(&id).bind(&layer.app_id).bind(&layer.name).bind(&layer.title).bind(&layer.description).bind(layer.catalog_item_id.to_string()).bind(&layer.default_style_id).bind(layer.min_scale_denominator).bind(layer.max_scale_denominator).bind(layer.simplify_factor).bind(layer.queryable).bind(layer.enabled).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM published_layer_crs WHERE published_layer_id=?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for crs in &layer.supported_crs {
            sqlx::query("INSERT INTO published_layer_crs(published_layer_id,crs) VALUES(?,?)")
                .bind(&id)
                .bind(&crs.0)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.get_published_layer(&id)
            .await?
            .ok_or_else(|| CatalogError::NotFound(id))
    }
    async fn delete_published_layer(&self, id: &str) -> Result<bool, CatalogError> {
        Ok(sqlx::query("DELETE FROM published_layers WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }
}

impl SqliteCatalog {
    async fn hydrate_published(&self, row: PublishedRow) -> Result<PublishedLayer, CatalogError> {
        let supported_crs = sqlx::query_scalar::<_, String>(
            "SELECT crs FROM published_layer_crs WHERE published_layer_id=? ORDER BY crs",
        )
        .bind(&row.id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Crs)
        .collect();
        Ok(PublishedLayer {
            id: row.id,
            app_id: row.app_id,
            name: row.name,
            title: row.title,
            description: row.description,
            catalog_item_id: row.catalog_item_id.parse()?,
            default_style_id: row.default_style_id,
            supported_crs,
            min_scale_denominator: row.min_scale_denominator,
            max_scale_denominator: row.max_scale_denominator,
            simplify_factor: row.simplify_factor,
            queryable: row.queryable,
            enabled: row.enabled,
        })
    }
}

#[async_trait]
impl StyleStore for SqliteCatalog {
    async fn list_styles(&self) -> Result<Vec<StyleRecord>, CatalogError> {
        sqlx::query_as::<_, StyleRow>(&format!("{SELECT_STYLE} ORDER BY app_id,name"))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(hydrate_style)
            .collect()
    }

    async fn get_style(&self, id: &str) -> Result<Option<StyleRecord>, CatalogError> {
        sqlx::query_as::<_, StyleRow>(&format!("{SELECT_STYLE} WHERE id=?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(hydrate_style)
            .transpose()
    }

    async fn put_style(&self, style: &StyleRecord) -> Result<StyleRecord, CatalogError> {
        style.validate()?;
        let id = StyleRecord::derive_id(&style.app_id, &style.name)?;
        sqlx::query(
            "INSERT INTO styles(id,app_id,name,title,format,definition,description,geometry_type,source_format,source_text,compiled_ir,compiled_sld,compiler_version,revision,content_hash,validation_error,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,1,?,?,CURRENT_TIMESTAMP,CURRENT_TIMESTAMP) ON CONFLICT(id) DO UPDATE SET title=excluded.title,definition=excluded.definition,description=excluded.description,geometry_type=excluded.geometry_type,source_format=excluded.source_format,source_text=excluded.source_text,compiled_ir=excluded.compiled_ir,compiled_sld=excluded.compiled_sld,compiler_version=excluded.compiler_version,revision=CASE WHEN styles.content_hash<>excluded.content_hash THEN styles.revision+1 ELSE styles.revision END,content_hash=excluded.content_hash,validation_error=excluded.validation_error,updated_at=CURRENT_TIMESTAMP",
        )
        .bind(&id)
        .bind(&style.app_id)
        .bind(&style.name)
        .bind(&style.title)
        .bind("geocss")
        .bind(&style.source_text)
        .bind(&style.description)
        .bind(&style.geometry_type)
        .bind(&style.source_format)
        .bind(&style.source_text)
        .bind(serde_json::to_string(&style.compiled_ir)?)
        .bind(&style.compiled_sld)
        .bind(&style.compiler_version)
        .bind(&style.content_hash)
        .bind(&style.validation_error)
        .execute(&self.pool)
        .await?;
        self.get_style(&id).await?.ok_or(CatalogError::NotFound(id))
    }

    async fn delete_style(&self, id: &str) -> Result<bool, CatalogError> {
        Ok(sqlx::query("DELETE FROM styles WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }
}

fn hydrate_style(row: StyleRow) -> Result<StyleRecord, CatalogError> {
    Ok(StyleRecord {
        id: row.id,
        app_id: row.app_id,
        name: row.name,
        title: row.title,
        description: row.description,
        geometry_type: row.geometry_type,
        source_format: row.source_format,
        source_text: row.source_text,
        compiled_ir: serde_json::from_str(&row.compiled_ir)?,
        compiled_sld: row.compiled_sld,
        compiler_version: row.compiler_version,
        revision: row.revision,
        content_hash: row.content_hash,
        validation_error: row.validation_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn stores_system_settings() {
        let store = SqliteCatalog::in_memory().await.unwrap();
        assert_eq!(store.get_setting("cache_service").await.unwrap(), None);
        let value = json!({"enabled": true, "base_url": "http://tile:7601"});
        store.put_setting("cache_service", &value).await.unwrap();
        assert_eq!(
            store.get_setting("cache_service").await.unwrap(),
            Some(value)
        );
    }
    fn item() -> CatalogItem {
        CatalogItem {
            id: None,
            app_id: "cis".into(),
            catalog_path: "/rasters/terrain".into(),
            title: "Terrain".into(),
            description: None,
            source_kind: SourceKind::Raster,
            source_config: json!({"path":"/data/terrain.tif"}),
            native_crs: Crs("EPSG:4326".into()),
            native_extent: BoundingBox {
                min_x: 100.,
                min_y: 20.,
                max_x: 120.,
                max_y: 40.,
            },
            attribution: None,
        }
    }
    #[tokio::test]
    async fn round_trip() {
        let c = SqliteCatalog::in_memory().await.unwrap();
        let saved = c.put_item(&item()).await.unwrap();
        assert_eq!(c.get_item(saved.id.unwrap()).await.unwrap(), Some(saved));
    }
    fn style(hash: char, source: &str) -> StyleRecord {
        StyleRecord {
            id: String::new(),
            app_id: "cis".into(),
            name: "parcel-default".into(),
            title: "地块默认样式".into(),
            description: None,
            geometry_type: "polygon".into(),
            source_format: "geocss".into(),
            source_text: source.into(),
            compiled_ir: json!({"name":"parcel-default","rules":[]}),
            compiled_sld: "<sld:StyledLayerDescriptor/>".into(),
            compiler_version: "map-style-geocss-polygon/1".into(),
            revision: 1,
            content_hash: hash.to_string().repeat(64),
            validation_error: None,
        }
    }

    #[tokio::test]
    async fn stores_styles_and_revises_only_when_content_changes() {
        let store = SqliteCatalog::in_memory().await.unwrap();
        let first = store
            .put_style(&style('a', "* { fill: #fff; }"))
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(store.list_styles().await.unwrap(), vec![first.clone()]);

        let unchanged = store
            .put_style(&style('a', "* { fill: #fff; }"))
            .await
            .unwrap();
        assert_eq!(unchanged.revision, 1);

        let changed = store
            .put_style(&style('b', "* { fill: #000; }"))
            .await
            .unwrap();
        assert_eq!(changed.revision, 2);
        assert!(store.delete_style(&changed.id).await.unwrap());
        assert!(store.get_style(&changed.id).await.unwrap().is_none());
    }
    #[test]
    fn deterministic_id() {
        assert_eq!(
            CatalogItemId::derive("cis", "rasters/terrain").unwrap(),
            CatalogItemId::derive("cis", "/rasters//./terrain/").unwrap()
        );
    }
    #[tokio::test]
    async fn publishes_catalog_item() {
        let store = SqliteCatalog::in_memory().await.unwrap();
        let catalog = store.put_item(&item()).await.unwrap();
        let layer = PublishedLayer {
            id: String::new(),
            app_id: "cis".into(),
            name: "terrain".into(),
            title: "区域地形".into(),
            description: None,
            catalog_item_id: catalog.id.unwrap(),
            default_style_id: None,
            supported_crs: vec![Crs("EPSG:4326".into()), Crs("EPSG:3857".into())],
            min_scale_denominator: Some(1_000.0),
            max_scale_denominator: Some(1_000_000.0),
            simplify_factor: Some(0.5),
            queryable: false,
            enabled: true,
        };
        let saved = store.put_published_layer(&layer).await.unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(saved.supported_crs.len(), 2);
        assert_eq!(store.list_published_layers().await.unwrap(), vec![saved]);
    }

    #[tokio::test]
    async fn validates_default_style_application() {
        let store = SqliteCatalog::in_memory().await.unwrap();
        let catalog = store.put_item(&item()).await.unwrap();
        let saved_style = store
            .put_style(&style('a', "* { fill: #fff; }"))
            .await
            .unwrap();
        let mut layer = PublishedLayer {
            id: String::new(),
            app_id: "cis".into(),
            name: "styled-parcels".into(),
            title: "带样式地块".into(),
            description: None,
            catalog_item_id: catalog.id.unwrap(),
            default_style_id: Some(saved_style.id.clone()),
            supported_crs: vec![Crs("EPSG:4326".into())],
            min_scale_denominator: None,
            max_scale_denominator: None,
            simplify_factor: None,
            queryable: false,
            enabled: true,
        };
        assert_eq!(
            store
                .put_published_layer(&layer)
                .await
                .unwrap()
                .default_style_id,
            Some(saved_style.id.clone())
        );

        let foreign = store
            .put_style(&StyleRecord {
                app_id: "another-app".into(),
                name: "foreign".into(),
                ..style('b', "* { fill: #000; }")
            })
            .await
            .unwrap();
        layer.default_style_id = Some(foreign.id);
        assert!(matches!(
            store.put_published_layer(&layer).await,
            Err(CatalogError::InvalidData(_))
        ));
    }

    #[tokio::test]
    async fn lists_group_members_in_position_order() {
        let store = SqliteCatalog::in_memory().await.unwrap();
        let catalog = store.put_item(&item()).await.unwrap();
        let layer = store
            .put_published_layer(&PublishedLayer {
                id: String::new(),
                app_id: "cis".into(),
                name: "terrain".into(),
                title: "区域地形".into(),
                description: None,
                catalog_item_id: catalog.id.unwrap(),
                default_style_id: None,
                supported_crs: vec![],
                min_scale_denominator: None,
                max_scale_denominator: None,
                simplify_factor: None,
                queryable: false,
                enabled: true,
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO layer_groups(id,app_id,name,title,enabled) VALUES('base','cis','base','基础数据',1)")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO layer_group_members(group_id,published_layer_id,position,opacity,min_scale_denominator,max_scale_denominator,simplify_factor) VALUES('base',?,10,1.0,500,500000,0.75)")
            .bind(&layer.id)
            .execute(&store.pool)
            .await
            .unwrap();

        let groups = store.list_layer_groups().await.unwrap();
        assert_eq!(groups[0].title, "基础数据");
        assert_eq!(groups[0].members[0].min_scale_denominator, Some(500.0));
        assert_eq!(groups[0].members[0].max_scale_denominator, Some(500_000.0));
        assert_eq!(groups[0].members[0].simplify_factor, Some(0.75));
        assert_eq!(groups[0].members[0].published_layer_id, layer.id);
        assert_eq!(groups[0].members[0].position, 10);
    }

    #[tokio::test]
    async fn deleting_catalog_item_removes_published_layers_and_group_members() {
        let store = SqliteCatalog::in_memory().await.unwrap();
        let catalog = store.put_item(&item()).await.unwrap();
        let catalog_id = catalog.id.unwrap();
        let layer = store
            .put_published_layer(&PublishedLayer {
                id: String::new(),
                app_id: "cis".into(),
                name: "terrain".into(),
                title: "区域地形".into(),
                description: None,
                catalog_item_id: catalog_id,
                default_style_id: None,
                supported_crs: vec![],
                min_scale_denominator: None,
                max_scale_denominator: None,
                simplify_factor: None,
                queryable: false,
                enabled: true,
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO layer_groups(id,app_id,name,title,enabled) VALUES('base','cis','base','基础数据',1)")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO layer_group_members(group_id,published_layer_id,position,opacity) VALUES('base',?,0,1.0)")
            .bind(&layer.id)
            .execute(&store.pool)
            .await
            .unwrap();

        assert!(store.delete_item(catalog_id).await.unwrap());
        assert!(store.get_item(catalog_id).await.unwrap().is_none());
        assert!(store.list_published_layers().await.unwrap().is_empty());
        let groups = store.list_layer_groups().await.unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].members.is_empty());
    }
}
