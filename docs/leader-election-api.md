# 通用 Leader 选举 API 使用说明

## 1. 适用范围

Leader 选举由单实例、持久化运行的 `cangling-broker` 提供。选举接口不包含
`cangling-map` 专属逻辑，其他服务可以使用不同的选举名称共享同一套能力，例如：

- `cangling-map/imagery-repository`：影像仓库元数据写入者；
- `cangling-update/reconciler`：部署协调者；
- `cis/algorithm-dispatcher`：算法任务调度者。

同一选举名称在同一时刻最多只有一个有效 Leader；不同选举名称相互隔离。

## 2. 核心字段

| 字段 | 说明 |
| --- | --- |
| `election` | 稳定的选举名称，建议采用 `<应用>/<职责>` |
| `candidate_id` / `leader_id` | 候选实例 ID，StatefulSet Pod 名称最合适 |
| `term` | 单调递增的 fencing token，新 Leader 的 term 一定更大 |
| `lease_id` | 本次租约的随机标识，续租和退出必须精确匹配 |
| `expire_at` | Broker 记录的租约过期时间，主要用于观察和诊断 |
| `remaining_ttl_ms` | Broker 响应时的剩余租期，客户端据此建立单调时钟截止时间 |

建议 TTL 为 15 秒，每 5 秒续租。客户端失联或本地安全截止时间到达后必须立即
按 Follower 工作，不能继续接受仅限 Leader 的写操作。

## 3. HTTP API

HTTP 端口默认为 `7501`。配置了 `CL_BROKER_AUTH_TOKEN` 时，所有请求都要携带：

```text
Authorization: Bearer <token>
```

### 3.1 参与竞选

```bash
curl -sS -X POST http://cangling-broker:7501/election/campaign \
  -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -d '{
    "election": "cis/algorithm-dispatcher",
    "candidate_id": "cis-server-0",
    "ttl_seconds": 15
  }'
```

`won=true` 表示当前候选者持有租约。无论是否获胜，响应中的 `leader` 都表示
Broker 当前观察到的 Leader。

### 3.2 续租

```bash
curl -sS -X POST http://cangling-broker:7501/election/renew \
  -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -d '{
    "election": "cis/algorithm-dispatcher",
    "leader_id": "cis-server-0",
    "term": 3,
    "lease_id": "<campaign 返回的 lease_id>",
    "ttl_seconds": 15
  }'
```

只有仍然有效且完全匹配的 `leader_id + term + lease_id` 才能续租。过期租约不能恢复。

### 3.3 主动退出

```bash
curl -sS -X POST http://cangling-broker:7501/election/resign \
  -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -d '{
    "election": "cis/algorithm-dispatcher",
    "leader_id": "cis-server-0",
    "term": 3,
    "lease_id": "<lease_id>"
  }'
```

### 3.4 查询 Leader

```bash
curl -sS -H 'Authorization: Bearer <token>' \
  'http://cangling-broker:7501/election?election=cis%2Falgorithm-dispatcher'
```

`active=false` 表示记录存在但租约已经过期。

## 4. Java 自动选举

Java SDK 同时提供底层 `campaignElection`、`renewElection`、`resignElection`、
`getElection`，以及推荐使用的自动续租句柄：

```java
import cn.mapway.broker.ElectionHandle;
import cn.mapway.broker.SatwayClient;

try (SatwayClient broker = SatwayClient.connect("cangling-broker:7500");
     ElectionHandle election = broker.startElection(
             "cis/algorithm-dispatcher",
             System.getenv("HOSTNAME"),
             15)) {
    while (!Thread.currentThread().isInterrupted()) {
        if (election.isLeader()) {
            dispatchOneJob(election.term());
        }
        Thread.sleep(500);
    }
}
```

回调只适合更新界面或唤醒后台循环。执行排他操作前仍需调用 `isLeader()`。

## 5. Python 自动选举

```python
import os
import time
from cangling_broker import SatwayClient

with SatwayClient.connect("cangling-broker:7500") as broker:
    with broker.start_election(
        "cis/algorithm-dispatcher",
        os.environ["HOSTNAME"],
        ttl_seconds=15,
    ) as election:
        while True:
            if election.is_leader:
                dispatch_one_job(fencing_term=election.term)
            time.sleep(0.5)
```

## 6. fencing term 必须如何使用

租约只能限制正常工作的客户端，不能阻止暂停后恢复的旧进程继续发送已经缓存的请求。
因此 Leader 产生的任务、命令和元数据写入必须携带 `term`：

```json
{
  "job_id": "job-123",
  "leader_term": 8,
  "command": "start"
}
```

下游需要保存已接受的最大 term，并拒绝 `leader_term` 更小的请求。只判断
`candidate_id` 或 `lease_id` 不足以避免旧 Leader 写入。

## 7. 部署约束

- `cangling-broker` 当前必须是单实例，并为 SQLite 配置持久卷；
- 每个候选实例必须使用并发唯一的 `candidate_id`，不要让两个 Pod 共用同一个 ID；
- StatefulSet 推荐直接使用 Pod 名称，Deployment 可使用 Pod UID 或 Pod 名称；
- Broker 不可用时，客户端在本地安全截止时间到达后自动降级为 Follower；
- 如果选举服务本身也需要多副本容错，应改用 etcd/Consul/Kubernetes Lease 等共识存储，
  不能简单启动多个独立 Broker。

