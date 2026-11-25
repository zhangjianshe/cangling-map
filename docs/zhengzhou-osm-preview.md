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
- 版本：`555dc57e0fadbcac4f0a362994a2df0f1bf650a04b757ce11b0f609827c834e8`
- 28个非空500米网格分块、35,847顶点、29个发布文件（含tileset）。
- Keeper独立项目：郑州 CBD · OSM 三维建筑。
- 地球地址：`http://127.0.0.1:4301/earth-window.html?project=5b996eff-0058-4317-8ad7-cd93a688e4d5`

浏览器原生WASM/WebGL2正射、45°视角和刷新恢复通过：28个瓦片loaded/drawing，
pending=0、errors=[]、budgetLimited=false。建筑明暗与立体挤出可见。
本示例采用空间分块和视锥裁剪，没有生成多精度LOD；不要将距离变化称为几何LOD验证。
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
