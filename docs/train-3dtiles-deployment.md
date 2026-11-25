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

## 运行方式与回退

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

## 缓存关系

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
