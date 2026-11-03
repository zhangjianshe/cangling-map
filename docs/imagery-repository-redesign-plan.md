# 影像仓库重新设计开发计划

## 1. 目标与边界

影像仓库从 CIS 中拆出，由 `cangling-map` 提供管理入口和任务编排，
`cangling-tile` 保存最终瓦片，`cangling-broker` 负责选举、任务、事件、心跳和进度协调。

本次改造必须满足：

- `cangling-map` 可以多实例部署，任意实例都可以接收浏览器上传。
- 保留浏览器 Web Worker 下载、断点续传和文件上传能力。
- 同时支持服务端下载、浏览器下载和混合下载。
- 瓦片二进制只通过 HTTP 传输，不进入 Broker。
- 浏览器不能持有 `cangling-tile` 或 Broker 的内部令牌。
- 在线存储继续兼容 `cangling-tile` 的 `.s` 分片；MBTiles 作为导入、导出和交换格式。
- 完成迁移后，可以安全移除 CIS 中的旧影像仓库模块。

## 2. 总体架构

```text
CIS / 浏览器管理界面
          │ HTTP、SSE
          ▼
┌──────────────────────────────┐
│      cangling-map 多实例      │
│ Master：仓库、任务、进度管理  │
│ Worker：上传代理、服务端下载   │
└───────────┬──────────────────┘
            │
      ┌─────┴──────────┐
      ▼                ▼
cangling-broker     cangling-tile
选举、任务和事件       瓦片最终存储
```

### cangling-map

- 提供仓库、任务、分片、租约、导入和导出 API。
- Master 是元数据唯一写入者；Worker 执行下载和上传代理。
- 上传成功以 `cangling-tile` 已提交为准。
- 使用本地 SQLite 保存状态，通过 Outbox/Inbox 与 Broker 同步。

### cangling-tile

- 是瓦片数据唯一事实来源。
- 提供单瓦片及二进制批量写入接口。
- 影像仓库数据库和图层默认不可自动回收。
- 写入返回幂等回执、校验值和最终存储标识。

### cangling-broker

- 固定单实例运行，使用持久化数据目录和 `Recreate` 更新策略。
- 提供带任期和隔离令牌的 Master 选举。
- 使用 persistent + single 主题调度任务。
- 使用 broadcast 主题通知进度、心跳和目录变化。

## 3. Master 选举与状态同步

通用选举接口、HTTP/Java/Python 示例及 fencing term 使用约束见
[通用 Leader 选举 API 使用说明](leader-election-api.md)。

选举名称为 `cangling-map/imagery-repository-master`。租约建议 15 秒，5 秒续租；
连续两次续租失败后实例必须停止 Master 工作。选举结果包含：

- `leader_id`
- `term`
- `lease_id`
- `expire_at`

每次新选举递增 `term`。续租必须同时匹配 leader、term 和 lease；过期租约不可恢复。
所有 Master 写操作携带 term，下游拒绝旧任期写入。

Master 在同一 SQLite 事务内写业务状态和 Outbox。其他实例消费持久化事件，更新本地
只读 SQLite；定期状态快照用于新实例初始化和事件缺口修复。

## 4. 数据模型

### Repository

保存名称、说明、源 URL 模板、空间范围、缩放级别、格式、`cangling-tile`
数据库/图层标识、回收策略和版本。仓库范围是长期容器允许覆盖的**最大范围**，可以是
全球 Z0-Z19；创建仓库时不展开瓦片，也不会因为理论瓦片总量过大而拒绝创建。

每个仓库维护按缩放级别分页的稀疏位图索引。每页记录 32,768 个 XYZ 坐标是否已经
写入 `cangling-tile`，只为实际写入过的页分配 4 KiB，不创建全球稠密矩阵。

位图的坐标编号与原 CIS `IndexService` 保持一致：在 Z 层将瓦片映射为
`tileId = x + y * 2^z`，再以 `tileId / 8` 定位字节、`tileId % 8` 定位位。因此旧索引
语义可以无损迁移。存储方式不再照搬 CIS：CIS 会为每一级预创建完整的 `{z}.index`
稠密文件，全球 Z0–Z19 的理论索引约 45.8 GiB；新实现将相同位序切成 32,768 位的
SQLite 稀疏页，空仓库不预分配索引，只有首次写入某页的瓦片时才增加 4 KiB。
位图独立保存到 `IMAGERY_INDEX_DATABASE_URL` 指定的 `imagery-index.db`，不再扩大任务
元数据库 `imagery.db`。当一页内所有有效 XYZ 坐标均已存在时，该页转换成无 BLOB 的
`full` 标记；包括 Z0–Z7 最后一页不足 32,768 位的情况也按实际有效坐标判断完整。
升级时服务会一次性把旧 `imagery.db` 中的位图页迁入新库并同步压缩完整页。

