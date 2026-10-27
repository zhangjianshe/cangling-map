use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::{CatalogError, SqliteCatalog};

/// Published DEM metadata. Bounds are in EPSG:3857 metres.
#[derive(Clone, Debug, FromRow, Serialize, Deserialize)]
pub struct DemDataset {
    pub id: String,
    pub title: String,
    pub revision: String,
    pub vertical_datum: String,
    pub attribution: Option<String>,
    pub min_zoom: i64,
    pub max_zoom: i64,
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

#[derive(Clone, Debug, FromRow, Serialize, Deserialize)]
pub struct DemAsset {
    pub dataset_id: String,
    pub path: String,
    pub native_crs: String,
    pub width: i64,
    pub height: i64,
    pub resolution_m: f64,
    pub nodata: Option<f64>,
    pub unit: String,
    pub priority: i64,
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl SqliteCatalog {
    pub async fn get_dem_dataset(&self, id: &str) -> Result<Option<DemDataset>, CatalogError> {
        sqlx::query_as::<_, DemDataset>(
            "SELECT id,title,revision,vertical_datum,attribution,min_zoom,max_zoom,\
             min_x,min_y,max_x,max_y FROM dem_datasets WHERE id=?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn put_dem_dataset(
        &self,
        dataset: &DemDataset,
        assets: &[DemAsset],
    ) -> Result<(), CatalogError> {
        if assets.is_empty()
            || dataset.min_zoom < 0
            || dataset.max_zoom > 22
            || dataset.min_zoom > dataset.max_zoom
        {
            return Err(CatalogError::InvalidData("invalid DEM publication".into()));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM dem_assets WHERE dataset_id=?")
            .bind(&dataset.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO dem_datasets(id,title,revision,vertical_datum,attribution,\
             min_zoom,max_zoom,min_x,min_y,max_x,max_y) VALUES(?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET title=excluded.title,revision=excluded.revision,\
             vertical_datum=excluded.vertical_datum,attribution=excluded.attribution,\
             min_zoom=excluded.min_zoom,max_zoom=excluded.max_zoom,\
             min_x=excluded.min_x,min_y=excluded.min_y,max_x=excluded.max_x,max_y=excluded.max_y,\
             updated_at=CURRENT_TIMESTAMP",
        )
        .bind(&dataset.id)
        .bind(&dataset.title)
        .bind(&dataset.revision)
        .bind(&dataset.vertical_datum)
        .bind(&dataset.attribution)
        .bind(dataset.min_zoom)
        .bind(dataset.max_zoom)
        .bind(dataset.min_x)
        .bind(dataset.min_y)
        .bind(dataset.max_x)
        .bind(dataset.max_y)
        .execute(&mut *tx)
        .await?;
        for asset in assets {
            sqlx::query(
                "INSERT INTO dem_assets(dataset_id,path,native_crs,width,height,resolution_m,\
                 nodata,unit,priority,min_x,min_y,max_x,max_y) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(&dataset.id)
            .bind(&asset.path)
            .bind(&asset.native_crs)
            .bind(asset.width)
            .bind(asset.height)
            .bind(asset.resolution_m)
            .bind(asset.nodata)
            .bind(&asset.unit)
            .bind(asset.priority)
            .bind(asset.min_x)
            .bind(asset.min_y)
            .bind(asset.max_x)
            .bind(asset.max_y)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn dem_assets_at(
        &self,
        id: &str,
        bounds: [f64; 4],
    ) -> Result<Vec<DemAsset>, CatalogError> {
        sqlx::query_as::<_, DemAsset>(
            "SELECT a.dataset_id,a.path,a.native_crs,a.width,a.height,a.resolution_m,\
             a.nodata,a.unit,a.priority,a.min_x,a.min_y,a.max_x,a.max_y \
             FROM dem_asset_bounds b JOIN dem_assets a ON a.id=b.id \
             WHERE a.dataset_id=? AND b.min_x<? AND b.max_x>? AND b.min_y<? AND b.max_y>? \
             ORDER BY a.priority DESC,a.resolution_m ASC,a.id ASC",
        )
        .bind(id)
        .bind(bounds[2])
        .bind(bounds[0])
        .bind(bounds[3])
        .bind(bounds[1])
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::{DemAsset, DemDataset};
    use crate::SqliteCatalog;

    #[tokio::test]
    async fn republishing_replaces_spatial_index_entries() {
        let catalog = SqliteCatalog::in_memory().await.unwrap();
        let mut dataset = DemDataset {
            id: "test".into(),
            title: "Test".into(),
            revision: "one".into(),
            vertical_datum: "EGM96".into(),
            attribution: None,
            min_zoom: 0,
            max_zoom: 18,
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        let mut asset = DemAsset {
            dataset_id: dataset.id.clone(),
            path: "/tmp/test.tif".into(),
            native_crs: "EPSG:3857".into(),
            width: 10,
            height: 10,
            resolution_m: 1.0,
            nodata: None,
            unit: "metre".into(),
            priority: 0,
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        catalog
            .put_dem_dataset(&dataset, &[asset.clone()])
            .await
            .unwrap();
        assert_eq!(
            catalog
                .dem_assets_at("test", [1.0, 1.0, 2.0, 2.0])
                .await
                .unwrap()
                .len(),
            1
        );
        dataset.revision = "two".into();
        dataset.min_x = 100.0;
        dataset.max_x = 110.0;
        asset.min_x = 100.0;
        asset.max_x = 110.0;
        catalog.put_dem_dataset(&dataset, &[asset]).await.unwrap();
        assert!(catalog
            .dem_assets_at("test", [1.0, 1.0, 2.0, 2.0])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            catalog
                .dem_assets_at("test", [101.0, 1.0, 102.0, 2.0])
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            catalog
                .get_dem_dataset("test")
                .await
                .unwrap()
                .unwrap()
                .revision,
            "two"
        );
    }
}
