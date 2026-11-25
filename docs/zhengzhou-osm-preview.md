# 郑州 CBD 3×3 公里 OSM 建筑预览

2026-11-25 本机实测。中心 WGS84 经度113.728、纬度34.775（郑东新区CBD、如意湖）。
采用以中心为原点的等距方位投影，裁剪至 x/y 各[-1500,1500]米的正方形。
底图和模型均使用WGS84，不对OSM坐标添加GCJ-02偏移。

## 数据与高度规则

原始数据来自 OpenStreetMap 官方地图API：
`https://api.openstreetmap.org/api/0.6/map?bbox=113.7109,34.7610,113.7451,34.7890`。
Overpass节点在本次下载时连接失败，最终使用完整OSM XML，并检查relation成员完整性。
保留原始XML、转换JSON、含原始标签和高度来源的GeoJSON以及生成报告。

© OpenStreetMap contributors，数据使用 [ODbL](https://www.openstreetmap.org/copyright)。
派生建筑数据库继续按ODbL提供；tileset、GLB和Keeper图层名称均保留署名。

- 原始523个建筑/建筑部分对象；裁剪、relation去重和建筑部分覆盖处理后485个对象。
- 5个使用`height`，25个按`building:levels × 3m`估高，455个缺失高度暂用9m。
- 最终28个对象含楼层标签（其中3个同时有height，以height优先）。
- 平屋顶挤出，保留内院；不模拟纹理、真实屋顶形状、窗户或BIM细节。
- 地面采用现有Earth的Copernicus GLO-30地形，建筑边界采样最大值+0.5m，
  各建筑保持水平底面。该数据为DSM，且沿用当前Earth数值高程基准，
  未做EGM2008到椭球高的测绘级转换；不代表真实地坪或精确楼高。

## 本机结果

- 数据目录：`~/.local/share/cangling-map-local/tiles3d/zhengzhou-cbd-osm/`
- 稳定入口：`http://127.0.0.1:4310/3dtiles/zhengzhou-cbd-osm/tileset.json`
- 版本：`e29e9063f176bdd9389f093841798b45aab053d6048e1ff8c39c37ab451c2a71`
- 28个完整500米网格分块，加远/中景各一个模型，共30个几何资源、31个发布文件（含tileset）。
- Keeper独立项目：郑州 CBD · OSM 三维建筑。
- 地球地址：`http://127.0.0.1:4301/earth-window.html?project=5b996eff-0058-4317-8ad7-cd93a688e4d5`

浏览器原生WASM/WebGL2正射、45°视角和刷新恢复通过：28个瓦片loaded/drawing，
pending=0、errors=[]、budgetLimited=false。建筑明暗与立体挤出可见。
已增加以下三级几何LOD，采用REPLACE而非叠加绘制。
原有Dragon和general场景未修改，服务仅发布在本机。

## 重建

在数据目录准备上述API的`map.osm`。Python虚拟环境依赖：numpy、shapely、pyproj、mapbox-earcut、pillow。

```bash
python scripts/build-zhengzhou-osm.py --data-root /path/to/zhengzhou-cbd-osm
# 首次会下载Earth现用的地形元数据和少量z12瓦片；后续复用terrain/缓存。
# 输出source/tileset.json、GLB、buildings.geojson、report.json。
```

再按3D Tiles管理API登记、校验、发布。脚本验证三角剖分面积（含内院）、有限法线和顶点预算；
每次构建后应调用Map的validate再发布，已有发布版本不受source文件重建影响。

## 三级 LOD（2026-11-25）

| 级别 | 轮廓简化容差 | 顶点数 | 几何文件 | geometricError |
| --- | ---: | ---: | ---: | ---: |
| 远景 low | 20m | 14,850 | 1 | 40m |
| 中景 medium | 3m | 24,102 | 1 | 6m |
| 近景 high | 原始轮廓 | 35,847 | 28 | 0 |

低/中景保持拓扑（建筑分部和内院数量）、485个OSM对象ID与原有高度。
实际轮廓Hausdorff误差最大约19.858m、2.990m；声明两倍容差作为保守几何误差。
远景减少约59%的顶点，中景减少约33%。近景GLB与上一版逐字节一致。

层级为一个low根节点 → 一个medium节点 → 28个空间叶节点；祖先包围体
覆盖所有级别几何，最多30个资源，不调整Earth的32资源预算。
父级使用REPLACE，在子级就绪前保留已有父级，避免重复叠加。

切换依据Earth原生屏幕空间误差（当前图层阈值16），同时受视距、视口、
相机倾角和包围体影响，不是固定的相机高度断点。
本机浏览器正射视角实测：

| 相机高度 | selectedContents | loaded / drawing |
| --- | --- | --- |
| 12,000m | buildings-low.glb | 1 / 1 |
| 6,500m | buildings-medium.glb | 2 / 1 |
| 3,500m | buildings-X-Y.glb（完整叶节点） | 30 / 28 |

各级pending=0、errors=[]、budgetLimited=false。高→低回退释放子资源，
再次中→高加载、刷新后恢复30个已加载资源/28个绘制叶节点通过。验收读取原生选中资源和相机命令确认版本，不只比较截图。

新增数据验证：

```bash
python scripts/test-zhengzhou-lod.py --data-root /path/to/zhengzhou-cbd-osm
```

验证三级顶点递增、同一组OSM对象、单位法线、所有GLB顶点落入包围体、
父包围体包含子包围体、REPLACE树和资源预算。Map validate/publish通过。
原始单级发布版本继续保留，可按原版本URL访问。

## 程序化立面贴图版本

本机独立数据集 `zhengzhou-cbd-textured`：
`http://127.0.0.1:4310/3dtiles/zhengzhou-cbd-textured/tileset.json`。
发布版本 `c075386bf001100f28db4f99b61c0ec9a8d19f1390a43439d8f622a1304fb33f`。

近景墙面按每 3 米一个窗格单元生成重复 UV，窗格图片为原创 64×64 PNG，内嵌在 GLB 中；屋顶独立纯色材质。楼层/高度沿用原有标签与估算规则，不能视为真实立面照片。低、中 LOD 使用原简化轮廓及纯色材质，减少远处纹理采样和加载开销。

保留 485 个 OSM 对象、原始建筑位置、高度、内院和所有三级顶点数，仍为 30 个几何资源、31 个发布文件。近景改为屋顶/墙面两个 primitive，共享 POSITION/NORMAL/UV，使用互不重叠的索引覆盖原有全部三角形。

`test-zhengzhou-lod.py` 新增 PNG、重复采样、有限 UV、索引完整覆盖和低中景无贴图校验。Map 校验、版本发布通过；原 `zhengzhou-cbd-osm` 发布版本不变。

### 旋转时灰色回退修复

后续版本 `41e7f03a16ee19e3df29498d8e2fa070170c0449658e09d2d4adbcf76101bffe` 已将 low/medium 同样改为屋顶和立面贴图，避免 REPLACE 子节点加载期间切回纯灰色父级。仍为相同 485 对象和三级几何顶点数；内嵌图片可由 Earth 纹理缓存复用。稳定入口不变，旧发布版本保留。

### 基础 PBR 材质版本

版本 `5460375eb2925fa9c7f4a7c78125e7eb390b059063f5f86380fbe283e90d079b` 在全部 LOD 的墙面材质中加入 64×64 原创窗框凹凸法线和粗糙度图片。玻璃区域比墙面平滑，但不模拟真实玻璃透射或环境反射。法线只修改着色，485 个对象的几何和碰撞不变。

三个内嵌图片分别为基础色、法线及 metallicRoughness，复用相同 UV 和重复采样器；屋顶继续使用独立材质。数据校验、发布及真实瓦片 GPU 绘制测试通过。本机稳定入口不变。