### Job

保存仓库、任务自己的缩放级别和空间范围、执行模式、状态、总量、成功/失败/跳过数量、
Master 任期和时间信息。任务范围必须包含在仓库最大范围内，但可以远小于仓库范围。
执行模式为 `server`、`browser` 或 `hybrid`。

任务允许由操作员删除。删除会清理任务、分片、回执和未发送的任务事件，但不会清除
仓库级位图，也不会删除已经提交给 `cangling-tile` 的瓦片。这样删除的是运行记录而非
数据本身，之后创建重叠任务时仍能跳过已经存在的瓦片。

规划任务时，系统只枚举任务范围，并查询仓库位图；已存在的坐标不进入下载列表，完全
命中的分片不会创建。这样同一个全球仓库可以反复创建不同区域任务，也可以对同一区域
断点续传，而不需要创建多个仓库。

### Chunk

保存缩放级别、X/Y 范围、执行实例、租约、过期时间、重试次数和分片进度。

### TileReceipt

保存 Z/X/Y、格式、大小、SHA-256、幂等键和 `cangling-tile` 写入回执。

## 5. Broker 主题

| 主题 | 配置 | 用途 |
| --- | --- | --- |
| `imagery.job.command` | persistent + single | 创建、暂停、恢复、取消 |
| `imagery.chunk.work` | persistent + single | Worker 竞争领取分片 |
| `imagery.chunk.result` | persistent + single | Master 汇总分片结果 |
| `imagery.tile.stored` | persistent + single | 瓦片提交完成 |
| `imagery.job.events` | ephemeral + broadcast | UI 进度通知 |
| `imagery.repository.changed` | persistent + broadcast | 目录刷新 |
| `system.component.heartbeat` | ephemeral + broadcast | 组件状态 |
| `system.election.changed` | persistent + broadcast | Master 变化 |

所有业务事件包含 `event_id`、`job_id`、`chunk_id`、`term`、`lease_id`、
`attempt`、`source` 和 `occurred_at`。消费端按 `event_id` 去重。

## 6. 数据流程

### 浏览器模式

1. Master 生成任务和分片。
2. 浏览器分页读取每个分片当前仍缺失的坐标列表。
3. 浏览器领取带签名的短期租约，Web Worker 只下载缺失瓦片。
4. 浏览器把二进制瓦片上传到任意 map 实例。
5. map 校验任务、坐标、格式、范围和租约。
6. map 将瓦片转发到 `cangling-tile`。
7. Tile 提交后，map 持久化 Outbox 并发布 `imagery.tile.stored`。
8. Master 幂等汇总进度并通过 SSE 通知页面。

### 服务端和混合模式

服务端 Worker 通过 `imagery.chunk.work` 竞争消费任务。`hybrid` 任务采用浏览器优先：
创建分片时保持 `pending`，不立即占用服务端 Worker；操作员调用 takeover API 后，Leader
在同一事务内把所有仍为 `pending` 的分片转为 `queued` 并写入 Outbox。重复接管不会重复
投递。浏览器定期调用 heartbeat API 刷新活动时间；超过
`CANGLING_MAP_HYBRID_TAKEOVER_SECONDS` 后，当前 Leader 自动调用同一 takeover 操作。
浏览器和服务端交叉完成的瓦片由坐标唯一键与 SHA-256 回执保证幂等。

## 7. API

```text
GET             /api/imagery/leadership
GET             /api/imagery/worker-metrics
GET             /api/imagery/snapshot
GET/POST       /api/imagery/repositories
GET/PUT/DELETE /api/imagery/repositories/{id}
GET/POST       /api/imagery/jobs
GET/DELETE      /api/imagery/jobs/{id}
GET             /api/imagery/jobs/{id}/chunks
GET             /api/imagery/jobs/{id}/chunks/{chunk_id}/missing
POST            /api/imagery/jobs/{id}/chunks/{chunk_id}/complete
POST            /api/imagery/jobs/{id}/heartbeat
POST            /api/imagery/jobs/{id}/takeover
POST            /api/imagery/jobs/{id}/pause
POST            /api/imagery/jobs/{id}/resume
POST            /api/imagery/jobs/{id}/cancel
POST            /api/imagery/jobs/{id}/leases
PUT             /api/imagery/uploads/{z}/{x}/{y}.{format}
GET             /api/imagery/jobs/{id}/events
POST            /api/imagery/imports
POST            /api/imagery/repositories/{id}/exports
```

批量上传使用二进制帧、multipart 或 CBOR，禁止使用 Base64 JSON。

