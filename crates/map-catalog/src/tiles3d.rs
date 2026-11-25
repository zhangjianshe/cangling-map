use crate::{CatalogError, SqliteCatalog};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Tiles3dDataset {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub revision: Option<String>,
    pub updated_at: String,
}
impl SqliteCatalog {
    /// Dataset-local publication database; intentionally contains no global catalog tables.
    pub async fn open_tiles3d(path: &std::path::Path) -> Result<Self, CatalogError> {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .idle_timeout(std::time::Duration::from_secs(30))
            .connect_with(options)
            .await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS tiles3d_datasets (
            id TEXT PRIMARY KEY,title TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 0,
            revision TEXT,updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
            CREATE TABLE IF NOT EXISTS tiles3d_versions (
            dataset_id TEXT NOT NULL REFERENCES tiles3d_datasets(id),revision TEXT NOT NULL,
            manifest TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY(dataset_id,revision));
            CREATE TABLE IF NOT EXISTS tiles3d_files (
            dataset_id TEXT NOT NULL, revision TEXT NOT NULL, path TEXT NOT NULL, info TEXT NOT NULL,
            PRIMARY KEY(dataset_id,revision,path));").execute(&pool).await?;
        Ok(Self { pool })
    }
    /// One transaction imports all historical versions. Existing local state always wins.
    pub async fn import_tiles3d(
        &self,
        source: &Self,
        d: &Tiles3dDataset,
    ) -> Result<(), CatalogError> {
        if self.get_tiles3d(&d.id).await?.is_some() {
            return Ok(());
        }
        let versions: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT revision,manifest,created_at FROM tiles3d_versions WHERE dataset_id=?",
        )
        .bind(&d.id)
        .fetch_all(&source.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO tiles3d_datasets(id,title,enabled,revision,updated_at) VALUES(?,?,?,?,?)")
            .bind(&d.id).bind(&d.title).bind(d.enabled).bind(&d.revision).bind(&d.updated_at)
            .execute(&mut *tx).await?.rows_affected();
        if inserted > 0 {
            for (revision, manifest, created) in versions {
                sqlx::query("INSERT INTO tiles3d_versions(dataset_id,revision,manifest,created_at) VALUES(?,?,?,?)")
                    .bind(&d.id).bind(revision).bind(manifest).bind(created).execute(&mut *tx).await?;
            }
        }
        // A file index avoids parsing a potentially huge publication manifest per HTTP request.
        sqlx::query("INSERT OR IGNORE INTO tiles3d_files SELECT v.dataset_id,v.revision,j.key,j.value FROM tiles3d_versions v,json_each(v.manifest,'$.files') j")
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn tiles3d_file(
        &self,
        id: &str,
        revision: &str,
        path: &str,
    ) -> Result<Option<Value>, CatalogError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT info FROM tiles3d_files WHERE dataset_id=? AND revision=? AND path=?",
        )
        .bind(id)
        .bind(revision)
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        raw.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(CatalogError::from)
    }
    pub async fn list_tiles3d(&self) -> Result<Vec<Tiles3dDataset>, CatalogError> {
        Ok(
            sqlx::query_as("SELECT * FROM tiles3d_datasets ORDER BY title,id")
                .fetch_all(&self.pool)
                .await?,
        )
    }
    pub async fn get_tiles3d(&self, id: &str) -> Result<Option<Tiles3dDataset>, CatalogError> {
        Ok(sqlx::query_as("SELECT * FROM tiles3d_datasets WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }
    pub async fn register_tiles3d(&self, id: &str, title: &str) -> Result<(), CatalogError> {
        sqlx::query("INSERT INTO tiles3d_datasets(id,title) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title,updated_at=CURRENT_TIMESTAMP").bind(id).bind(title).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn publish_tiles3d(
        &self,
        id: &str,
        revision: &str,
        manifest: &Value,
    ) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO tiles3d_versions(dataset_id,revision,manifest) VALUES(?,?,?) ON CONFLICT DO NOTHING").bind(id).bind(revision).bind(manifest.to_string()).execute(&mut *tx).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS tiles3d_files (dataset_id TEXT NOT NULL,revision TEXT NOT NULL,path TEXT NOT NULL,info TEXT NOT NULL,PRIMARY KEY(dataset_id,revision,path))").execute(&mut *tx).await?;
        sqlx::query("INSERT OR IGNORE INTO tiles3d_files SELECT dataset_id,revision,j.key,j.value FROM tiles3d_versions,json_each(manifest,'$.files') j WHERE dataset_id=? AND revision=?")
            .bind(id).bind(revision).execute(&mut *tx).await?;
        sqlx::query("UPDATE tiles3d_datasets SET revision=?,enabled=1,updated_at=CURRENT_TIMESTAMP WHERE id=?").bind(revision).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn disable_tiles3d(&self, id: &str) -> Result<(), CatalogError> {
        sqlx::query(
            "UPDATE tiles3d_datasets SET enabled=0,updated_at=CURRENT_TIMESTAMP WHERE id=?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn tiles3d_manifest(
        &self,
        id: &str,
        revision: &str,
    ) -> Result<Option<Value>, CatalogError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT manifest FROM tiles3d_versions WHERE dataset_id=? AND revision=?",
        )
        .bind(id)
        .bind(revision)
        .fetch_optional(&self.pool)
        .await?;
        raw.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(CatalogError::from)
    }
}
