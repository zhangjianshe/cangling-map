# Cangling Map

Cangling Map 是使用 Rust、Axum 和 GDAL 构建的地图服务，提供 WMS 1.3.0、
WMTS 1.0.0、矢量/栅格渲染、GeoCSS 样式、图层组、瓦片缓存和管理控制台。

## 小结

- 单一可执行程序，内置管理页面和 `cangling-gdal-core`；
- 支持 SQLite 目录库、发布图层、样式与图层组持久化；
- 支持 WMS 动态出图与 WMTS 瓦片服务；
- 支持本地文件缓存或远程 Cangling Tile 缓存；
- 数据目录 `/data` 与可随时清理的缓存目录 `/cache` 相互独立；
- Docker 镜像支持 `linux/amd64` 和 `linux/arm64`。

## 架构

```text
HTTP / OGC KVP 请求
        ↓
ogc-wms / ogc-wmts：协议解析、校验与能力文档
        ↓
map-core / map-catalog：统一请求模型与 SQLite 目录
        ↓
map-style / map-gdal / map-render：样式、投影与渲染
        ↓
PNG/JPEG、瓦片缓存或 OGC 异常响应
```

主要 crate：

- `cangling-gdal-core`：GDAL FFI、矢量预投影和 Tiny Skia 渲染；
- `ogc-wms`、`ogc-wmts`：OGC 服务协议；
- `map-core`：协议无关的地图请求和图层接口；
- `map-catalog`：SQLite 目录、样式、发布配置与系统设置；
- `map-style`：GeoCSS 解析、校验和 SLD 输出；
- `map-gdal`、`map-render`：GDAL 数据读取与图像合成；
- `map-server`：Axum HTTP 服务与管理控制台。

## 本地运行

系统需要 GDAL 3.13 开发环境和 Rust 工具链。

```bash
cargo run -p map-server

curl http://127.0.0.1:3000/health
curl 'http://127.0.0.1:3000/wms?SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.3.0'
```

打开 `http://127.0.0.1:3000/` 进入指标首页，数据目录位于
`http://127.0.0.1:3000/catalog`。

## 反向代理子路径

管理页面、静态资源、前端 API、WMS/WMTS 预览以及能力文档支持通过
`X-Forwarded-Prefix` 动态识别外部路径前缀。Nginx 应删除前缀后再转发，并保留外部
主机和协议：

```nginx
location ^~ /geoserver/ {
    proxy_set_header X-Forwarded-Prefix /geoserver;
    proxy_set_header X-Forwarded-Host $http_host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_pass http://cangling-map:3000/;
}
```

`proxy_pass` 末尾的 `/` 不能省略，它负责把 `/geoserver/metrics` 转换成上游的
`/metrics`。前缀可以改成任意合法子路径，无需重新编译；未设置
`X-Forwarded-Prefix` 时继续使用根路径。

## 数据与缓存目录

服务将持久化数据和可清理缓存分开保存：

| 用途 | 容器默认路径 | 配置项 |
| --- | --- | --- |
| SQLite 目录库 | `/data/catalog.db` | `CATALOG_DATABASE_URL` |
| 全球地名库 | `/usr/share/cangling-map/placenames.sqlite` | `PLACENAMES_DATABASE_URL` |
| 样式图片 | `/data/style-assets` | `STYLE_ASSET_ROOT` |
| 本地缓存根目录 | `/cache` | `MAP_CACHE_ROOT` |
| WMTS 瓦片缓存 | `/cache/wmts` | `WMTS_CACHE_PATH` |
| 矢量预投影缓存 | `/cache/vector-projection` | `VECTOR_PROJECTION_CACHE_PATH` |
| 中文标注字体 | `/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc` | `MAP_FONT_PATH` |
| GPU 就绪网格缓存 | `/cache/gpu-mesh` | `MAP_GPU_MESH_CACHE_PATH` |
| DEM 原始文件 | `/data/dem` | `DEM_DATA_ROOT` |
| DEM Terrarium 本地缓存（文件系统模式） | `/cache/terrain` | `DEM_CACHE_PATH` |

`WMTS_CACHE_PATH`、`VECTOR_PROJECTION_CACHE_PATH` 和 `MAP_GPU_MESH_CACHE_PATH` 是可选的
细粒度覆盖项；未设置时，分别使用 `MAP_CACHE_ROOT/wmts`、
`MAP_CACHE_ROOT/vector-projection` 和 `MAP_CACHE_ROOT/gpu-mesh`。本地直接运行时，
`MAP_CACHE_ROOT` 默认为当前目录下的 `cache`。

