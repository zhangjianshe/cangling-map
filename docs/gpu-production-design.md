# GPU 渲染后端生产化设计

## 原则

- CPU 渲染器始终保留，GPU 初始化、缓存或绘制失败时自动降级。
- GDAL 负责数据读取、投影和属性；GPU 不直接解析 Shapefile。
- 几何按 CRS、缩放级别和空间块预处理，使用索引网格常驻显存。
- 请求只选择相交块、提交绘制、回读并编码，不重复投影和三角化。
- WMTS 优先按 metatile 批处理；WMS 使用匹配请求范围的空间块。

## 模块边界

`map-gpu` 只接受已经准备好的索引网格，不依赖 GDAL、目录库或 OGC 协议：

```text
GDAL / 样式规则
  → GeometryPreparer（投影、简化、三角化、线展开、标签布局）
  → Vec<MeshChunk>
  → .cgmesh（版本化、原子写入的本地持久缓存）
  → map-gpu（上传、显存缓存、render pass、readback）
  → RGBA
  → PNG/JPEG 编码
```

这种边界允许几何预处理单独测试和持久化，也能让无 GPU 环境完全不初始化 `map-gpu`。

## 缓存键

```text
source fingerprint + target CRS + style revision + zoom band + spatial chunk
```

持久缓存记录场景尺寸、背景、顶点、索引、图案纹理及已经排版的标签 atlas。文件包含 magic、
格式版本、端序和顶点布局校验，采用同目录临时文件加原子 rename 写入；损坏、截断、版本不符
或超过 `MAP_GPU_MESH_CACHE_MAX_ENTRY_MB` 的文件不会进入渲染器，而是删除后重建。缓存键包含
数据主文件及 Shapefile 伴随文件的路径、大小和修改时间，以及目标 CRS、生效样式、范围和尺寸。

显存层超过 `MAP_GPU_SCENE_CACHE_MB` 后按 LRU 淘汰完整场景。磁盘层位于
`MAP_GPU_MESH_CACHE_PATH`，默认继承 `${MAP_CACHE_ROOT}/gpu-mesh`，可以在上线前通过真实 WMS
请求计划预热。进程重启后只需读取 `.cgmesh` 并上传，不再重复 GDAL 查询、投影、三角化、
线展开、文字栅格化和碰撞布局。

`map-gpu::ByteLruCache` 已实现按估算显存字节数计费和最近使用时间淘汰。单个请求块超过预算
时允许暂时独占缓存以完成请求，其余条目全部释放。内存场景和磁盘网格两层的命中、未命中、
写入、错误及常驻字节均通过 `/api/metrics` 暴露。

## 分阶段能力

1. 索引面网格、透明混合与 4× MSAA（已完成原型和真实数据回归）；
2. 线段展开、线宽与虚线（已完成实线及 6/4 虚线；连接方式和实例化待完成）；
3. 图案 atlas（多纹理 repeat/clamp 批次和凹多边形像素回归已完成）；
4. CPU 标签布局 + GPU atlas、文字与 halo（真实投影数据、重复规则和碰撞结果已完成像素回归）；
5. metatile、切片和异步 PNG 编码；
6. WMS/WMTS 接入、CPU 降级、指标与压力测试。

只有当同一套真实请求在 CPU/GPU 输出差异、并发 P95、显存上限、设备丢失恢复和长时间运行
测试中全部达标，GPU 后端才允许作为默认渲染器。

## 服务降级边界

`map-render::FallbackVectorRasterizer` 作为 WMS、WMTS 与预览共用的后端包装器。GPU 对当前
规则返回 `Unsupported` 时只回退该请求；初始化失败或设备丢失返回 `Unavailable` 时触发
进程内熔断，后续请求不再反复访问故障设备。包装器记录 primary 尝试/成功/失败、fallback
请求及熔断状态，并允许运维恢复设备后显式重新启用。GPU 后端接入服务后这些计数需并入现有
指标页面。
