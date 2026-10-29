use std::{collections::BTreeMap, io::Read};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    FromRow, SqlitePool,
};

use crate::CatalogError;

#[derive(Clone)]
pub struct SqliteAdmin {
    pool: SqlitePool,
    attribution: String,
    revision: String,
}

#[derive(Debug, FromRow, Serialize)]
pub struct AdminLabel {
    pub id: i64,
    pub name: String,
    pub longitude: f64,
    pub latitude: f64,
    pub admin_level: i64,
}

#[derive(Debug, Serialize)]
pub struct AdminBoundary {
    pub id: i64,
    pub admin_level: i64,
    pub geometry: Value,
}

#[derive(FromRow)]
struct BoundaryRow {
    id: i64,
    level: i64,
    geometry_json: String,
}

impl SqliteAdmin {
    pub async fn connect(url: &str) -> Result<Self, CatalogError> {
        let options: SqliteConnectOptions = url.parse()?;
        let mut file = std::fs::File::open(options.get_filename()).map_err(|error| {
            CatalogError::InvalidData(format!(
                "cannot read administration database for revision: {error}"
            ))
        })?;
        let mut hasher = Sha256::new();
        let mut chunk = vec![0_u8; 1024 * 1024];
        loop {
            let count = file.read(&mut chunk).map_err(|error| {
                CatalogError::InvalidData(format!("cannot hash administration database: {error}"))
            })?;
            if count == 0 {
                break;
            }
            hasher.update(&chunk[..count]);
        }
        let revision = format!("{:x}", hasher.finalize());
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await?;
        let version: String =
            sqlx::query_scalar("SELECT value FROM metadata WHERE key='schema_version'")
                .fetch_one(&pool)
                .await?;
        if version != "1" {
            return Err(CatalogError::InvalidData(format!(
                "unsupported administration schema version: {version}"
            )));
        }
        let attribution: String =
            sqlx::query_scalar("SELECT value FROM metadata WHERE key='attribution'")
                .fetch_optional(&pool)
                .await?
                .unwrap_or_else(|| "© OpenStreetMap contributors".into());
        Ok(Self {
            pool,
            attribution,
            revision,
        })
    }

