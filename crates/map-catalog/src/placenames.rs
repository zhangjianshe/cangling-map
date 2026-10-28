use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};

use crate::CatalogError;

#[derive(Clone)]
pub struct SqlitePlaceNames {
    pool: SqlitePool,
}

#[derive(Debug, FromRow, Serialize)]
pub struct PlaceName {
    pub id: i64,
    pub name: String,
    pub longitude: f64,
    pub latitude: f64,
    pub feature_class: String,
    pub label_rank: i64,
    pub min_zoom: f64,
    pub population: Option<i64>,
}

impl SqlitePlaceNames {
    pub async fn connect(url: &str) -> Result<Self, CatalogError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await?;
        sqlx::query_scalar::<_, String>("SELECT value FROM metadata WHERE key='schema_version'")
            .fetch_one(&pool)
            .await?;
        Ok(Self { pool })
    }

    pub async fn metadata(&self) -> Result<BTreeMap<String, String>, CatalogError> {
        let entries =
            sqlx::query_as::<_, (String, String)>("SELECT key,value FROM metadata ORDER BY key")
                .fetch_all(&self.pool)
                .await?;
        Ok(entries.into_iter().collect())
    }

    /// `bounds` are west, south, east, north in WGS84 degrees.
    pub async fn within_bounds(
        &self,
        bounds: [f64; 4],
        zoom: u8,
        language: &str,
        limit: i64,
    ) -> Result<Vec<PlaceName>, CatalogError> {
        let name_expression = match language {
            "zh" => "COALESCE(NULLIF(p.name_zh,''),p.name)",
            "en" => "COALESCE(NULLIF(p.name_en,''),p.name)",
            _ => "p.name",
        };
        let sql = format!(
            "SELECT p.id,{name_expression} AS name,p.longitude,p.latitude,\
             p.feature_class,p.label_rank,p.min_zoom,p.population \
             FROM place_bounds b JOIN places p ON p.id=b.id \
             WHERE b.max_lon>=? AND b.min_lon<? AND b.max_lat>=? AND b.min_lat<? \
             AND p.longitude>=? AND p.longitude<? AND p.latitude>=? AND p.latitude<? \
             AND p.min_zoom<=? \
             ORDER BY p.label_rank ASC,COALESCE(p.population,0) DESC,p.id ASC LIMIT ?"
        );
        sqlx::query_as::<_, PlaceName>(&sql)
            .bind(bounds[0])
            .bind(bounds[2])
            .bind(bounds[1])
            .bind(bounds[3])
            .bind(bounds[0])
            .bind(bounds[2])
            .bind(bounds[1])
            .bind(bounds[3])
            .bind(zoom as f64)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_localized_places_in_the_requested_bounds_and_zoom() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE VIRTUAL TABLE place_bounds USING rtree(id,min_lon,max_lon,min_lat,max_lat)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("CREATE TABLE places(id INTEGER PRIMARY KEY,name TEXT,name_zh TEXT,name_en TEXT,longitude REAL,latitude REAL,feature_class TEXT,label_rank INTEGER,min_zoom REAL,population INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO places VALUES(1,'Beijing','北京','Beijing',116.4,39.9,'Admin-0 capital',1,2.0,20000000)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO place_bounds VALUES(1,116.4,116.4,39.9,39.9)")
            .execute(&pool)
            .await
            .unwrap();
        let names = SqlitePlaceNames { pool };
        assert!(names
            .within_bounds([110.0, 35.0, 120.0, 45.0], 1, "zh", 10)
            .await
            .unwrap()
            .is_empty());
        let visible = names
            .within_bounds([110.0, 35.0, 120.0, 45.0], 2, "zh", 10)
            .await
            .unwrap();
        assert_eq!(visible[0].name, "北京");
        assert!(names
            .within_bounds([0.0, 0.0, 10.0, 10.0], 2, "zh", 10)
            .await
            .unwrap()
            .is_empty());
    }
}
