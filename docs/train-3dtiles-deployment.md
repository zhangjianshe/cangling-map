# train.cangling.cn 三维演示部署（2026-11-25）

主机：`ssh -p2225 root@changping.cangling.cn`，hostname master，K3s namespace cangling。
沿用既有公网入口：Map `/geoserver/`（另有`/ib/geoserver/`），Tile `/cangling-tile/`。

## 数据

- Dragon：`https://train.cangling.cn/geoserver/3dtiles/stanford-dragon/tileset.json`
  - 版本 `c34549b2fa543e6c1c487bb33fcc4f6be3164508ab4f4397ae00a7616ab7f566`，4文件。
- 郑州三级LOD：`https://train.cangling.cn/geoserver/3dtiles/zhengzhou-cbd-osm/tileset.json`
  - 版本 `e29e9063f176bdd9389f093841798b45aab053d6048e1ff8c39c37ab451c2a71`，31文件。

数据在主机 `/opt/cl/local/cangling/cangling-map/data/3dtiles/`，容器 `/data/3dtiles/`。
源数据和许可信息保留在数据集目录，HTTP只暴露发布清单中的模型资源。

## 首次部署的运行方式与回退（后续缓存部署见下文）

原Map镜像0.1.25没有3D Tiles接口，本次在原镜像运行环境中使用持久卷内的新release二进制。
Deployment command为`/data/deploy/20261125/map-server`，设置`TILES3D_DATA_ROOT=/data/3dtiles`。
原地图数据、Tile配置和挂载保持原状，Tile继续使用0.1.21，未更换程序或清理缓存。

- 构建源提交：3309c98。
- 构建命令：`cargo build --release --locked -p map-server`。
- 二进制SHA256：`7a876c3fbeeb389cc2199fd0e253b4d65361a36e335ca29c8b5d3ee7317b1c40`。
- 先用独立检查Pod/独立SQLite验证运行和两套数据，再滚动更新正式部署（原Map策略为Recreate）。
- 检查Pod已删除，源数据及正式版本保留。
- 配置与在线SQLite备份：`/opt/cl/local/cangling/cangling-map/deployment-backup-20261125/`。
- 本次更新后的直接回退：`kubectl -n cangling rollout undo deploy/cangling-map`；若已有后续部署，
  应依据备份恢复对应版本，不要盲目undo。新增目录/表不需删除。
- 后续改为正式新镜像发布时，必须移除Deployment的command覆盖，否则仍会运行这份二进制。

Nginx配置在cangling-nginx ConfigMap。新增两条`/geoserver/api/3dtiles`和
`/ib/geoserver/api/3dtiles`只读保护：公网允许GET/HEAD，写请求403。
管理员从主机访问Map ClusterIP的管理API登记/发布。配置验证后已重启Nginx部署使subPath挂载生效。

## 首次部署时的缓存关系（已由下文缓存接入替代）

系统现有二维瓦片使用`Earth -> Map -> Tile`；Map负责服务，Tile负责缓存。
**当前3D Tiles仍由Map读取版本文件直接返回，未接入Tile的持久缓存。**
本次不把二维z/x/y接口冒用为3D资源缓存，也没有宣称完成3D缓存接入。
后续需扩展Tile按数据集/发布版本/资源路径保存对象，再由Map做缓存读写、
回源及停用检查；Earth仍只访问Map。

## 验证

- 两服务公网health正常；Map、Tile均1/1 Ready。
- 35个发布文件公网下载与本机源文件逐字节一致。
- HTTPS证书正常，CORS `*`，Range 206及ETag正常。
- 两个前缀的公开POST validate均403，内网发布成功。
- Keeper郑州项目已改用公网Map URL；原Dragon项目当前无对应图层，未覆盖其现有场景。
- Earth公网实测12km/6.5km/3.5km分别选low/medium/high，loaded/drawing为1/1、2/1、30/28；
  errors为空、pending=0、budgetLimited=false。

