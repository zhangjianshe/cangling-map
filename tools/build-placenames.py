#!/usr/bin/env python3
"""Build the read-only Natural Earth place-name SQLite database.

Requires the GDAL Python bindings used by cangling-map. The source ZIP may be
supplied locally to make the build independent of network availability.
"""

import argparse
import hashlib
import os
import sqlite3
import tempfile
import urllib.request
import zipfile
from pathlib import Path

from osgeo import ogr

ogr.UseExceptions()

SOURCE_URL = "https://naturalearth.s3.amazonaws.com/10m_cultural/ne_10m_populated_places.zip"
SOURCE_NAME = "ne_10m_populated_places"


def build(source: Path, output: Path) -> int:
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".placenames-", dir=output.parent) as temp:
        temp = Path(temp)
        if source.is_file():
            archive = source
        else:
            archive = temp / "source.zip"
            urllib.request.urlretrieve(SOURCE_URL, archive)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        with zipfile.ZipFile(archive) as zipped:
            zipped.extractall(temp / "source")
        source_dir = temp / "source"
        version = (source_dir / f"{SOURCE_NAME}.VERSION.txt").read_text().strip()
        dataset = ogr.Open(str(source_dir / f"{SOURCE_NAME}.shp"))
        if dataset is None:
            raise RuntimeError("GDAL cannot open the Natural Earth shapefile")
        database = temp / "placenames.sqlite"
        connection = sqlite3.connect(database)
        connection.executescript(
            """
            PRAGMA page_size=4096;
            CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
            CREATE TABLE places (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                name_zh TEXT,
                name_en TEXT,
                longitude REAL NOT NULL CHECK (longitude BETWEEN -180 AND 180),
                latitude REAL NOT NULL CHECK (latitude BETWEEN -90 AND 90),
                feature_class TEXT NOT NULL,
                label_rank INTEGER NOT NULL,
                min_zoom REAL NOT NULL,
                population INTEGER,
                source TEXT NOT NULL
            ) STRICT;
            CREATE VIRTUAL TABLE place_bounds USING rtree(
                id, min_lon, max_lon, min_lat, max_lat
            );
            CREATE INDEX places_zoom_rank ON places(min_zoom, label_rank);
            """
        )
        count = 0
        for feature in dataset.GetLayer():
            point = feature.GetGeometryRef()
            if point is None:
                continue
            longitude, latitude = point.GetX(), point.GetY()
            name = feature.GetField("NAME")
            if not name or not (-180 <= longitude <= 180 and -90 <= latitude <= 90):
                continue
            identifier = feature.GetField("NE_ID")
            connection.execute(
                """INSERT INTO places
                   (id,name,name_zh,name_en,longitude,latitude,feature_class,
                    label_rank,min_zoom,population,source)
                   VALUES (?,?,?,?,?,?,?,?,?,?,?)""",
                (
                    identifier,
                    name,
                    feature.GetField("NAME_ZH") or None,
                    feature.GetField("NAME_EN") or None,
                    longitude,
                    latitude,
                    feature.GetField("FEATURECLA") or "Populated place",
                    feature.GetField("LABELRANK") if feature.GetField("LABELRANK") is not None else 10,
                    feature.GetField("MIN_ZOOM") if feature.GetField("MIN_ZOOM") is not None else 0.0,
                    feature.GetField("POP_MAX"),
                    "natural-earth-10m-populated-places",
                ),
            )
            connection.execute(
                "INSERT INTO place_bounds VALUES (?,?,?,?,?)",
                (identifier, longitude, longitude, latitude, latitude),
            )
            count += 1
        connection.executemany(
            "INSERT INTO metadata(key,value) VALUES (?,?)",
            [
                ("schema_version", "1"),
                ("source", "Natural Earth 10m Populated Places"),
                ("source_url", SOURCE_URL),
                ("source_version", version),
                ("source_sha256", digest),
                ("license", "public domain"),
                ("attribution", "Natural Earth"),
                ("crs", "EPSG:4326"),
                ("feature_count", str(count)),
            ],
        )
        connection.commit()
        connection.execute("VACUUM")
        integrity = connection.execute("PRAGMA integrity_check").fetchone()[0]
        connection.close()
        if integrity != "ok":
            raise RuntimeError(f"SQLite integrity check failed: {integrity}")
        os.replace(database, output)
    return count


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="local Natural Earth ZIP")
    parser.add_argument(
        "--output", type=Path, default=Path("data/placenames.sqlite")
    )
    args = parser.parse_args()
    print(f"Imported {build(args.source or Path(), args.output)} places into {args.output}")
