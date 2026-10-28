#!/usr/bin/env python3
"""Build a versioned China administrative SQLite catalog from an OSM PBF.

Requires GDAL's OSM and GPKG drivers. Geofabrik extracts are ODbL 1.0 data;
keep the attribution and the source snapshot date with every publication.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()

LEVELS = {2: "country", 3: "special_region", 4: "province", 5: "prefecture",
          6: "county", 8: "township", 10: "village"}
TOLERANCE = {2: 0.01, 3: 0.005, 4: 0.005, 5: 0.002, 6: 0.001, 8: 0.0002, 10: 0.0001}


def osm_configuration(source: Path, destination: Path) -> None:
    configuration = source.read_text()
    configuration = configuration.replace("#tags_format=json", "tags_format=json")
    destination.write_text(configuration)


def extract(pbf: Path, temp: Path, config: Path) -> tuple[Path, Path]:
    env = {**os.environ, "OSM_CONFIG_FILE": str(config), "CPL_TMPDIR": str(temp)}
    boundaries = temp / "boundaries.gpkg"
    places = temp / "places.gpkg"
    subprocess.run(
        ["ogr2ogr", "-f", "GPKG", str(boundaries), str(pbf), "multipolygons",
         "-where", "boundary='administrative' AND admin_level IN ('2','3','4','5','6','8','10')",
         "-lco", "SPATIAL_INDEX=NO"],
        env=env, check=True,
    )
    subprocess.run(
        ["ogr2ogr", "-f", "GPKG", str(places), str(pbf), "points",
         "-where", "place IN ('village','hamlet')",
         "-lco", "SPATIAL_INDEX=NO"],
        env=env, check=True,
    )
    return boundaries, places


def extract_relation(source: Path, relation_id: int, temp: Path, config: Path) -> Path:
    destination = temp / f"relation-{relation_id}.gpkg"
    env = {**os.environ, "OSM_CONFIG_FILE": str(config), "CPL_TMPDIR": str(temp)}
    subprocess.run(
        ["ogr2ogr", "-f", "GPKG", str(destination), str(source), "multipolygons",
         "-where", f"osm_id='{relation_id}' AND boundary='administrative'",
         "-lco", "SPATIAL_INDEX=NO"],
        env=env, check=True,
    )
    return destination


def tags(feature: ogr.Feature) -> dict:
    raw = feature.GetField("other_tags")
    return json.loads(raw) if raw else {}


def insert_geometry(connection: sqlite3.Connection, feature: ogr.Feature) -> bool:
    try:
        level = int(feature.GetField("admin_level"))
    except (TypeError, ValueError):
        return False
    if level not in LEVELS:
        return False
    geometry = feature.GetGeometryRef()
    if geometry is None or geometry.IsEmpty():
        return False
    tag = tags(feature)
    name_zh = tag.get("name:zh") or tag.get("name:zh-Hans")
    name = name_zh or feature.GetField("name")
    if not name:
        return False
    relation_id = feature.GetField("osm_id")
    way_id = feature.GetField("osm_way_id")
    source_id = (f"relation/{relation_id}" if relation_id else
                 f"way/{way_id}" if way_id else None)
    if not source_id:
        return False
    # The regional PBF also contains small level-2 coastal restriction ways.
    # They are not countries; only China's full relation belongs to this tier.
    if level == 2 and source_id != "relation/270056":
        return False
    simplified = geometry.SimplifyPreserveTopology(TOLERANCE[level])
    if simplified is None or simplified.IsEmpty():
        simplified = geometry
    point = geometry.PointOnSurface()
    extent = geometry.GetEnvelope()  # minx, maxx, miny, maxy
    value = json.loads(simplified.ExportToJson())
    cursor = connection.execute(
        """INSERT OR IGNORE INTO admin_units
        (source_id, level, name, name_zh, name_en, longitude, latitude,
         min_lon, max_lon, min_lat, max_lat, geometry_json)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""",
        (source_id, level, feature.GetField("name") or name,
         name_zh, tag.get("name:en"), point.GetX(), point.GetY(),
         extent[0], extent[1], extent[2], extent[3], json.dumps(value, separators=(",", ":"))),
    )
    if not cursor.rowcount:
        return False
    connection.execute("INSERT INTO admin_bounds VALUES (?,?,?,?,?)",
                       (cursor.lastrowid, extent[0], extent[1], extent[2], extent[3]))
    return True


def insert_place(connection: sqlite3.Connection, feature: ogr.Feature) -> bool:
    geometry = feature.GetGeometryRef()
    if geometry is None or geometry.IsEmpty():
        return False
    tag = tags(feature)
    name_zh = tag.get("name:zh") or tag.get("name:zh-Hans")
    name = name_zh or feature.GetField("name")
    source_id = feature.GetField("osm_id")
    if not name or not source_id:
        return False
    longitude, latitude = geometry.GetX(), geometry.GetY()
    cursor = connection.execute(
        """INSERT OR IGNORE INTO admin_units
        (source_id, level, name, name_zh, name_en, longitude, latitude,
         min_lon, max_lon, min_lat, max_lat, geometry_json)
        VALUES (?,10,?,?,?,?,?,?,?,?,?,NULL)""",
        (f"node/{source_id}", feature.GetField("name") or name,
         name_zh, tag.get("name:en"), longitude, latitude,
         longitude, longitude, latitude, latitude),
    )
    if not cursor.rowcount:
        return False
    connection.execute("INSERT INTO admin_bounds VALUES (?,?,?,?,?)",
                       (cursor.lastrowid, longitude, longitude, latitude, latitude))
    return True


def build(pbf: Path, output: Path, snapshot: str, config_source: Path,
          country_osm: Path | None, country_snapshot: str | None,
          supplements: list[tuple[int, Path]], supplement_snapshot: str | None) -> dict[int, int]:
    if not pbf.is_file():
        raise FileNotFoundError(pbf)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="admin-osm-", dir=output.parent) as directory:
        temp = Path(directory)
        config = temp / "osmconf.ini"
        osm_configuration(config_source, config)
        boundaries, places = extract(pbf, temp, config)
        database = temp / "admin.sqlite"
        connection = sqlite3.connect(database)
        connection.executescript("""
            PRAGMA journal_mode=OFF;
            PRAGMA synchronous=OFF;
            CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
            CREATE TABLE admin_units (
                id INTEGER PRIMARY KEY,
                source_id TEXT NOT NULL UNIQUE,
                level INTEGER NOT NULL,
                name TEXT NOT NULL,
                name_zh TEXT,
                name_en TEXT,
                longitude REAL NOT NULL,
                latitude REAL NOT NULL,
                min_lon REAL NOT NULL,
                max_lon REAL NOT NULL,
                min_lat REAL NOT NULL,
                max_lat REAL NOT NULL,
                geometry_json TEXT
            ) STRICT;
            CREATE VIRTUAL TABLE admin_bounds USING rtree(
                id, min_lon, max_lon, min_lat, max_lat
            );
            CREATE INDEX admin_level_label ON admin_units(level, name);
        """)
        counts = {level: 0 for level in LEVELS}
        boundary_counts = {level: 0 for level in LEVELS}
        if country_osm is not None:
            country_source = ogr.Open(str(extract_relation(country_osm, 270056, temp, config)))
            country_features = list(country_source.GetLayer(0))
            if (len(country_features) != 1 or country_features[0].GetField("admin_level") != "2"
                    or not insert_geometry(connection, country_features[0])):
                raise ValueError("OSM relation 270056 did not yield one valid China boundary")
            counts[2] += 1
            boundary_counts[2] += 1
        for relation_id, source in supplements:
            supplement_source = ogr.Open(str(extract_relation(source, relation_id, temp, config)))
            features = list(supplement_source.GetLayer(0))
            if len(features) != 1:
                raise ValueError(f"OSM relation {relation_id} did not yield one boundary")
            level = int(features[0].GetField("admin_level"))
            if level not in (3, 4) or not insert_geometry(connection, features[0]):
                raise ValueError(f"OSM relation {relation_id} is not a unique level 3/4 boundary")
            counts[level] += 1
            boundary_counts[level] += 1
        boundary_source = ogr.Open(str(boundaries))
        for feature in boundary_source.GetLayer(0):
            if insert_geometry(connection, feature):
                level = int(feature.GetField("admin_level"))
                counts[level] += 1
                boundary_counts[level] += 1
        place_source = ogr.Open(str(places))
        for feature in place_source.GetLayer(0):
            if insert_place(connection, feature):
                counts[10] += 1
        # Some OSM special regions appear twice at levels 3 and 4 with the
        # same name and extent. Retain the level-3 relation as the province-tier
        # representation rather than drawing two coincident borders/labels.
        duplicates = [row[0] for row in connection.execute("""
            SELECT a.id FROM admin_units a JOIN admin_units b
              ON b.level=3 AND a.level=4 AND a.name=b.name
             AND abs(a.min_lon-b.min_lon)<0.000001
             AND abs(a.max_lon-b.max_lon)<0.000001
             AND abs(a.min_lat-b.min_lat)<0.000001
             AND abs(a.max_lat-b.max_lat)<0.000001
        """)]
        connection.executemany("DELETE FROM admin_bounds WHERE id=?", ((id,) for id in duplicates))
        connection.executemany("DELETE FROM admin_units WHERE id=?", ((id,) for id in duplicates))
        counts = {level: connection.execute(
            "SELECT count(*) FROM admin_units WHERE level=?", (level,)).fetchone()[0]
            for level in LEVELS}
        boundary_counts = {level: connection.execute(
            "SELECT count(*) FROM admin_units WHERE level=? AND geometry_json IS NOT NULL",
            (level,)).fetchone()[0] for level in LEVELS}
        with pbf.open("rb") as source_file:
            digest = hashlib.file_digest(source_file, "sha256").hexdigest()
        if country_osm is not None:
            with country_osm.open("rb") as country_file:
                country_digest = hashlib.file_digest(country_file, "sha256").hexdigest()
        else:
            country_digest = "not installed"
        supplement_sources = []
        for relation_id, source in supplements:
            with source.open("rb") as source_file:
                supplement_sources.append({
                    "relation": relation_id,
                    "url": f"https://api.openstreetmap.org/api/0.6/relation/{relation_id}/full",
                    "snapshot": supplement_snapshot,
                    "sha256": hashlib.file_digest(source_file, "sha256").hexdigest(),
                })
        connection.executemany("INSERT INTO metadata VALUES (?,?)", [
            ("schema_version", "1"),
            ("source", "OpenStreetMap contributors / Geofabrik China extract"),
            ("source_url", "https://download.geofabrik.de/asia/china-latest.osm.pbf"),
            ("source_snapshot", snapshot),
            ("source_sha256", digest),
            ("country_source_url", "https://api.openstreetmap.org/api/0.6/relation/270056/full")
            if country_osm is not None else ("country_source_url", "not installed"),
            ("country_source_snapshot", country_snapshot or "not installed"),
            ("country_source_sha256", country_digest),
            ("supplement_sources", json.dumps(supplement_sources, separators=(",", ":"))),
            ("license", "ODbL 1.0"),
            ("attribution", "© OpenStreetMap contributors"),
            ("crs", "EPSG:4326"),
            *((f"count_admin_{level}", str(count)) for level, count in counts.items()),
            *((f"count_boundary_{level}", str(count)) for level, count in boundary_counts.items()),
        ])
        connection.commit()
        connection.execute("VACUUM")
        assert connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        connection.close()
        os.replace(database, output)
    return counts


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("data/admin.sqlite"))
    parser.add_argument("--snapshot", required=True, help="extract UTC snapshot date, YYYY-MM-DD")
    parser.add_argument("--country-osm", type=Path,
                        help="OSM API XML for China relation 270056/full; fills the country boundary absent from Geofabrik China PBF")
    parser.add_argument("--country-snapshot", help="UTC date on which the country relation XML was fetched")
    parser.add_argument("--supplement-relation", action="append", default=[], metavar="ID=PATH",
                        help="full OSM API XML for a missing province or special region; repeatable")
    parser.add_argument("--supplement-snapshot", help="UTC date on which supplement XML files were fetched")
    parser.add_argument("--osmconf", type=Path, default=Path("/usr/local/share/gdal/osmconf.ini"))
    args = parser.parse_args()
    if args.country_osm is not None and not args.country_snapshot:
        parser.error("--country-snapshot is required with --country-osm")
    if args.supplement_relation and not args.supplement_snapshot:
        parser.error("--supplement-snapshot is required with --supplement-relation")
    try:
        supplements = [(int(value.split("=", 1)[0]), Path(value.split("=", 1)[1]))
                       for value in args.supplement_relation]
    except (ValueError, IndexError):
        parser.error("--supplement-relation must be ID=PATH")
    print(build(args.source, args.output, args.snapshot, args.osmconf,
                args.country_osm, args.country_snapshot, supplements,
                args.supplement_snapshot), flush=True)