任务事件接口使用 SSE：首次连接与重连均返回完整进度快照，运行阶段发送 `progress`，
完成、失败或取消时发送一次 `terminal` 后关闭。快照包含 Job 计数、分片状态聚合和最后
分片更新时间，不发送可能达到数十万条的完整分片数组。

`GET /api/imagery/snapshot` 仅由当前 Leader 提供，返回仓库、任务、分片和 Outbox
水位，不复制瓦片二进制。快照在一个 SQLite 读事务中生成；Follower 校验来源
leader-id 和 fencing term 后周期导入。Broker 结果先写入 Inbox，幂等应用成功后才
ACK；ACK 丢失产生的重复投递不会重复累计进度。

## 8. 实施阶段

1. **基础纵向切片（已完成）**：领域模型、SQLite 迁移、仓库/任务 API、签名租约、Tile 转发、Outbox。
2. **Broker 协调（基础闭环已完成）**：已完成 Broker 通用租约选举、单调 fencing term、Map 写入门禁、领导状态接口、主题自动配置、Outbox 幂等发布、HTTP 租约消费/续租/ACK、Tile 结果 Inbox 幂等消费和 Follower 元数据快照自动同步。服务端/混合任务通过 `imagery.job.command` 进入 Leader，按经纬度范围和缩放级别生成确定性 XYZ 分片，再发布到 `imagery.chunk.work`；重复命令不会重复生成分片。
3. **浏览器下载器（管理闭环已完成）**：内置 Web Worker 分页读取未完成分片，并在启动分片前从仓库稀疏位图取得当前缺失坐标，以有限并发从源站下载并直接上传二进制瓦片；支持单瓦片指数退避重试、任务心跳、短期租约逐页刷新、暂停/恢复/取消和 SSE 进度事件。每页完成后使用签名租约提交分片回执，Leader 按仓库位图核对整个分片后幂等转为 completed，并对集群同步延迟自动退避。管理控制台已提供仓库分页、仓库编辑、独立任务范围、三种模式任务创建、浏览器下载控制、Hybrid 接管、任务分页与实时进度。下一步增加二进制批量上传协议。
4. **服务端下载器（基础闭环已完成）**：所有 map 实例竞争分片，长任务自动续租；可信来源白名单、重定向禁止、连接/响应超时、图片类型和大小校验已启用。分片内并发采用 AIMD 风格动态背压：近期耗时低于目标时逐级增加，明显变慢时逐级降低，请求失败时立即减半。瓦片写入 `cangling-tile` 后产生幂等 Tile 回执，分片完成结果返回 Leader。Broker 会返回当前/最大投递次数，最后一次失败会持久化 `imagery.chunk.failed`，Leader 幂等写入失败终态；领导状态接口暴露配置上下限、当前并发、平均瓦片耗时和 Worker 运行指标，独立指标接口保留最近 120 个进程内采样点。Hybrid 已支持浏览器优先、心跳、显式幂等接管和 Leader 自动失活接管；取消任务会终止未完成分片，Worker 对暂停任务保持续租并等待恢复。SSE 任务进度已提供完整快照、变化去重、keep-alive 和终态关闭。
5. **导入导出**：`.s` 扫描、分片 MBTiles、`repository.json`、浏览器文件上传。
6. **CIS 迁移**：兼容代理、数据核对、并行观察、旧模块只读和最终移除。

## 9. 安全和可靠性

- 租约使用集群共享密钥签名，包含 job、repository、范围、格式和过期时间。
- Tile/Broker 令牌来自 K3s Secret，永不返回浏览器。
- 服务端下载执行协议、主机和目标地址检查，防止 SSRF。
- 限制单瓦片、批次数量和请求总大小，并校验图片格式。
- 使用 Outbox/Inbox、幂等键和 SHA-256 应对重复投递。
- Broker 不可用时禁止产生新的任务分配；已提交 Tile 的回执在本地 Outbox 中重试。
- `CANGLING_MAP_CHUNK_TILES` 控制单个服务端分片的最大瓦片数，默认 `512`，允许范围 `1..65536`；异常大任务会在分片数超过安全上限时被拒绝，避免一次规划耗尽内存。

## 10. 验收标准

- 任意 map 实例均可接收浏览器上传。
- 同一时刻只有一个仓库 Master，旧任期写操作会被拒绝。
- Master 切换不丢任务、不重复累计进度。
- 一百万瓦片连续测试无缺失、无错误重复计数。
- 代理写入吞吐不低于直接写入 `cangling-tile` 的 85%。
- 浏览器刷新、网络中断和 Worker 重启后任务可以继续。
- `.s` 与 MBTiles 迁移前后瓦片数量和抽样校验值一致。
- CIS 旧模块可以在保留回滚能力的前提下下线。
