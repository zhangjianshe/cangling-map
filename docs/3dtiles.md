# 3D Tiles 数据集发布（第一轮）

提供已有 3D Tiles 的校验、快照发布和 HTTP 访问。Earth 原生读取显式瓦片树并进行 LOD 选择；本模块不负责生产模型。
协议依据：https://github.com/CesiumGS/3d-tiles/tree/main/specification 。

## 数据准备与管理

设置 `TILES3D_DATA_ROOT`，默认 `data/3dtiles`。将服务目录挂载到持久磁盘。

```text
TILES3D_DATA_ROOT/
  buildings/
    dataset.sqlite             # 数据集独立的发布数据库
    source/tileset.json
    source/tiles/0.glb
    versions/<SHA-256>/tileset.json
    versions/<SHA-256>/tiles/0.glb
```

打开 `/3dtiles`，登记数据集 ID 和名称，将源文件复制到指定 source 目录，点击「校验」，然后「发布」。可以查看文件数、字节数、根包围体以及复制服务入口。更新数据后再次发布会生成内容寻址的新版本；相同内容复用版本。修改名称不会改变发布内容。

每个目录的 `dataset.sqlite` 保存数据集、当前版本、启用状态、各版本清单以及按版本/路径索引的文件元数据。三维请求不再读取全局 catalog 的发布记录；全局数据库仅作为旧版迁移来源。实际瓦片保存在版本目录。发布先将通过校验的同一份字节写入临时目录，完成后原子重命名，最后以数据库事务更新当前版本；失败不替换当前版本。未引用文件（例如旁边的说明或私有文件）不会发布。旧版本保留；本轮不提供自动清理。

本模块遵循现有管理 API 的部署方式：管理端应仅开放给受信任管理员，并由网关实施认证。只读 `/3dtiles/{id}/...` 开放 GET/HEAD CORS；写入 API 拒绝浏览器跨站请求。**不要把无认证的管理 API 直接暴露到公网。** 文件目录应仅由服务与受信任管理员写入；发布后不要直接改动 versions 内容。

## HTTP 接口

| 接口 | 说明 |
|---|---|
| GET `/api/3dtiles` | 列出已登记数据集 |
| PUT `/api/3dtiles/{id}`，`{"title":"名称"}` | 登记或修改名称；ID 允许1–64位字母、数字、横线、下划线 |
| GET `/api/3dtiles/{id}` | 当前状态、源目录和当前发布清单 |
| POST `/api/3dtiles/{id}/validate` | 校验源文件，返回文件清单与包围体；不发布 |
| POST `/api/3dtiles/{id}/publish` | 重新校验并发布快照（同时启用） |
| POST `/api/3dtiles/{id}/disable` | 停用，所有版本 HTTP 访问返回404，磁盘文件保留 |
| GET `/3dtiles/{id}/tileset.json` | 稳定入口：当前 root 中内容 URI 带版本路径，缓存为 no-store |
| GET/HEAD `/3dtiles/{id}/versions/{revision}/{path}` | 版本化资源；支持 Range、ETag、条件请求与正确 MIME |

客户端可使用稳定入口，也可固定到版本地址。稳定入口直接返回 JSON，避免客户端以重定向前的 URL 为基准解析相对路径。嵌套 tileset 的内容仍相对于各自文件解析。反向代理前缀通过相对 URI 和现有 `X-Forwarded-Prefix` 管理页逻辑兼容。

版本资源使用 `Cache-Control: no-cache`，允许缓存但每次重新验证，以便停用后下一次请求生效。已下载或已在客户端渲染的数据无法远程撤回。

## 支持范围与限制

- 3D Tiles 1.0/1.1 的显式瓦片树；`box`、`sphere`、`region`；4×4 transform；ADD/REPLACE；content/contents；嵌套外部 tileset。
- 内嵌缓冲区与纹理的 GLB 2.0；现代28字节头 b3dm；带 POSITION 或 POSITION_QUANTIZED 的 pnts。校验覆盖结构、引用、容器长度与基础字段，不替代 glTF/3D Tiles 完整一致性验证器，也不检查几何与包围体的精确包含关系。
- 暂不支持隐式瓦片、i3dm/cmpt、独立 glTF、外部元数据/必需 tileset 扩展、远程 URL、data URI、带转义/查询参数的路径或 `..` 相对引用。遇到不支持项明确拒绝，不发布不完整的数据集。
- 限制：单文件256MiB（JSON16MiB）、总量16GiB、100000文件/瓦片、嵌套64层、并发校验/发布2个。当前为同步管理请求，大数据发布须相应放宽网关超时；后续可扩展异步作业。
- 拒绝所有路径组成部分的符号链接，仅从指定数据集 source 目录读取。HTTP 仅提供数据库清单内的文件。

