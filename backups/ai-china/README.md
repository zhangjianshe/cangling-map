# ai:china 行政区划迁移包

此目录保存 `ai:china` 中国行政区划图层组的完整迁移材料：

- `bundle.json`：目录项、发布图层、GeoCSS 样式与图层组配置；
- `data/`：本地生成的五组 Shapefile 及其配套文件（不提交到普通 Git 历史）；
- `export.sh`：从运行中的系统重新生成备份；
- `import.sh`：向新系统幂等导入，自动重新关联新系统生成的 ID。

## 重新导出

```bash
./export.sh http://127.0.0.1:3000
```

也可以指定输出目录：

```bash
./export.sh http://127.0.0.1:3000 /data/backups/ai-china
```

## 导入新系统

原始行政区数据体积较大，GitHub 普通 Git 单文件上限为 100 MiB，因此仓库仅保存
配置、样式和迁移脚本。导入前请先执行 `export.sh` 生成 `data/`，或从发布资产/
对象存储恢复完整的 `data/` 目录。不要只放置单个 `.shp`，还需要对应的
`.dbf`、`.shx`、`.prj` 等文件。

先启动新系统，再执行：

```bash
./import.sh http://new-server:3000 /opt/cis/core/geoserver/data/china
```

第二个参数是新系统实际读取 Shapefile 的目录。脚本会：

1. 复制数据文件；
2. 按 `app_id + catalog_path` 导入或更新目录项；
3. 按 `app_id + name` 导入或更新样式和发布图层；
4. 将旧 ID 映射为新系统 ID；
5. 最后创建或更新 `ai:china` 图层组。

依赖：`bash`、`curl`、`jq`、`base64`。写入系统数据目录时，执行用户必须具有相应权限。
