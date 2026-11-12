#!/usr/bin/env python3
"""Import the ai:china map bundle without jq or other Python packages."""

import argparse
import filecmp
import json
import shutil
import sys
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def api(base, path, method="GET", payload=None):
    body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = Request(
        base.rstrip("/") + path,
        data=body,
        headers={"Content-Type": "application/json"} if body is not None else {},
        method=method,
    )
    try:
        with urlopen(request, timeout=60) as response:
            content = response.read()
    except HTTPError as error:
        detail = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"{method} {path}: HTTP {error.code}: {detail}") from error
    return json.loads(content) if content else None


def existing(items, **identity):
    return next(
        (item for item in items if all(item.get(key) == value for key, value in identity.items())),
        None,
    )


def save(base, endpoint, payload, current):
    if current:
        return api(base, f"{endpoint}/{current['id']}", "PUT", payload)
    return api(base, endpoint, "POST", payload)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_url", nargs="?", default="http://127.0.0.1:3000")
    parser.add_argument("data_dir", type=Path, help="Data path visible inside cangling-map")
    parser.add_argument("--skip-copy", action="store_true", help="Data is already on the target host")
    parser.add_argument("--host-data-dir", type=Path, help="Host path to validate when using --skip-copy")
    args = parser.parse_args()
    script_dir = Path(__file__).resolve().parent
    bundle = json.loads((script_dir / "bundle.json").read_text(encoding="utf-8"))
    if bundle.get("schema_version") != 1:
        raise RuntimeError("Unsupported bundle schema")

    if not args.skip_copy:
        args.data_dir.mkdir(parents=True, exist_ok=True)
    for item in bundle["catalog"]:
        name = item["data_file"]
        target = args.data_dir / name
        validation_target = (args.host_data_dir or args.data_dir) / name
        if not args.skip_copy:
            source = script_dir / "data" / name
            if not source.is_file():
                raise RuntimeError(f"Missing source data: {source}")
            for companion in source.parent.glob(source.stem + ".*"):
                destination = args.data_dir / companion.name
                if not destination.exists() or not filecmp.cmp(companion, destination, shallow=False):
                    shutil.copy2(companion, destination)
        if not validation_target.is_file():
            raise RuntimeError(f"Missing target data: {validation_target}")
        for extension in (".dbf", ".shx", ".prj"):
            if not validation_target.with_suffix(extension).is_file():
                raise RuntimeError(f"Missing Shapefile companion: {validation_target.with_suffix(extension)}")

    catalogs = api(args.base_url, "/api/catalog")
    styles = api(args.base_url, "/api/styles")
    layers = api(args.base_url, "/api/published-layers")
    groups = api(args.base_url, "/api/layer-groups")
    catalog_ids = {}
    style_ids = {}
    layer_ids = {}

    for item in bundle["catalog"]:
        payload = {key: item.get(key) for key in (
            "app_id", "catalog_path", "title", "description", "source_kind",
            "native_crs", "native_extent", "attribution",
        )}
        payload["source_config"] = dict(item["source_config"], path=str(args.data_dir / item["data_file"]))
        current = existing(catalogs, app_id=item["app_id"], catalog_path=item["catalog_path"])
        result = save(args.base_url, "/api/catalog", payload, current)
        catalog_ids[item["key"]] = result["id"]
        print(f"目录项: {item['app_id']}{item['catalog_path']}", flush=True)

    for item in bundle["styles"]:
        payload = {key: item.get(key) for key in (
            "app_id", "name", "title", "description", "geometry_type", "source_format", "source_text",
        )}
        current = existing(styles, app_id=item["app_id"], name=item["name"])
        result = save(args.base_url, "/api/styles", payload, current)
        style_ids[item["key"]] = result["id"]
        print(f"样式: {item['app_id']}:{item['name']}", flush=True)

    for item in bundle["layers"]:
        payload = {key: item.get(key) for key in (
            "app_id", "name", "title", "description", "supported_crs", "min_scale_denominator",
            "max_scale_denominator", "simplify_factor", "queryable", "enabled",
        )}
        payload["catalog_item_id"] = catalog_ids[item["catalog_key"]]
        payload["default_style_id"] = style_ids.get(item.get("default_style_key"))
        current = existing(layers, app_id=item["app_id"], name=item["name"])
        payload["id"] = current["id"] if current else ""
        result = save(args.base_url, "/api/published-layers", payload, current)
        layer_ids[item["key"]] = result["id"]
        print(f"发布图层: {item['app_id']}:{item['name']}", flush=True)

    group = bundle["group"]
    payload = {key: group.get(key) for key in ("app_id", "name", "title", "description", "enabled")}
    payload["members"] = [
        {
            "published_layer_id": layer_ids[member["layer_key"]],
            "style_id": style_ids.get(member.get("style_key")),
            "position": member["position"],
            "opacity": member["opacity"],
            "min_scale_denominator": member.get("min_scale_denominator"),
            "max_scale_denominator": member.get("max_scale_denominator"),
            "simplify_factor": member.get("simplify_factor"),
        }
        for member in group["members"]
    ]
    current = existing(groups, app_id=group["app_id"], name=group["name"])
    payload["id"] = current["id"] if current else ""
    save(args.base_url, "/api/layer-groups", payload, current)
    print(f"导入完成: {group['app_id']}:{group['name']}")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"导入失败: {error}", file=sys.stderr)
        sys.exit(1)
