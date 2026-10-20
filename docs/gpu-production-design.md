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

## 分阶段能力

1. 索引面网格、透明混合与 4× MSAA（已完成原型和真实数据回归）；
2. 线段展开、线宽与虚线（已完成实线及 6/4 虚线；连接方式和实例化待完成）；
3. 图案 atlas；
4. CPU 标签布局 + GPU atlas、文字与 halo（RGBA atlas 与生产字体栅格化原型已完成，真实布局接入待完成）；
5. metatile、切片和异步 PNG 编码；
6. WMS/WMTS 接入、CPU 降级、指标与压力测试。

只有当同一套真实请求在 CPU/GPU 输出差异、并发 P95、显存上限、设备丢失恢复和长时间运行
测试中全部达标，GPU 后端才允许作为默认渲染器。
