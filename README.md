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

## 数据与缓存目录

服务将持久化数据和可清理缓存分开保存：

| 用途 | 容器默认路径 | 配置项 |
| --- | --- | --- |
| SQLite 目录库 | `/data/catalog.db` | `CATALOG_DATABASE_URL` |
| 样式图片 | `/data/style-assets` | `STYLE_ASSET_ROOT` |
| 本地缓存根目录 | `/cache` | `MAP_CACHE_ROOT` |
| WMTS 瓦片缓存 | `/cache/wmts` | `WMTS_CACHE_PATH` |
| 矢量预投影缓存 | `/cache/vector-projection` | `VECTOR_PROJECTION_CACHE_PATH` |

`WMTS_CACHE_PATH` 和 `VECTOR_PROJECTION_CACHE_PATH` 是可选的细粒度覆盖项；未设置时，
分别使用 `MAP_CACHE_ROOT/wmts` 和 `MAP_CACHE_ROOT/vector-projection`。本地直接运行时，
`MAP_CACHE_ROOT` 默认为当前目录下的 `cache`。

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
```

将 `VECTOR_PROJECTION_CACHE_PATH` 设置为空字符串可关闭持久化预投影缓存。

## Docker Compose

仓库提供 [docker-compose.yml](docker-compose.yml)。其中数据和缓存使用两个独立卷：

```yaml
services:
  cangling-map:
    image: soft.cangling.cn:22002/cangling/cangling-map:latest
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
    image: soft.cangling.cn:22002/cangling/cangling-map:latest
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
soft.cangling.cn:22002/cangling/cangling-map:<version>
soft.cangling.cn:22002/cangling/cangling-map:latest
```

GitHub 仓库需要配置 `HARBOR_USERNAME` 和 `HARBOR_PASSWORD`。执行
`./release.sh` 会在干净工作区完成检查、递增补丁版本、创建版本标签并推送，随后由
GitHub Actions 发布 amd64/arm64 镜像。

## 中国行政区划迁移包

`backups/ai-china` 保存配置、样式和导入导出脚本。原始 Shapefile 体积较大，默认不
提交到普通 Git 历史。迁移前请使用 `export.sh` 生成完整 `data/`，或从发布资产、
对象存储恢复数据，再执行 `import.sh`。
