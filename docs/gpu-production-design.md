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
  → map-gpu（上传、显存缓存、render pass、readback）
  → RGBA
  → PNG/JPEG 编码
```

这种边界允许几何预处理单独测试和持久化，也能让无 GPU 环境完全不初始化 `map-gpu`。

## 缓存键

```text
source fingerprint + target CRS + style revision + zoom band + spatial chunk
```

缓存记录顶点、索引、属性选择结果、显存字节数、最后访问时间和构建耗时。超过
`GPU_CACHE_BUDGET_MB` 后按 LRU 淘汰完整空间块。失效以数据文件指纹、样式 revision 和
投影配置为准。

`map-gpu::ByteLruCache` 已实现按估算显存字节数计费和最近使用时间淘汰。单个请求块超过预算
时允许暂时独占缓存以完成请求，其余条目全部释放；服务接入时还需把命中、未命中、淘汰、
常驻字节和超预算请求暴露到 metrics。

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
