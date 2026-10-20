#!/usr/bin/env bash
set -euo pipefail

base_url="${1:-http://127.0.0.1:3000}"
output_dir="${2:-$(cd "$(dirname "$0")" && pwd)}"
app_id="${APP_ID:-ai}"
group_name="${GROUP_NAME:-china}"

for command in curl jq; do
  command -v "$command" >/dev/null || { echo "缺少命令: $command" >&2; exit 1; }
done

mkdir -p "$output_dir/data"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

curl -fsS "$base_url/api/catalog" -o "$tmp_dir/catalog.json"
curl -fsS "$base_url/api/published-layers" -o "$tmp_dir/layers.json"
curl -fsS "$base_url/api/styles" -o "$tmp_dir/styles.json"
curl -fsS "$base_url/api/layer-groups" -o "$tmp_dir/groups.json"

exported_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n \
  --arg app_id "$app_id" \
  --arg group_name "$group_name" \
  --arg exported_at "$exported_at" \
  --slurpfile catalogs "$tmp_dir/catalog.json" \
  --slurpfile layers "$tmp_dir/layers.json" \
  --slurpfile styles "$tmp_dir/styles.json" \
  --slurpfile groups "$tmp_dir/groups.json" '
  ($groups[0][] | select(.app_id == $app_id and .name == $group_name)) as $group
  | [$group.members[].published_layer_id] as $layer_ids
  | [$layers[0][] | select(.id as $id | $layer_ids | index($id))] as $selected_layers
  | [$selected_layers[].catalog_item_id] as $catalog_ids
  | (([$selected_layers[].default_style_id] + [$group.members[].style_id])
      | map(select(. != null)) | unique) as $style_ids
  | {
      schema_version: 1,
      exported_at: $exported_at,
      application: $app_id,
      catalog: [
        $catalogs[0][]
        | select(.id as $id | $catalog_ids | index($id))
        | {
            key: .id,
            app_id,
            catalog_path,
            title,
            description,
            source_kind,
            source_config: (.source_config | del(.path)),
            data_file: (.source_config.path | split("/") | last),
            original_path: .source_config.path,
            native_crs,
            native_extent,
            attribution
          }
      ],
      styles: [
        $styles[0][]
        | select(.id as $id | $style_ids | index($id))
        | {key: .id, app_id, name, title, description, geometry_type, source_format, source_text}
      ],
      layers: [
        $selected_layers[]
        | {
            key: .id,
            app_id,
            name,
            title,
            description,
            catalog_key: .catalog_item_id,
            default_style_key: .default_style_id,
            supported_crs,
            min_scale_denominator,
            max_scale_denominator,
            simplify_factor,
            queryable,
            enabled
          }
      ],
      group: {
        app_id: $group.app_id,
        name: $group.name,
        title: $group.title,
        description: $group.description,
        enabled: $group.enabled,
        members: [
          $group.members[]
          | {
              layer_key: .published_layer_id,
              style_key: .style_id,
              position,
              opacity,
              min_scale_denominator,
              max_scale_denominator,
              simplify_factor
            }
        ]
      }
    }
' > "$output_dir/bundle.json"

while IFS=$'\t' read -r source_path data_file; do
  source_base="${source_path%.*}"
  found=0
  shopt -s nullglob
  for source_file in "$source_base".*; do
    cp -p "$source_file" "$output_dir/data/"
    found=1
  done
  shopt -u nullglob
  if [[ "$found" -eq 0 ]]; then
    echo "未找到数据文件: $source_path" >&2
    exit 1
  fi
  echo "已备份: $data_file"
done < <(jq -r '.catalog[] | [.original_path, .data_file] | @tsv' "$output_dir/bundle.json")

# 目标系统不需要知道原机器的绝对路径。
jq 'del(.catalog[].original_path)' "$output_dir/bundle.json" > "$tmp_dir/bundle.json"
mv "$tmp_dir/bundle.json" "$output_dir/bundle.json"

echo "备份完成: $output_dir"