    pub fn attribution(&self) -> &str {
        &self.attribution
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub async fn metadata(&self) -> Result<BTreeMap<String, String>, CatalogError> {
        let rows =
            sqlx::query_as::<_, (String, String)>("SELECT key,value FROM metadata ORDER BY key")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn labels(
        &self,
        bounds: [f64; 4],
        level: i64,
        language: &str,
        limit: i64,
    ) -> Result<Vec<AdminLabel>, CatalogError> {
        let name = match language {
            "zh" => "COALESCE(NULLIF(a.name_zh,''),a.name)",
            "en" => "COALESCE(NULLIF(a.name_en,''),a.name)",
            _ => "a.name",
        };
        let join = if level <= 5 {
            "admin_units a JOIN admin_bounds b ON b.id=a.id"
        } else {
            "admin_bounds b CROSS JOIN admin_units a"
        };
        let sql = format!(
            "SELECT -a.id AS id,{name} AS name,a.longitude,a.latitude,a.level AS admin_level \
             FROM {join} \
             WHERE a.id=b.id AND a.level IN (?,?) AND a.name<>'' AND b.max_lon>=? AND b.min_lon<? \
             AND b.max_lat>=? AND b.min_lat<? \
             AND a.longitude>=? AND a.longitude<? AND a.latitude>=? AND a.latitude<? \
             ORDER BY a.id LIMIT ?"
        );
        sqlx::query_as::<_, AdminLabel>(&sql)
            .bind(level)
            .bind(if level == 4 { 3 } else { level })
            .bind(bounds[0])
            .bind(bounds[2])
            .bind(bounds[1])
            .bind(bounds[3])
            .bind(bounds[0])
            .bind(bounds[2])
            .bind(bounds[1])
            .bind(bounds[3])
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn boundaries(
        &self,
        bounds: [f64; 4],
        level: i64,
        limit: i64,
    ) -> Result<Vec<AdminBoundary>, CatalogError> {
        let join = if level <= 5 {
            "admin_units a JOIN admin_bounds b ON b.id=a.id"
        } else {
            "admin_bounds b CROSS JOIN admin_units a"
        };
        let sql = format!(
            "SELECT -a.id AS id,a.level,a.geometry_json \
             FROM {join} \
             WHERE a.id=b.id AND (a.level IN (?,?) OR \
               (a.source_id='overlay/south-china-sea-dashes' AND ? IN (2,4))) \
             AND a.geometry_json IS NOT NULL \
             AND b.max_lon>=? AND b.min_lon<? AND b.max_lat>=? AND b.min_lat<? \
             AND a.max_lon>=? AND a.min_lon<? AND a.max_lat>=? AND a.min_lat<? \
             ORDER BY a.id LIMIT ?"
        );
        let rows = sqlx::query_as::<_, BoundaryRow>(&sql)
            .bind(level)
            .bind(if level == 4 { 3 } else { level })
            .bind(level)
            .bind(bounds[0])
            .bind(bounds[2])
            .bind(bounds[1])
            .bind(bounds[3])
            .bind(bounds[0])
            .bind(bounds[2])
            .bind(bounds[1])
            .bind(bounds[3])
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(AdminBoundary {
                    id: row.id,
                    admin_level: row.level,
                    geometry: serde_json::from_str(&row.geometry_json)
                        .map_err(|e| CatalogError::InvalidData(e.to_string()))?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn labels_and_boundaries_respect_level_and_bounds() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE admin_units(id INTEGER PRIMARY KEY,source_id TEXT,level INTEGER,name TEXT,name_zh TEXT,name_en TEXT,longitude REAL,latitude REAL,min_lon REAL,max_lon REAL,min_lat REAL,max_lat REAL,geometry_json TEXT)").execute(&pool).await.unwrap();
        sqlx::query(
            "CREATE VIRTUAL TABLE admin_bounds USING rtree(id,min_lon,max_lon,min_lat,max_lat)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO admin_units VALUES(1,'test/1',4,'Shaanxi','陕西','Shaanxi',108.9,34.3,105.0,111.0,31.0,39.0,'{\"type\":\"Polygon\",\"coordinates\":[]}')").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO admin_bounds VALUES(1,105,111,31,39)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO admin_units VALUES(2,'test/2',3,'Macau','澳门','Macau',113.55,22.2,113.5,113.6,22.1,22.3,'{\"type\":\"Polygon\",\"coordinates\":[]}')").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO admin_bounds VALUES(2,113.5,113.6,22.1,22.3)")
            .execute(&pool)
            .await
            .unwrap();
        let admin = SqliteAdmin {
            pool,
            attribution: String::new(),
            revision: String::new(),
        };
        assert_eq!(
            admin
                .labels([108.0, 34.0, 110.0, 36.0], 4, "zh", 10)
                .await
                .unwrap()[0]
                .name,
            "陕西"
        );
        assert!(admin
            .labels([108.0, 34.0, 110.0, 36.0], 5, "zh", 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            admin
                .labels([113.0, 22.0, 114.0, 23.0], 4, "zh", 10)
                .await
                .unwrap()[0]
                .name,
            "澳门"
        );
        assert_eq!(
            admin
                .boundaries([110.0, 34.0, 112.0, 36.0], 4, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        sqlx::query("INSERT INTO admin_units VALUES(3,'overlay/south-china-sea-dashes',2,'','',NULL,115,12,108,122,3,22,'{\"type\":\"MultiLineString\",\"coordinates\":[[[110,12],[111,11]]]}')")
            .execute(&admin.pool).await.unwrap();
        sqlx::query("INSERT INTO admin_bounds VALUES(3,108,122,3,22)")
            .execute(&admin.pool)
            .await
            .unwrap();
        assert!(admin
            .labels([108.0, 10.0, 122.0, 20.0], 2, "zh", 10)
            .await
            .unwrap()
            .is_empty());
        for level in [2, 4] {
            let lines = admin
                .boundaries([108.0, 10.0, 122.0, 20.0], level, 10)
                .await
                .unwrap();
            assert!(lines
                .iter()
                .any(|item| item.geometry["type"] == "MultiLineString"));
        }
    }
}
