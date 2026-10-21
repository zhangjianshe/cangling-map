#!/usr/bin/env sh
set -eu

server_url=${MAP_SERVER_URL:-http://127.0.0.1:3000}
plan_file=${1:-deploy/gpu-prewarm.example.tsv}

if [ ! -r "$plan_file" ]; then
  echo "无法读取 GPU 预热计划: $plan_file" >&2
  exit 1
fi

request_count=0
while IFS='|' read -r layer crs bbox width height pixel_ratio; do
  case "$layer" in
    ''|'#'*) continue ;;
  esac
  pixel_ratio=${pixel_ratio:-1}
  echo "预热 ${layer}: ${crs} ${bbox} ${width}x${height} @${pixel_ratio}x"
  curl --fail --silent --show-error --output /dev/null --get "${server_url%/}/wms" \
    --data-urlencode 'SERVICE=WMS' \
    --data-urlencode 'VERSION=1.3.0' \
    --data-urlencode 'REQUEST=GetMap' \
    --data-urlencode "LAYERS=$layer" \
    --data-urlencode "CRS=$crs" \
    --data-urlencode "BBOX=$bbox" \
    --data-urlencode "WIDTH=$width" \
    --data-urlencode "HEIGHT=$height" \
    --data-urlencode 'FORMAT=image/png' \
    --data-urlencode 'TRANSPARENT=true' \
    --data-urlencode "PIXELRATIO=$pixel_ratio"
  request_count=$((request_count + 1))
done < "$plan_file"

echo "GPU 网格预热完成，共请求 ${request_count} 个视口。"
