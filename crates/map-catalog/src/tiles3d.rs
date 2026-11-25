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
