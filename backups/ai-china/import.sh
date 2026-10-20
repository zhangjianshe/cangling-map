#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
base_url="${1:-http://127.0.0.1:3000}"
data_dir="${2:-/opt/cis/core/geoserver/data/china}"
bundle="$script_dir/bundle.json"

for command in curl jq; do
  command -v "$command" >/dev/null || { echo "缺少命令: $command" >&2; exit 1; }
done
[[ -f "$bundle" ]] || { echo "找不到 $bundle" >&2; exit 1; }
[[ "$(jq -r '.schema_version' "$bundle")" == "1" ]] || { echo "不支持的备份格式" >&2; exit 1; }
[[ -d "$script_dir/data" ]] || { echo "找不到数据目录: $script_dir/data" >&2; exit 1; }

while IFS= read -r data_file; do
  [[ -f "$script_dir/data/$data_file" ]] || {
    echo "缺少迁移数据: $script_dir/data/$data_file" >&2
    echo "请先运行 export.sh，或从发布资产/对象存储恢复 data/ 目录。" >&2
    exit 1
  }
done < <(jq -r '.catalog[].data_file' "$bundle")

mkdir -p "$data_dir"
for source_file in "$script_dir"/data/*; do
  [[ -f "$source_file" ]] || continue
  target_file="$data_dir/$(basename "$source_file")"
  if [[ -f "$target_file" ]] && cmp -s "$source_file" "$target_file"; then
    continue
  fi
  cp -p "$source_file" "$target_file"
done

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
printf '{}' > "$tmp_dir/catalog-map.json"
printf '{}' > "$tmp_dir/style-map.json"
printf '{}' > "$tmp_dir/layer-map.json"

refresh() {
  curl -fsS "$base_url/api/catalog" -o "$tmp_dir/catalog-current.json"
  curl -fsS "$base_url/api/styles" -o "$tmp_dir/styles-current.json"
  curl -fsS "$base_url/api/published-layers" -o "$tmp_dir/layers-current.json"
  curl -fsS "$base_url/api/layer-groups" -o "$tmp_dir/groups-current.json"
}
refresh

while IFS= read -r encoded; do
  item="$(printf '%s' "$encoded" | base64 -d)"
  key="$(jq -r '.key' <<<"$item")"
  app="$(jq -r '.app_id' <<<"$item")"
  path="$(jq -r '.catalog_path' <<<"$item")"
  data_file="$(jq -r '.data_file' <<<"$item")"
  existing_id="$(jq -r --arg app "$app" --arg path "$path" '.[] | select(.app_id==$app and .catalog_path==$path) | .id' "$tmp_dir/catalog-current.json" | head -1)"
  payload="$(jq --arg data_path "$data_dir/$data_file" '{id:null,app_id,catalog_path,title,description,source_kind,source_config:(.source_config+{path:$data_path}),native_crs,native_extent,attribution}' <<<"$item")"
  if [[ -n "$existing_id" ]]; then
    result="$(curl -fsS -X PUT -H 'Content-Type: application/json' --data-binary "$payload" "$base_url/api/catalog/$existing_id")"
  else
    result="$(curl -fsS -X POST -H 'Content-Type: application/json' --data-binary "$payload" "$base_url/api/catalog")"
  fi
  new_id="$(jq -r '.id' <<<"$result")"
  jq --arg key "$key" --arg id "$new_id" '. + {($key):$id}' "$tmp_dir/catalog-map.json" > "$tmp_dir/map.next"
  mv "$tmp_dir/map.next" "$tmp_dir/catalog-map.json"
  echo "目录项: $app$path"
done < <(jq -r '.catalog[] | @base64' "$bundle")

while IFS= read -r encoded; do
  item="$(printf '%s' "$encoded" | base64 -d)"
  key="$(jq -r '.key' <<<"$item")"
  app="$(jq -r '.app_id' <<<"$item")"
  name="$(jq -r '.name' <<<"$item")"
  existing_id="$(jq -r --arg app "$app" --arg name "$name" '.[] | select(.app_id==$app and .name==$name) | .id' "$tmp_dir/styles-current.json" | head -1)"
  payload="$(jq '{app_id,name,title,description,geometry_type,source_format,source_text}' <<<"$item")"
  if [[ -n "$existing_id" ]]; then
    result="$(curl -fsS -X PUT -H 'Content-Type: application/json' --data-binary "$payload" "$base_url/api/styles/$existing_id")"
  else
    result="$(curl -fsS -X POST -H 'Content-Type: application/json' --data-binary "$payload" "$base_url/api/styles")"
  fi
  new_id="$(jq -r '.id' <<<"$result")"
  jq --arg key "$key" --arg id "$new_id" '. + {($key):$id}' "$tmp_dir/style-map.json" > "$tmp_dir/map.next"
  mv "$tmp_dir/map.next" "$tmp_dir/style-map.json"
  echo "样式: $app:$name"
done < <(jq -r '.styles[] | @base64' "$bundle")

while IFS= read -r encoded; do
  item="$(printf '%s' "$encoded" | base64 -d)"
  key="$(jq -r '.key' <<<"$item")"
  app="$(jq -r '.app_id' <<<"$item")"
  name="$(jq -r '.name' <<<"$item")"
  existing_id="$(jq -r --arg app "$app" --arg name "$name" '.[] | select(.app_id==$app and .name==$name) | .id' "$tmp_dir/layers-current.json" | head -1)"
  catalog_id="$(jq -r --arg key "$(jq -r '.catalog_key' <<<"$item")" '.[$key]' "$tmp_dir/catalog-map.json")"
  style_key="$(jq -r '.default_style_key // empty' <<<"$item")"
  style_id=""
  [[ -z "$style_key" ]] || style_id="$(jq -r --arg key "$style_key" '.[$key]' "$tmp_dir/style-map.json")"
  payload="$(jq --arg id "${existing_id:-}" --arg catalog_id "$catalog_id" --arg style_id "$style_id" '{id:$id,app_id,name,title,description,catalog_item_id:$catalog_id,default_style_id:(if $style_id=="" then null else $style_id end),supported_crs,min_scale_denominator,max_scale_denominator,simplify_factor,queryable,enabled}' <<<"$item")"
  if [[ -n "$existing_id" ]]; then
    result="$(curl -fsS -X PUT -H 'Content-Type: application/json' --data-binary "$payload" "$base_url/api/published-layers/$existing_id")"
  else
    result="$(curl -fsS -X POST -H 'Content-Type: application/json' --data-binary "$payload" "$base_url/api/published-layers")"
  fi
  new_id="$(jq -r '.id' <<<"$result")"
  jq --arg key "$key" --arg id "$new_id" '. + {($key):$id}' "$tmp_dir/layer-map.json" > "$tmp_dir/map.next"
  mv "$tmp_dir/map.next" "$tmp_dir/layer-map.json"
  echo "发布图层: $app:$name"
done < <(jq -r '.layers[] | @base64' "$bundle")

group_app="$(jq -r '.group.app_id' "$bundle")"
group_name="$(jq -r '.group.name' "$bundle")"
existing_group_id="$(jq -r --arg app "$group_app" --arg name "$group_name" '.[] | select(.app_id==$app and .name==$name) | .id' "$tmp_dir/groups-current.json" | head -1)"
group_payload="$(jq --slurpfile layer_map "$tmp_dir/layer-map.json" --slurpfile style_map "$tmp_dir/style-map.json" --arg id "${existing_group_id:-}" '
  .group
  | {
      id:$id,
      app_id,
      name,
      title,
      description,
      enabled,
      members:[.members[] | {
        published_layer_id:$layer_map[0][.layer_key],
        style_id:(if .style_key==null then null else $style_map[0][.style_key] end),
        position,
        opacity,
        min_scale_denominator,
        max_scale_denominator,
        simplify_factor
      }]
    }
' "$bundle")"
if [[ -n "$existing_group_id" ]]; then
  curl -fsS -X PUT -H 'Content-Type: application/json' --data-binary "$group_payload" "$base_url/api/layer-groups/$existing_group_id" >/dev/null
else
  curl -fsS -X POST -H 'Content-Type: application/json' --data-binary "$group_payload" "$base_url/api/layer-groups" >/dev/null
fi

echo "导入完成: $group_app:$group_name"
echo "数据目录: $data_dir"
