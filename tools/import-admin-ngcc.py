#!/usr/bin/env python3
"""Install NGCC 1:1M public BOUA boundaries into an admin.sqlite catalog.

Download and extract the 77 official GDB sheets first. This tool never fetches
data from a third-party mirror. It replaces only the country, province and
prefecture tiers; county and smaller OSM tiers remain in the catalog.

An optional GMT dash-line file can be installed independently. Keep its
provenance separate: a community-derived GMT file is not an official release.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import sqlite3
import tempfile

from osgeo import ogr, osr

ogr.UseExceptions()
osr.UseExceptions()

OFFICIAL_URL = "https://www.webmap.cn/commres.do?method=result100W"
DASH_SOURCE_ID = "overlay/south-china-sea-dashes"
PROVINCE_PREFIXES = {
    "11", "12", "13", "14", "15", "21", "22", "23", "31", "32", "33", "34", "35",
    "36", "37", "41", "42", "43", "44", "45", "46", "50", "51", "52", "53", "54",
    "61", "62", "63", "64", "65", "71", "81", "82",
}
PROVINCE_NAMES = {
    "11": "北京市", "12": "天津市", "13": "河北省", "14": "山西省", "15": "内蒙古自治区",
    "21": "辽宁省", "22": "吉林省", "23": "黑龙江省", "31": "上海市", "32": "江苏省",
    "33": "浙江省", "34": "安徽省", "35": "福建省", "36": "江西省", "37": "山东省",
    "41": "河南省", "42": "湖北省", "43": "湖南省", "44": "广东省", "45": "广西壮族自治区",
    "46": "海南省", "50": "重庆市", "51": "四川省", "52": "贵州省", "53": "云南省",
    "54": "西藏自治区", "61": "陕西省", "62": "甘肃省", "63": "青海省", "64": "宁夏回族自治区",
    "65": "新疆维吾尔自治区", "71": "台湾省", "81": "香港特别行政区", "82": "澳门特别行政区",
}
TOLERANCE = {2: 0.01, 4: 0.005, 5: 0.002}


def sources_under(directory: Path) -> list[Path]:
    if directory.suffix.lower() in {".gdb", ".gpkg", ".geojson", ".json"}:
        return [directory]
    sources = []
    for root, dirs, files in os.walk(directory):
        for name in list(dirs):
            if name.lower().endswith(".gdb"):
                sources.append(Path(root) / name)
                dirs.remove(name)
        for name in files:
            if name.lower().endswith((".gpkg", ".geojson")):
                sources.append(Path(root) / name)
    return sorted(sources)


def normalized_polygon(geometry: ogr.Geometry) -> ogr.Geometry | None:
    if geometry is None or geometry.IsEmpty():
        return None
    geometry = geometry.Clone()
    if not geometry.IsValid():
        geometry = geometry.MakeValid()
    kind = ogr.GT_Flatten(geometry.GetGeometryType())
    if kind == ogr.wkbPolygon:
        return geometry
    if kind == ogr.wkbMultiPolygon:
        return geometry
    return None


def collect_polygons(directory: Path) -> dict[str, tuple[str, list[ogr.Geometry]]]:
    groups: dict[str, tuple[str, list[ogr.Geometry]]] = {}
    paths = sources_under(directory)
    if not paths:
        raise ValueError(f"no GDB/GeoPackage sources found below {directory}")
    if all(path.suffix.lower() == ".gdb" for path in paths) and len(paths) < 77:
        raise ValueError(f"found only {len(paths)} of the 77 official GDB sheets")
    for path in paths:
        dataset = ogr.Open(str(path))
        if dataset is None:
            raise ValueError(f"cannot open {path}")
        layers = [dataset.GetLayerByIndex(i) for i in range(dataset.GetLayerCount())]
        layers = [layer for layer in layers if layer.GetName().upper().endswith("BOUA")]
        if not layers:
            raise ValueError(f"{path} has no BOUA administrative area layer")
        for layer in layers:
            names = {layer.GetLayerDefn().GetFieldDefn(i).GetName().upper(): i
                     for i in range(layer.GetLayerDefn().GetFieldCount())}
            if not {"PAC", "NAME"}.issubset(names):
                raise ValueError(f"{path}:{layer.GetName()} lacks PAC or NAME")
            source_srs = layer.GetSpatialRef()
            if source_srs is None:
                raise ValueError(f"{path}:{layer.GetName()} has no CRS")
            source_srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            target_srs = osr.SpatialReference()
            target_srs.ImportFromEPSG(4326)
            target_srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            transform = osr.CoordinateTransformation(source_srs, target_srs)
            for feature in layer:
                raw_code = str(feature.GetField(names["PAC"]) or "").strip()
                if not raw_code.isdecimal():
                    continue
                code = raw_code.zfill(6)
                if len(code) != 6 or code[:2] not in PROVINCE_PREFIXES:
                    continue
                name = str(feature.GetField(names["NAME"]) or "").strip()
                if not name:
                    continue
                geometry = normalized_polygon(feature.GetGeometryRef())
                if geometry is None:
                    continue
                geometry.Transform(transform)
                if code not in groups:
                    groups[code] = (name, [])
                elif groups[code][0] != name:
                    raise ValueError(f"conflicting names for PAC {code}: {groups[code][0]} / {name}")
                groups[code][1].append(geometry)
        print(f"read {path}", flush=True)
    return groups


def dissolve(parts: list[ogr.Geometry], tolerance: float) -> ogr.Geometry:
    multi = ogr.Geometry(ogr.wkbMultiPolygon)
    for part in parts:
        if ogr.GT_Flatten(part.GetGeometryType()) == ogr.wkbPolygon:
            multi.AddGeometry(part)
        else:
            for index in range(part.GetGeometryCount()):
                multi.AddGeometry(part.GetGeometryRef(index))
    merged = multi.UnionCascaded()
    if merged is None or merged.IsEmpty():
        raise ValueError("polygon union produced an empty geometry")
    simplified = merged.SimplifyPreserveTopology(tolerance)
    return simplified if simplified is not None and not simplified.IsEmpty() else merged


def insert_unit(connection: sqlite3.Connection, source_id: str, level: int,
                name: str, geometry: ogr.Geometry) -> None:
    extent = geometry.GetEnvelope()
    point = geometry.PointOnSurface()
    cursor = connection.execute(
        """INSERT INTO admin_units
        (source_id,level,name,name_zh,name_en,longitude,latitude,
         min_lon,max_lon,min_lat,max_lat,geometry_json)
        VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""",
        (source_id, level, name, name or None, None, point.GetX(), point.GetY(),
         extent[0], extent[1], extent[2], extent[3],
         json.dumps(json.loads(geometry.ExportToJson()), separators=(",", ":"))),
    )
    connection.execute("INSERT INTO admin_bounds VALUES (?,?,?,?,?)",
                       (cursor.lastrowid, extent[0], extent[1], extent[2], extent[3]))


def install_official(connection: sqlite3.Connection, groups: dict,
                     snapshot: str) -> dict[int, int]:
    # BOUA commonly contains county polygons. A direct province/city record is
    # skipped whenever more detailed PAC records cover the same region.
    codes = set(groups)
    leaves = {
        code for code in codes
        if not ((code[2:] == "0000" and any(
            other != code and other.startswith(code[:2]) for other in codes))
            or (code[4:] == "00" and any(
                other != code and other.startswith(code[:4]) for other in codes)))
    }
    province_parts: dict[str, list[ogr.Geometry]] = {}
    city_parts: dict[str, list[ogr.Geometry]] = {}
    for code in leaves:
        parts = groups[code][1]
        province_parts.setdefault(code[:2], []).extend(parts)
        if code[2:] != "0000":
            city_parts.setdefault(code[:4] + "00", []).extend(parts)
    if len(province_parts) != 34 or "71" not in province_parts or len(city_parts) < 250:
        raise ValueError(
            f"incomplete NGCC coverage: {len(province_parts)} provinces (including Taiwan required), "
            f"{len(city_parts)} prefectures; expected 34 and at least 250. "
            "Supply all 77 sheets before replacing the catalog."
        )
    provinces = []
    cities = []
    for prefix, parts in sorted(province_parts.items()):
        provinces.append((prefix + "0000", PROVINCE_NAMES[prefix],
                          dissolve(parts, TOLERANCE[4])))
    for code, parts in sorted(city_parts.items()):
        # The legacy OSM label points are kept separately. NGCC supplies the
        # city boundary geometry; blank name prevents duplicate city labels.
        cities.append((code, dissolve(parts, TOLERANCE[5])))
    country_parts = [geometry for _, _, geometry in provinces]
    country = dissolve(country_parts, TOLERANCE[2])
    country_extent = country.GetEnvelope()
    if not (country_extent[0] < 75 and country_extent[1] > 130
            and country_extent[2] < 10 and country_extent[3] > 50):
        raise ValueError(f"official country coverage is incomplete: {country_extent}")
    ids = [row[0] for row in connection.execute(
        "SELECT id FROM admin_units WHERE level IN (2,3,4) AND source_id NOT LIKE 'overlay/%'"
    )]
    connection.executemany("DELETE FROM admin_bounds WHERE id=?", ((id_,) for id_ in ids))
    connection.executemany("DELETE FROM admin_units WHERE id=?", ((id_,) for id_ in ids))
    connection.execute("UPDATE admin_units SET geometry_json=NULL WHERE level=5 AND source_id NOT LIKE 'ngcc/%'")
    old_cities = [row[0] for row in connection.execute(
        "SELECT id FROM admin_units WHERE level=5 AND source_id LIKE 'ngcc/%'"
    )]
    connection.executemany("DELETE FROM admin_bounds WHERE id=?", ((id_,) for id_ in old_cities))
    connection.executemany("DELETE FROM admin_units WHERE id=?", ((id_,) for id_ in old_cities))
    insert_unit(connection, "ngcc/2/100000", 2, "中华人民共和国", country)
    for code, name, geometry in provinces:
        insert_unit(connection, f"ngcc/4/{code}", 4, name, geometry)
    for code, geometry in cities:
        insert_unit(connection, f"ngcc/5/{code}", 5, "", geometry)
    connection.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('ngcc_source_snapshot',?)", (snapshot,)
    )
    connection.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('ngcc_source_url',?)", (OFFICIAL_URL,)
    )
    connection.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('ngcc_scale','1:1000000')"
    )
    connection.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('ngcc_scope','country,province,prefecture')"
    )
    connection.execute("DELETE FROM metadata WHERE key IN ('taiwan_preview_source','taiwan_preview_source_url')")
    return {2: 1, 4: len(provinces), 5: len(cities)}


def install_dashes(connection: sqlite3.Connection, path: Path,
                   count: int) -> None:
    source = ogr.Open(str(path))
    if source is None:
        raise ValueError(f"cannot open dash-line source {path}")
    lines = []
    for feature in source.GetLayer(0):
        geometry = feature.GetGeometryRef()
        if geometry is None or ogr.GT_Flatten(geometry.GetGeometryType()) != ogr.wkbLineString:
            continue
        if geometry.GetPointCount() >= 2:
            lines.append(geometry.Clone())
    if len(lines) != 10 or count not in (9, 10):
        raise ValueError(f"expected ten distinct GMT line segments; found {len(lines)}")
    multi = ogr.Geometry(ogr.wkbMultiLineString)
    for line in lines[:count]:
        multi.AddGeometry(line)
    old = connection.execute(
        "SELECT id FROM admin_units WHERE source_id=?", (DASH_SOURCE_ID,)
    ).fetchone()
    if old:
        connection.execute("DELETE FROM admin_bounds WHERE id=?", old)
        connection.execute("DELETE FROM admin_units WHERE id=?", old)
    insert_unit(connection, DASH_SOURCE_ID, 2, "", multi)
    connection.execute("INSERT OR REPLACE INTO metadata VALUES ('dash_source',?)",
                       ("GMT China community derivative of NGCC 2021",))
    connection.execute("INSERT OR REPLACE INTO metadata VALUES ('dash_source_url',?)",
                       ("https://github.com/gmt-china/china-geospatial-data",))
    connection.execute("INSERT OR REPLACE INTO metadata VALUES ('dash_segments',?)",
                       (str(count),))


def install_taiwan_preview(connection: sqlite3.Connection, path: Path) -> None:
    """Install the Taiwan polygon from GMT China's NGCC-derived preview file."""
    source = ogr.Open(str(path))
    if source is None:
        raise ValueError(f"cannot open Taiwan preview source {path}")
    parts = []
    found = False
    for feature in source.GetLayer(0):
        code = feature.GetField("CODE")
        if code == 710000:
            found = True
        elif found and code is not None:
            break
        if found:
            geometry = normalized_polygon(feature.GetGeometryRef())
            if geometry is not None:
                parts.append(geometry)
    if not parts:
        raise ValueError("Taiwan province polygon is absent from the preview file")
    geometry = dissolve(parts, TOLERANCE[4])
    extent = geometry.GetEnvelope()
    if not (119 <= extent[0] <= 122 and 122 <= extent[1] <= 125
            and 21 <= extent[2] <= 23 and 25 <= extent[3] <= 27):
        raise ValueError(f"Taiwan preview coverage is incomplete: {extent}")
    rows = connection.execute(
        "SELECT id FROM admin_units WHERE level IN (3,4) AND "
        "(source_id='ngcc-derived/4/710000' OR source_id='ngcc/4/710000' "
        "OR name IN ('台湾省','臺灣省','Taiwan'))"
    ).fetchall()
    connection.executemany("DELETE FROM admin_bounds WHERE id=?", rows)
    connection.executemany("DELETE FROM admin_units WHERE id=?", rows)
    insert_unit(connection, "ngcc-derived/4/710000", 4, "台湾省", geometry)
    connection.execute("INSERT OR REPLACE INTO metadata VALUES ('taiwan_preview_source',?)",
                       ("GMT China community derivative of NGCC 2021",))
    connection.execute("INSERT OR REPLACE INTO metadata VALUES ('taiwan_preview_source_url',?)",
                       ("https://github.com/gmt-china/china-geospatial-data",))


