#!/usr/bin/env python3
"""Build a reviewable admin.sqlite candidate from GeoCode's GitHub mirror.

The mirror says its polygons originate from Tianditu, but it does not publish
the original download's date or checksum. Keep this provenance distinct from
an independently verified official download. The existing GMT China sea-dash
overlay and OSM township/village data are preserved.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3
import tempfile

from osgeo import ogr

ogr.UseExceptions()

REPOSITORY = "https://github.com/zzhonglei/GeoCode-Release"
LAYERS = (
    ("china_province.geojson", 4, 34, 0.004),
    ("china_city.geojson", 5, 375, 0.001),
    ("china_county.geojson", 6, 2891, 0.0005),
)


def read_layer(path: Path, expected_count: int) -> tuple[list, str]:
    raw = path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    data = json.loads(raw)
    if data.get("type") != "FeatureCollection":
        raise ValueError(f"{path}: expected a GeoJSON FeatureCollection")
    crs = data.get("crs", {}).get("properties", {}).get("name")
    if crs != "urn:ogc:def:crs:OGC:1.3:CRS84":
        raise ValueError(f"{path}: unexpected CRS {crs!r}")
    features = data.get("features", [])
    if len(features) != expected_count:
        raise ValueError(f"{path}: expected {expected_count} features, found {len(features)}")
    units = []
    seen = set()
    for feature in features:
        properties = feature.get("properties", {})
        code, name = properties.get("adcode"), properties.get("name")
        if not isinstance(code, str) or len(code) != 6 or not code.isdecimal():
            raise ValueError(f"{path}: invalid adcode {code!r}")
        if code in seen or not isinstance(name, str) or not name.strip():
            raise ValueError(f"{path}: duplicate code or empty name at {code}")
        seen.add(code)
        geometry = ogr.CreateGeometryFromJson(json.dumps(feature.get("geometry")))
        if geometry is None or geometry.IsEmpty() or not geometry.IsValid():
            raise ValueError(f"{path}: invalid geometry at {code}")
        if ogr.GT_Flatten(geometry.GetGeometryType()) not in (ogr.wkbPolygon, ogr.wkbMultiPolygon):
            raise ValueError(f"{path}: non-polygon geometry at {code}")
        units.append((code, name.strip(), geometry))
    return units, digest


def simplified(geometry: ogr.Geometry, tolerance: float) -> ogr.Geometry:
    result = geometry.SimplifyPreserveTopology(tolerance)
    if result is None or result.IsEmpty() or not result.IsValid():
        raise ValueError("simplification produced invalid geometry")
    return result


def country_from_provinces(provinces: list) -> ogr.Geometry:
    polygons = ogr.Geometry(ogr.wkbMultiPolygon)
    for _, _, geometry in provinces:
        if ogr.GT_Flatten(geometry.GetGeometryType()) == ogr.wkbPolygon:
            polygons.AddGeometry(geometry)
        else:
            for index in range(geometry.GetGeometryCount()):
                polygons.AddGeometry(geometry.GetGeometryRef(index))
    merged = polygons.UnionCascaded()
    if merged is None or merged.IsEmpty() or not merged.IsValid():
        raise ValueError("could not dissolve province polygons into a country")
    extent = merged.GetEnvelope()
    if not (extent[0] < 75 and extent[1] > 130 and extent[2] < 5 and extent[3] > 50):
        raise ValueError(f"incomplete national coverage: {extent}")
    return simplified(merged, 0.03)


def insert_unit(connection: sqlite3.Connection, code: str, level: int,
                name: str, geometry: ogr.Geometry) -> None:
    extent = geometry.GetEnvelope()
    point = geometry.PointOnSurface()
    cursor = connection.execute(
        """INSERT INTO admin_units
        (source_id,level,name,name_zh,name_en,longitude,latitude,
         min_lon,max_lon,min_lat,max_lat,geometry_json)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""",
        (f"geocode-mirror/{level}/{code}", level, name, name, None,
         point.GetX(), point.GetY(), extent[0], extent[1], extent[2], extent[3],
         json.dumps(json.loads(geometry.ExportToJson()), separators=(",", ":"))),
    )
    connection.execute("INSERT INTO admin_bounds VALUES (?,?,?,?,?)",
                       (cursor.lastrowid, extent[0], extent[1], extent[2], extent[3]))


def install(connection: sqlite3.Connection, source_dir: Path, commit: str) -> None:
    layers = {}
    hashes = {}
    for filename, level, expected_count, _ in LAYERS:
        units, digest = read_layer(source_dir / filename, expected_count)
        layers[level] = units
        hashes[filename] = digest
        print(f"validated {filename}: {len(units)} features, sha256 {digest}", flush=True)

    province_codes = {code[:2] for code, _, _ in layers[4]}
    if len(province_codes) != 34 or "71" not in province_codes:
        raise ValueError("province coverage must include 34 unique provinces including Taiwan")
    for level in (5, 6):
        missing = {code[:2] for code, _, _ in layers[level]} - province_codes
        if missing:
            raise ValueError(f"level {level}: unknown province prefixes {missing}")
    if not any(code.startswith("71") and code != "710000" for code, _, _ in layers[6]):
        raise ValueError("Taiwan county/city subdivisions are missing")
    country = country_from_provinces(layers[4])

    old_ids = [row[0] for row in connection.execute(
        "SELECT id FROM admin_units WHERE level IN (2,3,4,5,6) "
        "AND source_id NOT LIKE 'overlay/%'"
    )]
    connection.executemany("DELETE FROM admin_bounds WHERE id=?", ((row,) for row in old_ids))
    connection.executemany("DELETE FROM admin_units WHERE id=?", ((row,) for row in old_ids))
    insert_unit(connection, "100000", 2, "中华人民共和国", country)
    for _, level, _, tolerance in LAYERS:
        for code, name, geometry in layers[level]:
            insert_unit(connection, code, level, name, simplified(geometry, tolerance))

    connection.execute("DELETE FROM metadata WHERE key LIKE 'ngcc_%' "
                       "OR key LIKE 'taiwan_preview_%' OR key LIKE 'geocode_mirror_%' "
                       "OR key LIKE 'country_source_%' OR key='supplement_sources'")
    values = {
        "source": "GeoCode GitHub mirror (country/province/city/county); OpenStreetMap (township/village)",
        "geocode_mirror_repository": REPOSITORY,
        "geocode_mirror_commit": commit,
        "geocode_mirror_scope": "country-dissolved,province,city,county",
        "geocode_mirror_provenance":
            "GitHub mirror claims Tianditu origin; original snapshot and license unverified",
        "geocode_mirror_sha256": json.dumps(hashes, sort_keys=True),
        "attribution": "GeoCode GitHub mirror (claims Tianditu origin; original snapshot unverified); "
                       "GMT China sea dashes; © OpenStreetMap contributors (township/village)",
        "license": "Mixed: GeoCode repository MIT notice; underlying Tianditu source terms "
                   "unverified; GMT China source terms; OSM ODbL 1.0",
    }
    for level in (2, 3, 4, 5, 6, 8, 10):
        values[f"count_admin_{level}"] = str(connection.execute(
            "SELECT count(*) FROM admin_units WHERE level=?", (level,)
        ).fetchone()[0])
        values[f"count_boundary_{level}"] = str(connection.execute(
            "SELECT count(*) FROM admin_units WHERE level=? AND geometry_json IS NOT NULL",
            (level,),
        ).fetchone()[0])
    connection.executemany("INSERT OR REPLACE INTO metadata VALUES (?,?)", values.items())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, default=Path("data/admin.sqlite"))
    parser.add_argument("--output", type=Path,
                        default=Path("data/admin.sqlite.github-candidate"))
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument("--source-commit", required=True,
                        help="40-character commit SHA pinning the three GeoJSON files")
    args = parser.parse_args()
    if len(args.source_commit) != 40 or any(ch not in "0123456789abcdef" for ch in args.source_commit):
        parser.error("--source-commit must be a lowercase 40-character hexadecimal SHA")
    if not args.database.is_file():
        parser.error(f"base database does not exist: {args.database}")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="geocode-admin-", dir=args.output.parent) as folder:
        temporary = Path(folder) / "admin.sqlite"
        shutil.copyfile(args.database, temporary)
        connection = sqlite3.connect(temporary)
        try:
            connection.execute("BEGIN IMMEDIATE")
            install(connection, args.source_dir, args.source_commit)
            connection.commit()
            if connection.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
                raise ValueError("SQLite integrity check failed")
        except Exception:
            connection.rollback()
            raise
        finally:
            connection.close()
        temporary.replace(args.output)
    print(f"candidate written to {args.output}", flush=True)


if __name__ == "__main__":
    main()
