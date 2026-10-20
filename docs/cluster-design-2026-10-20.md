# Cangling Map 集群设计讨论纪要（2026-10-20）

## 已确认的方向

- 保留当前 Rust 管理 UI、HTTP 管理 API 与 WMS/WMTS 服务，不迁移到 Java。
- 单机继续使用 SQLite；集群采用一个可写 Master 和多个只读 Replica。
- Master 默认也参与瓦片渲染，必要时可降低负载均衡权重或关闭渲染。
- 每个 StatefulSet Pod 使用独立 PVC，不允许多个 Pod 同时打开同一个 SQLite 文件。
- Master 是 SQLite 的唯一写入者；Replica 同步一致性快照，并从内存快照执行渲染。
- 渲染热路径读取 `ArcSwap<CatalogSnapshot>`，不逐请求查询 SQLite。

## 配置同步

每次 Master 配置事务提交后递增 `revision`，快照携带 `term`、`revision`、`checksum`、
`updated_at` 和 `writer_id`。Replica 先下载到临时文件，校验 SHA-256 与
`PRAGMA integrity_check`，再更新本地恢复副本并原子替换内存快照。

第一阶段采用固定 Master（StatefulSet 的 `cangling-map-0`）和定时 revision 轮询；
第二阶段如确有自动故障转移需求，再使用 Kubernetes Lease 选举，并加入任期 fencing、
写入前 Lease 验证以及候选节点最新快照恢复。

## 路由与故障行为

- 管理写请求固定路由到 Master。
- WMS/WMTS 请求负载均衡到 Master 和全部 Replica。
- Master 故障时 Replica 继续用最后一次有效快照渲染，管理写入暂停。
- 新 Master 必须取得集群已知最高 `(term, revision)` 后才能开放写入。

## 备份

SQLite 位于 PVC，不依赖容器可写层。在线备份必须使用 SQLite Backup API，不能只复制
活动的 `catalog.db`，也不应依赖 K3s `local-path` 的内部宿主机目录。备份生成后执行
`PRAGMA integrity_check`，再复制到独立的 NFS、S3 或 MinIO 存储。

## 下一步

1. 抽取不可变 `CatalogSnapshot` 与运行时内存读取路径。
2. 增加 revision、快照导出/下载和校验接口。
3. 增加 Master/Replica 角色和管理写入保护。
4. 更新 StatefulSet、Service 与管理/渲染路由。
5. 实现备份、恢复和故障切换测试。