## 地名标注数据

`data/placenames.sqlite` 是独立于目录库的只读 SQLite 数据库，内置 Natural Earth
10m Populated Places 5.1.2 的 7,342 个居民地点，保存 WGS84 经纬度、中文/英文名称、
标注等级、最小显示级别及 R-tree 空间索引。Docker 镜像将它放在
`/usr/share/cangling-map/placenames.sqlite`；可以通过 `PLACENAMES_DATABASE_URL`
切换到自行构建的数据库。更新数据时运行：

```bash
python3 tools/build-placenames.py --source /path/to/ne_10m_populated_places.zip
```

脚本也可省略 `--source` 从 Natural Earth 下载原始 ZIP；需要 GDAL Python bindings。
读取接口与 DEM 一样允许跨域 GET/HEAD：

```text
GET /api/placenames/metadata
GET /api/placenames/tiles/{z}/{x}/{y}?lang=zh
```

第二个接口返回 XYZ/Web Mercator 范围内的 JSON 点位数组（`features` 含
`name`、`longitude`、`latitude`、`label_rank`、`min_zoom`、`population`），
支持 `lang=zh|en|default`。服务按数据建议级别和标注优先级筛选，每个瓦片最多
返回 512 条；客户端仍需做屏幕空间避让。数据源为
[Natural Earth](https://www.naturalearthdata.com/downloads/10m-cultural-vectors/10m-populated-places/)
（public domain）。

## DEM 高程瓦片

管理页面位于 `/terrain`，可选择数据集、分页查看源文件、上传单波段 GeoTIFF，
显示当前版本的可复制 XYZ 瓦片地址，然后点击“导入并构建元数据”发布或重新构建。
上传文件不会立即改变已发布的瓦片；
重新导入后客户端通过元数据接口获得新的版本化瓦片地址。

将同一数据集的一组单波段高程 GeoTIFF 放在
`$DEM_DATA_ROOT/<dataset-id>/source/`。导入只读取该目录下一层的 `.tif`、`.tiff`；
发布中的源文件视为不可变，替换文件后须重新导入。源数据位于 `/data/dem`，
清理瓦片缓存不会删除原始 DEM。

导入会检查坐标系、地理范围、波段数、高程单位及像元缩放系数，并将每个文件的
EPSG:3857 覆盖范围写入 SQLite R-tree 索引。请求瓦片时，服务先按发布版本读缓存；
未命中才检索相交文件，用 GDAL 重投影并以双线性方式采样数值高程，最后编码为
256×256 Terrarium RGB PNG。无覆盖的瓦片编码为全 NoData 黑色瓦片，也会被缓存。
DEM 使用当前瓦片缓存后端：`WMTS_CACHE_BACKEND=cangling-tile` 时，按数据集和
发布版本写入 Cangling Tile 的 `dem` 数据库；文件系统模式下使用
`DEM_CACHE_PATH`（默认 `/cache/terrain`）；`none` 模式不缓存。远程缓存暂时不可用时
仍会返回新渲染的瓦片，但该次结果不会写入本地缓存。
重叠文件按 `priorities` 中的数值优先级排序，优先级相同时选较高分辨率；
空洞由其他文件补齐。例如导入 JSON 可包含
`"priorities":{"local_high_res.tif":10}`。

```bash
# 例：/data/dem/sichuan/source/ 下已有 GeoTIFF
curl -X POST http://127.0.0.1:3000/api/terrain/sichuan/import \
  -H 'Content-Type: application/json' \
  -d '{"title":"Sichuan DEM","vertical_datum":"EGM96","unit":"metre","max_zoom":18,"prepare_cog":true}'
curl http://127.0.0.1:3000/api/terrain/sichuan/metadata
```

元数据响应包含当前 `revision`、`tileUrl`、覆盖范围、垂直基准和缩放范围。客户端应使用
`tileUrl` 中的版本化地址请求瓦片；每次重新导入会生成新版本，旧浏览器缓存不会被复用。
公开的元数据和版本化 DEM 瓦片接口允许任意来源的浏览器使用 CORS 读取
（`GET`/`HEAD`，包含 `OPTIONS` 预检）；导入和上传接口仍保持同源限制。
`vertical_datum` 仅声明源数据基准，目前**不执行垂直基准转换**；混合数据集的各文件
须先在导入前统一高程基准。该字段应来自数据提供方的说明，表示像元高程值所参照的
高程参考面，如 EGM96（EPSG:5773）、1985 国家高程基准或 WGS84 椭球高。
EPSG:4326 是水平坐标参考系，不能据此推断高程基准；来源不明时填写“未知（待核实）”。
设置 `prepare_cog:true` 时，导入会在
`$DEM_DATA_ROOT/<dataset-id>/optimized/<revision>/` 生成带数值平均概览图的 COG
副本，并以副本作为发布源；原始文件不会被改写。省略该选项时直接发布源文件，
适合已经制作为 COG 的数据。旧版本的 COG 与瓦片缓存可在确认不再使用后清理。
全国等大范围 DEM 若由大量 1° 源文件组成，可先用 `gdalbuildvrt` 镶嵌，再用
`gdal_translate -of COG -co BIGTIFF=YES -co COMPRESS=DEFLATE -co BLOCKSIZE=512`
生成带内部概览层的单文件 COG；仅将成品放入 `source/` 后重新导入。
原始分幅可保存在同一数据集目录的 `raw-tiles/`，该目录不会被导入扫描。
DEM 瓦片渲染使用 GDAL Warp 的概览层选择，在低缩放级别避免逐幅重投影完整分辨率数据。
上传 API 为 `PUT /api/terrain/<dataset-id>/files/<filename.tif>`，请求体是原始文件字节；
文件列表 API 为 `GET /api/terrain/<dataset-id>/files?page=1&page_size=20`。
上传使用流式写盘，默认单文件上限 8 GiB，可用 `DEM_UPLOAD_MAX_BYTES` 调整。

例如将缓存放到另一块磁盘：

```bash
MAP_CACHE_ROOT=/mnt/fast-disk/cangling-map-cache cargo run -p map-server
```

矢量预投影缓存还可配置：

```bash
VECTOR_PROJECTION_CACHE_MB=2048
VECTOR_PROJECTION_CACHE_SIZE=32
VECTOR_PROJECTION_SCOPE=local
VECTOR_PROJECTION_METATILE_FACTOR=4
MAP_VECTOR_GEOMETRY_CACHE_MB=512
```

将 `VECTOR_PROJECTION_CACHE_PATH` 设置为空字符串可关闭持久化预投影缓存。

CPU 渲染器只持久化可跨范围、尺寸和样式复用的预投影矢量数据，不保存最终 RGBA 或 PNG。
进程内还会在 `MAP_VECTOR_GEOMETRY_CACHE_MB` 预算内复用按缩放精度简化后的世界坐标几何、
标签锚点和字形轮廓；这些都是下一次实时绘制的矢量中间结果，不是最终图片缓存。
最终瓦片缓存由 cangling-tile 负责。上线前可使用与客户端一致的范围、输出尺寸和像素比预热
矢量投影或 GPU 后端：

```bash
MAP_RENDER_BACKEND=cpu cargo run --release -p map-server
MAP_SERVER_URL=http://127.0.0.1:3000 \
  ./tools/prewarm-render-cache.sh deploy/render-prewarm.example.tsv
```

`/api/metrics` 的 `render_cache` 节点返回源数据集、预投影矢量和图片纹理的缓存状态。

## GPU 矢量渲染（实验性生产模式）

服务支持 CPU、强制 GPU 和带 CPU 自动降级的 GPU 模式：

```bash
MAP_RENDER_BACKEND=auto \
MAP_GPU_ADAPTER=NVIDIA \
MAP_GPU_SCENE_CACHE_MB=1024 \
MAP_GPU_TARGET_POOL_MB=512 \
MAP_GPU_MESH_CACHE_PATH=/cache/gpu-mesh \
MAP_GPU_VIEW_OVERSCAN=1.5 \
cargo run --release -p map-server
```

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `MAP_RENDER_BACKEND` | `cpu` | `cpu`、`gpu` 或 `auto`；生产试运行建议使用 `auto` |
| `MAP_GPU_ADAPTER` | 空 | 可选的显卡名称片段，例如 `NVIDIA`；为空时选择系统默认适配器 |
| `MAP_GPU_SCENE_CACHE_MB` | `1024` | 已投影网格、边界和标签图集的显存 LRU 预算 |
| `MAP_GPU_TARGET_POOL_MB` | `512` | 可复用离屏纹理、MSAA 纹理和回读缓冲的显存预算 |
| `MAP_GPU_MESH_CACHE_PATH` | `${MAP_CACHE_ROOT}/gpu-mesh` | 已投影、三角化并完成标签布局的 GPU 就绪网格磁盘缓存；空字符串表示关闭 |
| `MAP_GPU_MESH_CACHE_MAX_ENTRY_MB` | `4096` | 单个 GPU 网格缓存文件的最大读取尺寸，防止损坏文件造成异常内存分配 |
| `MAP_GPU_VIEW_OVERSCAN` | `1.5` | WMS 驻留视口相对请求范围的扩展倍数，限制为 `1.0`–`3.0`；`1.0` 表示关闭平移复用 |

`auto` 模式遇到某一请求暂不支持的符号器时，只将该请求降级到 CPU；如果 GPU
设备不可用，则触发进程内熔断，后续请求直接使用 CPU。`/api/metrics` 返回当前后端、
GPU 成功/失败/降级次数、场景缓存条目、命中率和驻留字节数。

指标页面的“渲染设备”区域同时显示配置模式与实际后端、CPU 使用率与系统负载、内存占用、
wgpu 枚举到的全部图形适配器，以及当前选中的 GPU。NVIDIA 主机安装 `nvidia-smi` 且驱动
正常时，还会显示显存、GPU 利用率、温度、功耗和驱动版本；遥测不可用不会影响渲染服务。

GPU 缓存分为三层：FlatGeobuf 预投影缓存保存目标坐标系数据，`.cgmesh` 磁盘缓存保存可直接
上传的顶点、索引、图案和标签 atlas，进程内 LRU 则保存已经上传到显存的场景。数据主文件或
Shapefile 伴随文件、目标 CRS、当前生效样式、范围或尺寸变化时，缓存键会自动变化；格式损坏
或版本不兼容的 `.cgmesh` 会被删除并重建。`/api/metrics` 的 `gpu_renderer` 节点会同时返回
`mesh_disk_hits`、`mesh_disk_misses`、`mesh_disk_writes` 和 `mesh_disk_errors`。

GPU 首次处理 WMS 请求时，会按 `MAP_GPU_VIEW_OVERSCAN` 准备一个更大的驻留视口；相同
CRS、分辨率和生效样式规则下，只要后续平移范围仍包含在驻留视口内，就能直接复用显存中的
网格、边界和标签图集。渲染目标与回读缓冲独立池化，不再随每个场景重复占用显存；图层组
成员并行准备并直接以 RGBA 合成，避免中间 PNG 编解码。WMTS 不启用视口扩展，仍优先使用
最终瓦片缓存。部署时必须给容器暴露兼容 Vulkan 的 GPU；NVIDIA Docker 环境
通常还需配置 NVIDIA Container Toolkit 和 `gpus: all`。ARM64 或没有 GPU 的节点应保留
`cpu`，或使用 `auto` 确保安全降级。

部署后可用预热计划提前生成 `.cgmesh`，使后续进程或 Pod 只需从本地缓存读取并上传 GPU：

```bash
MAP_SERVER_URL=http://127.0.0.1:3000 \
  ./tools/prewarm-gpu-mesh-cache.sh deploy/gpu-prewarm.example.tsv
```

计划格式为 `图层|CRS|BBOX|宽度|高度|pixelRatio`。预热请求仍会完成一次实际渲染，建议在
服务就绪后、接入流量前执行。多个实例若共享只读数据但各自使用本地 `/cache`，应分别预热；
若共享缓存卷，则原子写入可让后启动的实例直接复用文件。

Docker 主机安装 NVIDIA Container Toolkit 后，可使用 GPU 覆盖文件启动：

```bash
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d
docker compose exec cangling-map sh -c 'ls /dev/nvidia*'
curl http://127.0.0.1:3000/api/metrics
```

K3s 需要先安装 NVIDIA device plugin，再通过仓库提供的 Kustomize overlay 部署：

```bash
kubectl apply -k deploy
```

该 overlay 会把 [`deploy/k3s-gpu-patch.yaml`](deploy/k3s-gpu-patch.yaml) 合并到
基础部署。每个声明
`nvidia.com/gpu: 1` 的 Cangling Map Pod 会独占一个 GPU 资源；GPU 数少于副本数时，
应只给部分副本应用该补丁，其余副本继续作为 CPU 节点。

## Docker Compose

仓库提供 [docker-compose.yml](docker-compose.yml)。其中数据和缓存使用两个独立卷：

```yaml
services:
  cangling-map:
    image: harbor.cangling.cn:22002/cangling/cangling-map:latest
    restart: unless-stopped
    ports:
      - "3000:3000"
    environment:
      CATALOG_DATABASE_URL: sqlite:///data/catalog.db?mode=rwc
      WMTS_CACHE_BACKEND: filesystem
      MAP_CACHE_ROOT: /cache
    volumes:
      - cangling-map-data:/data
      - cangling-map-cache:/cache

volumes:
  cangling-map-data:
  cangling-map-cache:
```

启动与停止：

```bash
docker compose up -d
docker compose logs -f cangling-map
docker compose down
```

删除容器不会影响数据卷。缓存卷可以独立清理；只有明确需要同时删除持久化数据时，
才使用 `docker compose down -v`。

如果希望直接映射主机目录，可将卷配置替换为：

```yaml
    volumes:
      - ./runtime/data:/data
      - /mnt/fast-disk/cangling-map-cache:/cache
```

请确保 UID/GID `10001` 对这些目录具有读写权限。

## 远程缓存

生产环境可切换到 Cangling Tile：

```bash
WMTS_CACHE_BACKEND=cangling-tile
CANGLING_TILE_URL=http://cangling-tile:7601
CANGLING_TILE_TOKEN=replace-with-write-token
```

兼容变量 `TILE_CACHE_URL`、`TILE_CACHE_TOKEN` 仍可使用，并具有更高优先级。
也可以在管理控制台的“设置 → 缓存服务”中保存远程配置；数据库配置启用后会覆盖
启动环境，关闭或未配置时回退到本地缓存。

## 小结：集成 Cangling Tile

Cangling Map 与 Cangling Tile 组合后，职责划分如下：

- Cangling Map 管理数据目录、样式和图层组，并负责 GDAL 渲染；
- Cangling Tile 集中管理 WMTS 瓦片、SQLite 分片、热点内存缓存和自动清理；
- Cangling Map 先向 Cangling Tile 查询瓦片；命中时直接返回；
- 未命中时由 Cangling Map 渲染，并使用 Bearer Token 将结果写回 Cangling Tile；
- Cangling Tile 临时不可用时不会阻止新瓦片完成渲染和返回；
- 矢量预投影缓存仍属于 Cangling Map，保存在它自己的 `/cache` 卷中。

仓库提供可直接启动两个服务的
[docker-compose.cangling-tile.yml](docker-compose.cangling-tile.yml)。建议先创建 `.env`：

```dotenv
CANGLING_TILE_AUTH_TOKEN=replace-with-a-long-random-token
CANGLING_TILE_ADMIN_PASSWORD=replace-with-a-strong-admin-password
```

然后启动：

```bash
docker compose -f docker-compose.cangling-tile.yml up -d
docker compose -f docker-compose.cangling-tile.yml logs -f
```

完整示例的核心配置如下：

```yaml
services:
  cangling-map:
    image: harbor.cangling.cn:22002/cangling/cangling-map:latest
    depends_on:
      cangling-tile:
        condition: service_healthy
    ports:
      - "3000:3000"
    environment:
      WMTS_CACHE_BACKEND: cangling-tile
      CANGLING_TILE_URL: http://cangling-tile:7601
      CANGLING_TILE_TOKEN: ${CANGLING_TILE_AUTH_TOKEN}
      MAP_CACHE_ROOT: /cache
    volumes:
      - cangling-map-data:/data
      - cangling-map-cache:/cache

  cangling-tile:
    image: harbor.cangling.cn:22002/cangling/cangling-tile:latest
    ports:
      - "7601:7601"
    environment:
      CANGLING_TILE_AUTH_TOKEN: ${CANGLING_TILE_AUTH_TOKEN}
      CANGLING_TILE_ADMIN_PASSWORD: ${CANGLING_TILE_ADMIN_PASSWORD}
    volumes:
      - cangling-tile-data:/app/tiledata
      - cangling-tile-config:/app/config

volumes:
  cangling-map-data:
  cangling-map-cache:
  cangling-tile-data:
  cangling-tile-config:
```

两个服务必须使用相同的 `CANGLING_TILE_AUTH_TOKEN`。该 Token 只用于地图服务写入
瓦片；Cangling Tile Dashboard 的管理操作使用管理员密码和登录会话。启动后可访问：

- Cangling Map：`http://127.0.0.1:3000/`
- Cangling Tile Dashboard：`http://127.0.0.1:7601/`

## K3s 部署

仓库提供 [deploy/k3s.yaml](deploy/k3s.yaml)，默认部署：

- 1 个 Cangling Tile 实例；
- 3 个 Cangling Map 实例；
- K3s `local-path` 持久卷；
- Cangling Map NodePort `30080`；
- Cangling Tile Dashboard NodePort `30601`。

部署前需要修改清单中的 Token、管理员密码和地图源数据 `hostPath`。如果私有镜像仓库
需要认证，先创建拉取 Secret，再把 `imagePullSecrets` 加入两个 Pod 模板：

```bash
kubectl create namespace cangling-map
kubectl -n cangling-map create secret docker-registry cangling-registry \
  --docker-server=harbor.cangling.cn:22002 \
  --docker-username='<用户名>' \
  --docker-password='<密码>'
```

应用部署并观察状态：

```bash
kubectl apply -f deploy/k3s.yaml
kubectl -n cangling-map rollout status deployment/cangling-tile
kubectl -n cangling-map rollout status statefulset/cangling-map
kubectl -n cangling-map get pods,svc,pvc
```

三个 Map Pod 共享 SQLite 目录库 PVC，因此使用 `StatefulSet` 的 `OrderedReady` 顺序启动，
并由 `ReadWriteOnce` 本地卷约束在同一 K3s 节点。每个 Pod 的预投影缓存使用独立
`emptyDir`，WMTS 瓦片则统一写入单实例 Cangling Tile。这个清单适合单节点 K3s，或将
三个 Map 实例固定在目录卷所在节点的部署。若要跨节点扩展 Map 服务，应先将目录存储
从 SQLite 改为支持多节点并发访问的数据库。

## Docker 镜像与发布

镜像基于官方 OSGeo GDAL 3.13：

```text
ghcr.io/osgeo/gdal:ubuntu-small-3.13.0
```

本地构建：

```bash
docker buildx build -t cangling-map:local .
```

发布镜像：

```text
harbor.cangling.cn:22002/cangling/cangling-map:<version>
harbor.cangling.cn:22002/cangling/cangling-map:latest
```

GitHub 仓库需要配置 `HARBOR_USERNAME` 和 `HARBOR_PASSWORD`。执行
`./release.sh` 会在干净工作区完成检查、递增补丁版本、创建版本标签并推送，随后由
GitHub Actions 发布 amd64/arm64 镜像。

### GitHub 发布配置

在 GitHub 仓库中打开 **Settings → Secrets and variables → Actions**，添加以下
Repository secrets：

| Secret | 必需 | 说明 |
| --- | --- | --- |
| `HARBOR_USERNAME` | 是 | `harbor.cangling.cn:22002` 镜像仓库用户名或机器人账号 |
| `HARBOR_PASSWORD` | 是 | 对应密码或机器人 Token，需要推送 `cangling/cangling-map` 的权限 |

以下值由工作流或 GitHub 自动提供，不需要手动配置：

| 变量 | 来源 | 说明 |
| --- | --- | --- |
| `GITHUB_TOKEN` | GitHub Actions | 检出当前仓库；工作流只申请 `contents: read` |
| `GITHUB_SHA` | GitHub Actions | 写入镜像标签和 OCI revision 标签 |
| `GITHUB_REF_NAME` | GitHub Actions | 当前发布标签，例如 `v0.1.1` |
| `BUILD_DATE` | 镜像构建过程 | 未显式提供时由 `map-server/build.rs` 使用 UTC 日期生成 |

镜像地址当前固定为：

```text
harbor.cangling.cn:22002/cangling/cangling-map
```

`cangling-gdal-core` 已集成到本仓库，因此不需要配置
`CANGLING_GDAL_TOKEN`。发布工作流由 `v*` 标签或 GitHub Actions 页面中的
**Run workflow** 手动触发。建议先验证机器人账号可以登录和推送：

```bash
docker login harbor.cangling.cn:22002 -u '<HARBOR_USERNAME>'
docker push harbor.cangling.cn:22002/cangling/cangling-map:<测试标签>
```

## 中国行政区划迁移包

`backups/ai-china` 保存配置、样式和导入导出脚本。原始 Shapefile 体积较大，默认不
提交到普通 Git 历史。迁移前请使用 `export.sh` 生成完整 `data/`，或从发布资产、
对象存储恢复数据，再执行 `import.sh`。
