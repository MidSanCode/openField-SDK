# 每日阶段性任务 (Daily Activity Milestones)

每日阶段性任务按「当天做了多少」发放奖励：每发 1 / 5 / 10 / 20 篇帖子、
每天在聊天里发 25 / 50 / 100 条消息，都可以各自领取一档奖励。每一档
**每天只能领取一次**，第二天重新开始计算并可再次领取。

代码位置：

- 任务目录（种子数据）：`pkg/database/migration.go` (`taskSeeds` / `seedTasks`)
- 进度与领取逻辑：`pkg/repository/task.go`
- HTTP 接口：`services/account/internal/handler/task.go` + `router.go`
- 网关路由：`services/gateway/cmd/main.go`
- 客户端：`openfield/lib/pages/account/tasks_page.dart`

## 任务类型

`tasks.kind` 新增 `daily` 取值（`pkg/model/task.go` 的 `TaskKindDaily`）：

| kind     | 语义                                   | cycle_key      |
|----------|----------------------------------------|----------------|
| `once`   | 一次性成就，只能领一次                 | `''`           |
| `streak` | 连续签到里程（3/7/30 天）              | `''`           |
| `daily`  | 当天活动量分档，每天每档可领一次       | `YYYY-MM-DD`   |

`daily` 与 `streak` 的区别：`streak` 的进度是连续签到天数，`daily` 的进度是
**当天**的帖子数或聊天消息数，并且在 `task_completions` 中以当地日期作为
`cycle_key`，所以同一档位第二天可以再次领取。

## 内置档位

| code             | 目标 | 经验 | 金币 | 说明                     |
|------------------|------|------|------|--------------------------|
| `daily_posts_1`  | 1    | 10   | 5    | 今天发布 1 篇帖子        |
| `daily_posts_5`  | 5    | 40   | 25   | 今天发布 5 篇帖子        |
| `daily_posts_10` | 10   | 100  | 60   | 今天发布 10 篇帖子       |
| `daily_posts_20` | 20   | 260  | 160  | 今天发布 20 篇帖子       |
| `daily_chat_25`  | 25   | 30   | 15   | 今天发送 25 条聊天消息   |
| `daily_chat_50`  | 50   | 80   | 45   | 今天发送 50 条聊天消息   |
| `daily_chat_100` | 100  | 200  | 120  | 今天发送 100 条聊天消息  |

档位是**累计**的：当天发了 20 篇帖子，四档都能各领一次；发了 60 条消息
则 `daily_chat_25` 与 `daily_chat_50` 可领，`daily_chat_100` 仍不可领。

奖励经验会经过会员倍率（`model.ApplyMemberExp`），与其它任务一致。

## 进度统计

- 帖子：`posts` 表中 `user_id` 匹配且 `created_at` 落在当地当天区间内的行数。
- 聊天：`messages` 表中 `sender_id` 匹配、`deleted_at IS NULL` 且
  `created_at` 落在当天区间内的行数（撤回/删除的消息不计入）。

当地当天的区间由游戏配置的时区（`GameConfig.Location()`）决定，
`from = 当天 00:00`，`to = from + 1 天`。

## 接口

| 方法 | 路径                            | 说明                             |
|------|---------------------------------|----------------------------------|
| GET  | `/api/v1/tasks`                 | 任务目录 + 当前进度 + 可领取状态 |
| POST | `/api/v1/tasks/daily/:code/claim` | 领取当天的一档每日任务奖励     |

`GET /tasks` 返回的每个任务带 `kind` / `progress` / `target` /
`completed` / `claimable`，客户端按 `kind == "daily"` 分组展示。

领取结果：

- `200` `{"claimed": true, "exp": N, "currency": M}` — 领取成功。
- `404` 任务不存在。
- `409` 当天进度未达该档（`task requirements not met`）。
- `409` 该档今天已领取（`task already claimed today`）。

并发安全：`grantTaskReward` 在事务内先插入 `task_completions`
（`(user_id, task_id, cycle_key)` 唯一约束），唯一冲突即视为已领取并整体回滚，
因此奖励不会重复发放。

## 部署

任务目录通过 `seedTasks()` 以 code 为键 upsert，`kind` 也在更新列表内，
因此**已有部署在下次启动时自动获得新档位**，不需要新增数据库迁移版本。