## 验证

```bash
cargo test -p map-3dtiles -p map-catalog
cargo build -p map-server
python3 scripts/test-3dtiles.py
```

HTTP 测试使用临时库和临时目录，覆盖嵌套引用、GLB/b3dm/pnts、错误路径、符号链接、循环引用、发布失败保留旧版本、源文件修改隔离、Range/HEAD/ETag/CORS、停用和重启恢复。

`TILES3D_BROWSER_TEST=1 python3 scripts/test-3dtiles.py` 会打印临时服务地址并暂停，方便浏览器检查；按回车完成测试并清理。2026-11-25 使用 Cesium 1.147.0 实测稳定入口加载嵌套 tileset、获取 GLB 并渲染三角形通过。测试场景使用程序生成的小模型，不需要外部地图或 Cesium ion token。

## 后续两轮

1. Cangling Earth 原生瓦片树遍历、屏幕空间误差 LOD、请求与 GPU 缓存预算、GLB/B3DM 渲染，接入 Keeper 场景图层及智能体工具。
2. 数据转换异步任务、进度、失败重试、LOD 构建和版本清理策略。

可复现的 Cesium 浏览器验收（临时测试服务保持运行时）：

```bash
npm install --prefix /tmp/cangling-tiles3d-client-test cesium@1.147.0 --no-audit --no-fund
cp scripts/3dtiles-client.html /tmp/cangling-tiles3d-client-test/index.html
python3 -m http.server 4318 --bind 127.0.0.1 --directory /tmp/cangling-tiles3d-client-test
# 浏览器打开 http://127.0.0.1:4318/?base=<测试脚本打印的BROWSER_BASE>
# 成功时页面显示 PASS、Cesium 版本和已渲染三角形数量。
```

## 数据集独立发布库（2026-11-25）

- 每个数据集目录可独立迁移。目录名须与数据库中的数据集 ID 一致。
- 启动时将旧全局 catalog 中的发布记录及全部历史版本事务性导入对应 `dataset.sqlite`；保留旧记录以供回退检查。已有本地记录优先，后续不会被旧库覆盖或重新启用。
- 管理列表扫描数据目录发现独立数据库；换用空的全局 catalog 后，仍可读取已发布数据集。
- SQLite 使用 WAL；备份运行中的数据库应使用 SQLite backup API，或停服务后复制整个目录。不要运行时只复制 `.sqlite` 而遗漏 WAL。
- `tiles3d_datasets` 管理当前版本和启用状态，`tiles3d_versions` 保存完整清单，`tiles3d_files` 按 `(dataset_id,revision,path)` 提供内容查询。GLB/JSON 数据本身仍在磁盘，未存入 SQLite BLOB。
- 发布顺序仍为写临时文件、完成快照、提交本地数据库事务。旧版本保留；此次不增加生产任务、自动清理或隐式瓦片支持。
- 三维文件直接由 Map 流式输出。`TILES3D_CACHE_URL/TOKEN` 对三维访问不再生效，即使旧部署仍设置它们，也不会请求 cangling-tile。二维缓存不受影响。
- HTTP 兼容原有稳定入口、版本 URL、Range/HEAD/ETag 与停用规则。响应 `X-Tiles3d-Cache: DISABLED` 表示未经过 Tile。
- Earth 在场景版本变化时清除旧瓦片树，并以 `no-store` 获取根入口；本次加载的子资源仍指向根入口给出的固定版本，避免加载中混用新旧发布。

本机回归：`python3 scripts/test-3dtiles.py` 覆盖旧库迁移、旧版本保留、停用后重启、空全局库目录发现，以及失效 Tile 地址下的直接读取。

本次验证：Map 服务端 30 项测试、catalog 11 项测试、隔离 HTTP 发布/迁移回归通过；Keeper Earth 传输与桥接测试通过。没有改动远端部署，也未进行浏览器视觉验收。