## 第二轮：接入 Tile 持久对象缓存

当前链路为 `Earth → Map → Tile`。Map 先检查数据集启用状态及发布清单，再读取 Tile；
未命中时从版本目录读取并校验 SHA-256，回填 Tile 后响应。稳定入口 tileset.json 在缓存原文基础上
将资源 URI 改为版本路径。已发布版本互相隔离，停用数据集不会因旧缓存继续公开。

Map 复用「缓存服务设置」的 enabled/base_url/token；可用 `TILES3D_CACHE_URL` 与
`TILES3D_CACHE_TOKEN` 显式覆盖。train 使用 `http://cangling-tile:7601`，凭证引用已有
`CANGLING_TILE_AUTH_TOKEN`，不把 token 暴露给浏览器。HTTP 客户端不跟随重定向、不使用系统代理，
连接700ms、总请求3秒超时；缓存故障退回源文件。对象读取与回填均校验发布清单 SHA-256。

响应 `X-Tiles3d-Cache`：
- HIT：读取并验证了 Tile 缓存。
- MISS：首次未命中，已成功回填。
- ERROR：回填失败，但返回了有效源文件。
- BYPASS：未配置、超过32 MiB、并发满或缓存查询失败，使用源文件流式响应。
- ETag 命中的304直接返回，无需访问缓存，因此不增加 Tile 命中次数。

Range、HEAD、ETag/no-cache 语义保留。缓存接口全部要求服务 Bearer 凭证，公网匿名访问401。
Tile 数据文件为主机 `/opt/cl/local/cangling/tiledata/.tiles3d-cache.sqlite`；
统计和按版本回收接口见 cangling-tile README。当前旧版本缓存需显式回收，尚无自动TTL清理。

### 本次部署

- 配置备份：`/opt/cl/local/cangling/cangling-map/deployment-backup-20261125-cache/{map,tile}.json`。
- Map command：`/data/deploy/20261125-cache/map-server`。
- Tile command：`/app/tiledata/.deploy/20261125-cache/cangling-tile`。
- Map SHA-256：`bf0274f6591bc80af54656e414480dfcd9989c5fc37f6aa947ac96a85c32bd9c`。
- Tile SHA-256：`bc20ee8848c9ced6cae4b69d1192c22d90225542a6d0ecefd751b95ccb9b788f`。
- Tile 为 x86_64-unknown-linux-musl 静态构建，先在原 Debian 镜像的隔离检查Pod中验证，再更新部署。
- 原镜像及其环境/挂载保留；以后正式镜像发布仍需移除 command 覆盖。
- 回退应按备份恢复 command/env；Tile回退必须恢复原 command 为 null。缓存表为独立新增文件，
  不影响原二维瓦片。不要删除源数据、原数据库，也不要在后续滚动更新后盲目执行 rollout undo。

### 验收结果

- Map 31项、Tile 31项 Rust 测试通过。
- `scripts/test-3dtiles-cache.py` 使用隔离目录、真实HTTP服务验证35文件MISS/HIT、哈希、
  Range/HEAD/304、停用后404、无凭证401及Tile停止后的源文件回退。
- train公网：Dragon 4文件首次4 MISS、再次4 HIT；郑州31文件首次31 MISS、再次31 HIT，
  全部SHA-256与本机发布源一致。
- 浏览器LOD：Dragon 30km/6km/2km分别选中low/medium/high；郑州12km/6.5km/3.5km
  分别选中low/medium/high，绘制数1/1/28。所有级别pending=0、errors为空、无预算超限。
- 浏览器操作后Tile持久命中计数Dragon从4增至8、郑州从31增至32；浏览器已缓存资源的304不会增加该值。
- 两个本机Keeper演示项目均使用train公网Map地址。
- 正式Tile重启后仍保留35个对象；两套数据再次Range读取均为206 + HIT；Map和Tile最终均1/1 Ready、0次容器重启。