def update_counts_and_attribution(connection: sqlite3.Connection) -> None:
    for level in (2, 3, 4, 5, 6, 8, 10):
        count = connection.execute("SELECT count(*) FROM admin_units WHERE level=?", (level,)).fetchone()[0]
        boundaries = connection.execute(
            "SELECT count(*) FROM admin_units WHERE level=? AND geometry_json IS NOT NULL", (level,)
        ).fetchone()[0]
        connection.execute("INSERT OR REPLACE INTO metadata VALUES (?,?)",
                           (f"count_admin_{level}", str(count)))
        connection.execute("INSERT OR REPLACE INTO metadata VALUES (?,?)",
                           (f"count_boundary_{level}", str(boundaries)))
    official = bool(connection.execute(
        "SELECT 1 FROM metadata WHERE key='ngcc_source_snapshot'"
    ).fetchone())
    preview = bool(connection.execute(
        "SELECT 1 FROM metadata WHERE key IN ('dash_source','taiwan_preview_source') LIMIT 1"
    ).fetchone())
    sources = ["© OpenStreetMap contributors"]
    if official:
        sources.insert(0, "引自全国地理信息资源目录服务系统 www.webmap.cn")
    if connection.execute("SELECT 1 FROM metadata WHERE key='dash_source'").fetchone():
        sources.append("南海断续线：GMT China（基于国家基础地理信息中心数据）")
    if connection.execute("SELECT 1 FROM metadata WHERE key='taiwan_preview_source'").fetchone():
        sources.append("台湾省界：GMT China（基于国家基础地理信息中心数据）")
    connection.execute("INSERT OR REPLACE INTO metadata VALUES ('attribution',?)",
                       ("；".join(sources),))
    if official or preview:
        connection.execute(
            "INSERT OR REPLACE INTO metadata VALUES ('license',?)",
            ("Mixed sources: OSM ODbL 1.0; NGCC/GMT China data subject to source terms",),
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, default=Path("data/admin.sqlite"))
    parser.add_argument("--output", type=Path, default=Path("data/admin.sqlite"))
    parser.add_argument("--source-dir", type=Path,
                        help="directory of all 77 extracted official 2021 GDB sheets")
    parser.add_argument("--snapshot", help="version/date from the official data package")
    parser.add_argument("--dash-gmt", type=Path,
                        help="GMT China ten-dash-line.gmt, derived from NGCC data")
    parser.add_argument("--dash-count", type=int, choices=(9, 10), default=9)
    parser.add_argument("--taiwan-preview-gmt", type=Path,
                        help="temporary Taiwan polygon from GMT China's NGCC-derived CN-border-La.gmt")
    args = parser.parse_args()
    if not args.source_dir and not args.dash_gmt and not args.taiwan_preview_gmt:
        parser.error("--source-dir, --dash-gmt and/or --taiwan-preview-gmt is required")
    if args.source_dir and not args.snapshot:
        parser.error("--snapshot is required with --source-dir")
    groups = collect_polygons(args.source_dir) if args.source_dir else None
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ngcc-admin-", dir=args.output.parent) as directory:
        destination = Path(directory) / "admin.sqlite"
        shutil.copyfile(args.database, destination)
        connection = sqlite3.connect(destination)
        try:
            connection.execute("BEGIN IMMEDIATE")
            if groups is not None:
                print("official units:", install_official(connection, groups, args.snapshot), flush=True)
            if args.dash_gmt:
                install_dashes(connection, args.dash_gmt, args.dash_count)
            if args.taiwan_preview_gmt and groups is None:
                install_taiwan_preview(connection, args.taiwan_preview_gmt)
            update_counts_and_attribution(connection)
            connection.commit()
            if connection.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
                raise ValueError("SQLite integrity check failed")
        except Exception:
            connection.rollback()
            raise
        finally:
            connection.close()
        os.replace(destination, args.output)
    print(f"installed {args.output}", flush=True)


if __name__ == "__main__":
    main()
