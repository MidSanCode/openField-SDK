# Bot 开发文档（社区机器人）

本文面向在 OpenField 上开发 **bot（机器人账号）** 的开发者：如何创建 bot、如何认证、能调用哪些接口、
如何接收实时事件，以及有哪些明确的限制。

文中所有结论都来自服务端源码（`server/`），关键处标注了实现位置，便于核对。
文档末尾有[验证口径](#附录-c验证口径与未验证项)一节，说明哪些结论是静态读代码得出、尚未经过真实联调。

相关文档：[Features](features.md) · [API](API.md) · [Architecture](architecture.md) · [Moderation reports](reports.md)

---

## 目录

1. [Bot 是什么](#1-bot-是什么)
2. [快速上手](#2-快速上手)
3. [认证](#3-认证)
4. [Bot 管理接口](#4-bot-管理接口)
5. [权限模型](#5-权限模型)
6. [能力清单：bot 能调用哪些接口](#6-能力清单bot-能调用哪些接口)
7. [请求与响应约定](#7-请求与响应约定)
8. [实时事件（WebSocket）](#8-实时事件websocket)
9. [实战：聊天机器人](#9-实战聊天机器人)
10. [实战：发帖 / 内容机器人](#10-实战发帖--内容机器人)
11. [E2EE 与聊天](#11-e2ee-与聊天)
12. [上限、封禁与生命周期](#12-上限封禁与生命周期)
13. [错误码速查](#13-错误码速查)
14. [不支持的能力（明确的空白）](#14-不支持的能力明确的空白)
15. [最佳实践](#15-最佳实践)
16. [实现位置索引（维护者用）](#16-实现位置索引维护者用)

附录：[A 完整 Python 示例](#附录-a完整-python-示例) · [B 权限键全表](#附录-b权限键全表) · [C 验证口径](#附录-c验证口径与未验证项)

---

## 1. Bot 是什么

Bot 就是**一个普通的用户行**，只是带了三个标记（migration v17 `bots`，`pkg/database/migration.go:352`）：

| 字段 | 值 | 含义 |
|---|---|---|
| `users.is_bot` | `TRUE` | 机器人标记，客户端在名字旁显示机器人徽章 |
| `users.bot_owner_id` | 创建者（人类）的 user id | 归属关系；`REFERENCES users(id) ON DELETE CASCADE` |
| `users.password_hash` | 24 字节随机数 | 永远匹配不上任何密码，所以 bot **无法交互式登录** |

此外：`role='user'`、`needs_registration=FALSE`（不会被“未完成注册”的门禁拦住），
创建时自动加入内置默认用户组「所有人」（`services/account/internal/handler/bot.go:78`）。

因此：

- Bot **在系统各处都表现为一个正常用户**——发帖、发消息、进群、加营地，署名都是它自己（自己的昵称、头像、徽章）。
- Bot **没有额外的能力**，也没有额外的限制：服务端没有任何一处代码对 `is_bot` 做接口级拦截，唯一的例外是
  **bot 不能再创建 bot**（`bot.go:51`）。
- Bot 的权限完全取决于它所在的用户组，而新建 bot 落在默认组里，所以它拿到的是
  「普通注册用户」的那套权限（见[第 5 节](#5-权限模型)）。

**认证方式**：一个静态 API token（`ofb_` 前缀），而不是账号密码，也不是有时效的 JWT。
Token 与 bot 一一对应（`bot_tokens.user_id` 是主键），重置即旧 token 立即失效。

---

## 2. 快速上手

假设服务端地址是 `$BASE`（本地默认 `http://localhost:8080`，线上通常是 HTTPS 域名）。
所有接口都在网关的 `/api/v1` 之下。

### 2.1 用人类账号创建 bot

创建 bot 用的是**你自己的登录凭据**（不是 bot token）：JWT，或者自己的 bot token 也可以。

```bash
# 用人类账号的 access token
curl -s -X POST "$BASE/api/v1/bots" \
  -H "Authorization: Bearer $MY_ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"username":"weather_watcher","nickname":"天气预报"}'
```

```json
{
  "user": {
    "id": 4242,
    "username": "weather_watcher",
    "nickname": "天气预报",
    "is_bot": true,
    "bot_owner_id": 7,
    "created_at": "2026-10-10T15:20:12Z"
  },
  "token": "ofb_9f2c…（64 位十六进制）"
}
```

> **`token` 只在这里出现这一次。** 服务端只存它的 SHA-256 哈希（`pkg/repository/bot.go:159`），
> 丢了只能重置，找不回来。请立刻写进密钥管理 / 环境变量。

也可以从客户端 App 里创建：账号页 →「我的机器人」→「新建」（`openfield/lib/pages/bots/bots_page.dart`）。

### 2.2 验证 token 能用

```bash
export BOT_TOKEN="ofb_9f2c…"

curl -s "$BASE/api/v1/users/me" -H "Authorization: Bearer $BOT_TOKEN"
```

返回的 `id` 就是 `4242`——**bot 以自己身份行动**，`X-User-ID` 由网关按 token 解析出来，
下游服务看到的是 bot 自己的 id，不是创建者的。

### 2.3 发一条帖子

```bash
curl -s -X POST "$BASE/api/v1/posts" \
  -H "Authorization: Bearer $BOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"content":"今天北京 21°C，晴。","visibility":"public"}'
```

返回 `201` + 帖子对象本身。

### 2.4 发一条聊天消息

先找到会话 id：

```bash
curl -s "$BASE/api/v1/conversations" -H "Authorization: Bearer $BOT_TOKEN"
```

再发：

```bash
curl -s -X POST "$BASE/api/v1/conversations/$CONV_ID/messages" \
  -H "Authorization: Bearer $BOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"content":"收到，我来处理。"}'
```

> 私聊是**同意制**：bot 不能直接给陌生用户发消息。必须先 `POST /api/v1/conversations/start`
> 发出会话请求，对方接受后才存在会话。详见[第 9.4 节](#94-主动私聊必须先发会话请求)。

---

## 3. 认证

### 3.1 用法

每个请求带一个头即可：

```http
Authorization: Bearer ofb_<64 位小写十六进制>
```

Token 固定为 `ofb_` + 32 字节随机数的十六进制（共 **68 个字符**）。前缀 `ofb_` 是网关识别 bot token 的
唯一依据（`repository.BotTokenPrefix`）——网关看到这个前缀就走 bot 解析路径，否则按 JWT 解析。

### 3.2 网关做了什么

`services/gateway/cmd/main.go:530`：

1. 取 `Authorization` 头，剥掉 `Bearer `；
2. 以 `ofb_` 开头 → `repository.ResolveBotToken(token)`；
3. 该函数按 `bot_tokens.token_hash` 做一次带索引的查询，`JOIN users` 校验 `is_bot`，
   命中则返回 bot 的 user id，并顺带更新 `last_used_at`（best-effort，失败不影响请求）；
4. 解析成功 → 网关设置内部头 `X-User-ID: <bot id>` 后转发；
   客户端自己伪造的 `X-User-ID` 会被覆盖/删除，**无法冒充他人**。

**公开路由上也认 bot token**：`GET /api/v1/posts`、`GET /api/v1/posts/:id` 这类不强制登录的接口，
带上 bot token 会让返回结果“个性化”（例如自己的点赞状态），这是刻意支持的（`main.go:493`）。

### 3.3 失败长什么样

| 情况 | 响应 |
|---|---|
| 没有 Authorization 头 | `401 {"error":"missing authorization token"}` |
| token 被重置过 / 不存在 / bot 被封禁 | `401 {"error":"invalid bot token"}` |
| 有 token 但缺少该接口所需权限 | `403 {"error":"permission denied","permission":"<权限键>"}` |

> **封禁的 bot 返回的是 401 而不是 403。** `ResolveBotToken` 在解析阶段就拒绝 `status='banned'`
> 且 `banned_until` 未到期的 bot，所以请求根本走不到后面那条给 JWT 用户的
> `403 {"error":"account is banned","banned":true}` 检查（`main.go:567`）。写客户端时按 401 处理“token 失效”。

---

## 4. Bot 管理接口

这 4 个接口都是**用人类账号的凭据**调用的（管理自己的 bot），全部需要认证。
`services/account/internal/handler/bot.go`、路由见 `router.go:174`。

### `POST /api/v1/bots` — 创建

请求体：

| 字段 | 必填 | 约束 |
|---|---|---|
| `username` | ✅ | `^[a-zA-Z0-9_]{3,32}$`——3~32 位字母、数字、下划线 |
| `nickname` | ✅ | 去掉首尾空白后 1~100 个字符 |

响应 `201`：`{"user": {…}, "token": "ofb_…"}`。

| 状态 | 条件 |
|---|---|
| `400` | username 不合规 / nickname 为空或超长 |
| `401` | 未认证 |
| `403` | 调用者本身是 bot（`{"error":"bot accounts cannot create bots"}`） |
| `403` | 已达数量上限（`{"error":"bot limit reached","limit":10}`） |
| `409` | `{"error":"username already taken"}` |
| `500` | 服务端错误 |

### `GET /api/v1/bots` — 列表

响应 `200`：

```json
{
  "bots": [
    {
      "id": 4242,
      "username": "weather_watcher",
      "nickname": "天气预报",
      "avatar_url": "",
      "created_at": "2026-10-10T15:20:12Z",
      "token_created_at": "2026-10-10T15:20:12Z"
    }
  ],
  "limit": 10
}
```

`token_created_at` 用于判断“这个 token 有多旧”（重置后等于重置时间；没有 token 记录时回退为 bot 创建时间）。

### `POST /api/v1/bots/:id/regenerate` — 重置 token

响应 `200`：`{"token": "ofb_…新 token…"}`。

旧 token **立即失效**（单行 UPDATE，原子替换），这也是 token 泄露后的唯一补救手段。

| 状态 | 条件 |
|---|---|
| `400` | `:id` 不是正整数 |
| `404` | bot 不存在 |
| `403` | `{"error":"not your bot"}`——bot 存在但不是你的 |

### `DELETE /api/v1/bots/:id` — 删除

响应 `200`：`{"ok": true}`。

**这是不可逆的**：bot 的 user 行被删掉，级联删除它的帖子、回复、消息、附件、会话成员关系和营地成员关系
（`posts.user_id`、`post_replies.user_id`、`messages.sender_id`、`attachments.user_id`、
`conversation_members.user_id` 全部是 `ON DELETE CASCADE`）。客户端的确认框文案也是这个意思。

错误同上（`400/404/403`）。

---

## 5. 权限模型

网关对每个路由声明一个鉴权级别：

| 级别 | 含义 |
|---|---|
| `authPublic` | 公开；带 token 时依然会解析身份以个性化返回 |
| `authRequired` | 必须认证，不检查具体权限项 |
| `authPermission` | 必须认证 + 持有指定权限键，否则 `403` |

新建 bot 会被加入内置默认组「所有人」（`所有人` / `DefaultGroupPermissions`），
其权限 = **全部权限键 减去 7 个管理类键**（`pkg/permission/permission.go:66`）。

**bot 默认拿不到的管理权限**（这些永远不会授予默认组）：

```
account.verify            用户认证（加V）
user.adjust_exp           修改他人经验
user.membership.grant     授予他人会员
user.punish               处罚/封禁他人
wallet.manage             调整他人钱包
plugin.manage             插件商店管理
app.announcements.manage  全站公告管理
```

因此 bot **可以**（取决于具体接口）：发帖/改删自己的帖、发/改删自己的消息、点赞、收藏、关注、
上传附件、建群、加群、发会话请求、加入营地、在营地发帖、举报内容。
**不能**：封禁别人、改别人钱包/经验/会员、认证别人、发全站公告。

### 自查当前权限

```bash
curl -s "$BASE/api/v1/users/me/permissions" -H "Authorization: Bearer $BOT_TOKEN"
```

运营方如果把你移出默认组、或加进了自定义组，这个接口的返回会跟着变——**启动时检查一次比自己假设更可靠**。
完整权限键清单见[附录 B](#附录-b权限键全表)。

另有公开接口 `GET /api/v1/capabilities`（无需认证），列出本服务端构建支持的可选特性
（如 `chat.e2ee`、`camps.subgroups`、`moderation.reports`），可用于探测服务端版本能力。

---

## 6. 能力清单：bot 能调用哪些接口

下表的「鉴权」列取自网关路由表（`services/gateway/cmd/main.go`），是**权威值**。
`authPublic` 表示无需 token 也能调用（带 token 会返回个性化字段）。

### 6.1 账号

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| GET | `/users/me` | `authRequired` | 自己的档案（含 `is_bot`） |
| PUT | `/users/me` | `account.profile.edit` | 改昵称、简介等 |
| GET | `/users/me/permissions` | `authRequired` | 自己的权限键列表 |
| POST | `/users/me/avatar` | `account.avatar.edit` | 上传头像 |
| GET | `/users/search` | `authRequired` | 搜索用户 |
| GET | `/users/:user_id` | `authPublic` | 看别人的档案 |
| POST/DELETE | `/users/:user_id/follow` | `account.follow` | 关注 / 取关 |
| POST | `/reports` | `authRequired` | 举报（见 6.6） |

### 6.2 帖子

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| POST | `/posts` | `posts.create` | 发帖 |
| GET | `/posts` | `authPublic` | 时间线，`page`/`limit` 分页 |
| GET | `/posts/:id` | `authPublic` | 单帖（会记录浏览） |
| PUT | `/posts/:id` | `posts.edit` | 改自己的帖 |
| DELETE | `/posts/:id` | `posts.delete` | 删自己的帖 |
| GET | `/posts/:id/replies` | `authPublic` | 评论列表 |
| POST | `/posts/:id/replies` | `posts.reply.create` | 发评论 |
| PUT | `/posts/:id/replies/:reply_id` | `posts.reply.edit` | 改自己的评论 |
| DELETE | `/posts/:id/replies/:reply_id` | `posts.reply.delete` | 删自己的评论 |
| PUT/DELETE | `/posts/:id/reactions` | `posts.react` | 点赞 / 取消 |
| POST/DELETE | `/posts/:id/favorite` | `posts.favorite` | 收藏 / 取消 |
| GET | `/users/:user_id/posts` | `authPublic` | 某人的帖子 |

### 6.3 聊天

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| GET | `/conversations` | `chat.view` | 自己的会话列表 |
| POST | `/conversations` | `chat.group.create` | 建群 |
| POST | `/conversations/start` | `chat.request.send` | 发起私聊（同意制） |
| GET | `/conversations/public` | `chat.view` | 公开群列表 |
| GET | `/conversations/:id` | `chat.view` | 会话详情 |
| GET | `/conversations/:id/messages` | `chat.view` | 消息列表 |
| POST | `/conversations/:id/messages` | `chat.send` | 发消息 |
| PUT/DELETE | `/conversations/:id/messages/:message_id` | `chat.edit` / `chat.delete` | 改 / 删自己的消息 |
| POST | `/conversations/:id/read` | `chat.view` | 标记已读 |
| POST | `/conversations/:id/typing` | `chat.view` | 正在输入 |
| POST | `/conversations/:id/join` | `chat.group.manage` | 加入公开群 |
| POST | `/conversations/:id/invite` | `chat.group.invite` | 邀请成员 |
| POST | `/conversations/:id/leave` | `chat.group.manage` | 退群 |

### 6.4 营地（Camps）

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| GET | `/camps` | `authPublic` | 营地目录（不含隐藏营地） |
| POST | `/camps` | `authRequired` | 建营地（受创建配额限制，仅顶级营地） |
| GET | `/camps/:id` | `authPublic` | 营地详情（隐藏营地非成员 404） |
| PUT/DELETE | `/camps/:id` | `authRequired` | 改 / 删（需 owner/admin；删除仅 owner） |
| GET | `/camps/:id/posts` | `authPublic` | 营地帖子（成员可见） |
| POST | `/camps/:id/join` | `authRequired` | 加入 |
| DELETE | `/camps/:id/members/me` | `authRequired` | 退出 |
| GET | `/camps/:id/members` | `authRequired` | 成员名单（仅成员） |
| GET | `/camps/:id/subgroups` | `authPublic` | 子群列表（**处理层只允许父营地成员**，其他 404） |
| POST | `/camps/:id/subgroups` | `authRequired` | 建子群（需父营地 owner/admin） |

> 往营地里发帖：`POST /posts` 带 `camp_id`。**必须先加入该营地**，否则
> `403 {"error":"join the camp before posting into it"}`；营地里关了「成员可发帖」时只有 owner/admin 能发。

### 6.5 存储

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| POST | `/attachments` | `storage.upload` | 上传文件（**multipart/form-data，字段名 `file`**） |
| GET | `/attachments` | `storage.list` | 附件列表 |
| GET | `/attachments/:id` | `authPublic` | 单个附件 |
| DELETE | `/attachments/:id` | `storage.delete` | 删除 |

上传拿到 `attachment.id` 后，把它放进帖子的 `attachment_ids` 或消息的 `attachment_ids` 即可。

### 6.6 其它

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| POST | `/reports` | `authRequired` | 举报帖子 / 消息 / 用户 |
| GET | `/reports/mine` | `authRequired` | 自己提交过的举报及处理结果 |
| GET | `/notifications` | `authRequired` | 通知收件箱 |
| POST | `/notifications/read` | `authRequired` | 标记已读 |
| GET/POST | `/ws` | 见第 8 节 | 实时事件 |

**举报**（`services/account/internal/handler/report.go`）：

```json
{ "target_type": "post", "target_id": 123, "reason": "垃圾广告" }
```

- `target_type` ∈ `post` | `message` | `user`（其它值 `400`）；
- `reason` 必填、去空白后 ≤500 字符；
- 不能举报自己（对 `user` 类型自己举报自己 → `400`）；
- 目标不存在 → `404`；**同一目标重复举报 → `409`**（每个举报人对每个目标只能同时存在一条待处理举报）；
- 频率上限 **20 条/小时**，超出 → `429`。

---

## 7. 请求与响应约定

### 7.1 请求体与限制

所有 `POST/PUT` 都是 `Content-Type: application/json`（上传附件除外，那是 multipart）。

**发帖** `POST /posts`（`services/posts/internal/handler/post.go:272`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `content` | string | ≤5000 字符；`content` 与 `quoted_post_id` 至少要有一个 |
| `visibility` | string | `public` \| `login` \| `friends` \| `private`，默认 `public` |
| `attachment_ids` | []int64 | 最多 9 个 |
| `check_id` | int64 | 关联红包/抽奖 |
| `tags` | []string | 标签 |
| `quoted_post_id` | int64 | 引用/转发某帖 |
| `camp_id` | int64 | 发到营地；`0` 表示全局 |

`visibility` 语义（`canViewPost`）：`public` 所有人；`login` 任意已登录用户；`friends` 互关；`private` 仅作者；
营地帖仅营地成员与作者可见。

**发评论** `POST /posts/:id/replies`：`{"content": "…"(必填, ≤5000), "parent_id": null, "attachment_ids": []}`。
`parent_id` 必须指向**同一帖子下**存在的评论。

**发消息** `POST /conversations/:id/messages`（`services/chat/internal/handler/message.go:226`）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `content` | string | ≤5000 字符；`content` / `attachment_ids` / `check_id` 至少有一个 |
| `reply_to_id` | int64? | 引用某条消息 |
| `attachment_ids` | []int64 | 最多 9 个 |
| `mentions` | []int64 | @ 的用户 id；`-1` 表示 @everyone |
| `check_id` | int64 | 红包消息 |
| `burn_seconds` | int | 阅后即焚：`0` 或 `5..86400` |

### 7.2 响应信封

| 接口 | 成功响应 |
|---|---|
| `POST /posts`、`POST /posts/:id/replies`、`POST /conversations/:id/messages` | `201` + **对象本身** |
| `GET /posts` | `200 {"posts":[…],"page":1}` |
| `GET /posts/:id/replies` | `200 {"replies":[…],"page":1}` |
| `GET /conversations/:id/messages` | `200 {"messages":[…]}` |
| `GET /conversations` | `200 {"conversations":[…]}` |
| 错误 | `{"error":"…"}`，部分错误带附加字段（如 `{"error":"permission denied","permission":"chat.send"}`） |

### 7.3 分页（注意两种风格不同）

**帖子类：页码分页**

```http
GET /posts?page=1&limit=20
GET /posts?q=关键词&author=alice&tag=news&author_id=7&from=2026-01-01&to=2026-02-01
```

返回里的 `page` 就是当前页；没有 `total`、没有 `has_more`——请求到空数组即为末尾。

**消息：游标分页，且只有「往前翻」**

```http
GET /conversations/:id/messages?limit=50&before=12345
```

- `limit` 默认 50，不在 1..100 之间会被重置为 50；
- `before=<消息 id>` 取**比它更旧**的消息；不传则取最新一页；
- 返回数组是**时间正序（旧 → 新）**，所以取最新一条用 `messages[-1]`；
- **没有 `after` 参数**：想拿「新消息」只能轮询最新一页再按 id 去重，或改用 WebSocket（第 8 节）。

### 7.4 消息对象里几个要留意的字段

`model.Message`（`pkg/model/model.go:574`）中与 bot 逻辑最相关的：

| 字段 | 说明 |
|---|---|
| `sender_id` | 发送者 id；**用它判断“是不是我自己”**，避免自动回复死循环 |
| `sender_is_bot` | 发送者是否 bot——建议忽略其它 bot 的消息，否则两个 bot 会互刷 |
| `kind` | `text` \| `check` \| `system.join` \| `system.leave` \| `system.mute` \| …，**系统消息不要当用户输入** |
| `mentions` | 被 @ 的用户 id 列表，`-1` 表示 @everyone |
| `deleted_at` | 软删除时间；列表接口已经过滤掉已删除消息 |
| `burn_seconds` / `burn_at` | 阅后即焚倒计时 |

---

## 8. 实时事件（WebSocket）

除了轮询，bot 可以连上实时通道接收事件。通道与 App 用的是同一个（push 服务），
事件受众由服务端按会话成员/可见性计算，**bot 是成员就能收到**。

### 8.1 连接方式：先换票，再连

```http
POST /api/v1/ws          （带 Authorization: Bearer ofb_…）
  → 200 {"ticket":"<64 位十六进制>","expires_in":30}

GET  /api/v1/ws?ticket=<ticket>   （WebSocket 升级；URL 用 ws:// 或 wss://）
```

规则（`services/push/internal/handler/ticket.go`、`ws.go`）：

- 票据**一次性**，用掉即失效；
- 有效期 **30 秒**，只够“取票 → 立刻连接”这段间隔；
- 过期、用过、不存在的票据一律拒绝。

> **注意**：升级请求（`GET /api/v1/ws`）在网关侧是 `authTicket` 级别——网关会**先删掉身份头再原样转发**，
> 由 push 服务去兑换票据。所以**只带 token 的裸 `GET`（不带 `ticket`）会拿到 `401`**，
> 即使 token 是有效的。必须走 `POST` 取票这一步。
>
> 这也是设计意图：浏览器无法在 WebSocket 握手时自定义头，而把长期 token 放进 URL 会泄进日志，
> 所以用一次性短票代替。**不要为了省事把 bot token 写进 query string。**

### 8.2 事件格式

连上后首先会收到一句问候：

```json
{"type":"connected"}
```

之后每条消息都是同一个信封（`pkg/events/events.go:19`）：

```json
{
  "type": "chat.message.created",
  "recipients": [7, 4242],
  "data": {
    "id": 9001,
    "conversation_id": 33,
    "sender_id": 7,
    "kind": "text",
    "content": "你好",
    "created_at": "2026-10-10T15:22:41Z"
  }
}
```

`data` 就是对应的资源对象（消息事件给的是消息对象，帖子事件给的是帖子对象）；
上面只列了几个字段，完整字段见 `model.Message`。

### 8.3 事件类型

| `type` | `data` | 受众 |
|---|---|---|
| `chat.message.created` | 消息对象 | 会话内 `active` 成员 |
| `chat.message.updated` | 消息对象 | 同上 |
| `chat.message.deleted` | 消息对象 | 同上 |
| `chat.typing` | `{conversation_id, user_id}` | 会话成员 |
| `chat.conversation.updated` | `{conversation_id}` | 会话成员 |
| `chat.consent.requested` | `{user_id, encrypted, …}` | 被请求方 |
| `chat.e2ee.keys.updated` | 密钥信封元数据 | 会话成员 |
| `post.created` | 帖子对象 | 见下方「可见性」 |
| `post.deleted` | 帖子对象 | 同上 |
| `post.reply.created` / `.updated` / `.deleted` | 评论对象 | 帖子作者等相关方 |

`post.created` 的受众按可见性计算（`postRecipients`）：`private` 只发作者，`friends` 发互关好友 + 作者，
`public` / `login` 是**广播给所有已连接客户端**（`recipients` 为空数组即广播）。

### 8.4 连接维护与注意事项

- 服务端每 **54 秒**发一个 ping，读超时 **60 秒**——bot 必须及时回 pong（多数 WebSocket 库自动处理），
  否则连接被断开；
- 每个连接的发送缓冲是 **256 条**，写不进去（消费太慢）会被**直接断开**，不做堆积。请务必及时处理事件，
  代价高的操作丢进自己的队列；
- 事件是 Postgres `NOTIFY` 转发的**尽力而为**投递，不是持久队列（`pkg/events/events.go:25`）：
  服务重启、连接掉线期间的事件**不会补发**。所以：**实时事件用来做低延迟触发，定期用 REST 做对账**。

---

## 9. 实战：聊天机器人

### 9.1 先给自己定位

```python
status, me = bot.request("GET", "/api/v1/users/me")
BOT_ID = me["id"]
```

记住 `BOT_ID`：后面所有“是不是我在说话”的判断都靠它。

### 9.2 轮询版（最简单，无需长连接）

思路：每轮取最新一页，按 id 去重，只处理“比我见过的最大 id 更新、且不是我自己发的”消息。

```python
def poll_loop(bot, conv_id, interval=3.0):
    # 启动时先对齐游标，避免把历史消息全部回一遍
    _, page = bot.request("GET", f"/api/v1/conversations/{conv_id}/messages",
                          query={"limit": 1})
    last_seen = page["messages"][-1]["id"] if page["messages"] else 0

    while True:
        _, page = bot.request("GET", f"/api/v1/conversations/{conv_id}/messages",
                              query={"limit": 50})
        for msg in page["messages"]:
            if msg["id"] <= last_seen:
                continue
            last_seen = msg["id"]
            handle(bot, conv_id, msg)
        time.sleep(interval)


def handle(bot, conv_id, msg):
    if msg["sender_id"] == BOT_ID:            # 自己发的，跳过
        return
    if msg.get("kind") != "text":             # 系统消息（入群/禁言…）不当指令
        return
    if msg.get("sender_is_bot"):              # 别的 bot，忽略，防止互刷
        return
    text = (msg.get("content") or "").strip()
    if text == "/ping":
        bot.request("POST", f"/api/v1/conversations/{conv_id}/messages",
                    body={"content": "pong", "reply_to_id": msg["id"]})
```

三点务必保留：**跳过自己**、**跳过非 `text`**、**跳过其它 bot**。缺一个就可能出现自激循环。

### 9.3 事件版（延迟低）

```python
import json, websockets

async def listen(bot, ws_base):
    while True:
        try:
            _, res = bot.request("POST", "/api/v1/ws")          # 取票
            async with websockets.connect(
                    f"{ws_base}/api/v1/ws?ticket={res['ticket']}") as ws:
                async for raw in ws:
                    evt = json.loads(raw)
                    if evt.get("type") != "chat.message.created":
                        continue
                    data = evt.get("data") or {}
                    if data.get("sender_id") == BOT_ID or data.get("sender_is_bot"):
                        continue
                    ...
        except Exception:
            await asyncio.sleep(5)      # 断线重连；事件不补发，重连后按 9.2 对账
```

### 9.4 主动私聊：必须先发会话请求

私聊是同意制（`services/chat/internal/handler/conversation.go:253`）：

```bash
curl -s -X POST "$BASE/api/v1/conversations/start" \
  -H "Authorization: Bearer $BOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"user_id": 7, "message": "你好，我是机器人，可以帮你查天气。", "encrypted": false}'
```

- 这只是**发出请求**，不创建会话；对方接受后才有会话，之后才能发消息；
- 已有待处理请求时再发 → `409 {"error":"a chat request is already pending"}`；
- 给自己发 → `400 {"error":"cannot start a chat with yourself"}`；
- **`encrypted: true` 请谨慎**，见第 11 节。

向群里加 bot：由群管理员用 `POST /conversations/:id/invite` 邀请；公开群也可以让 bot 自己
`POST /conversations/:id/join`。

---

## 10. 实战：发帖 / 内容机器人

```python
# 一条纯文字帖
bot.request("POST", "/api/v1/posts",
            body={"content": "每日行情：…", "visibility": "public", "tags": ["market"]})

# 带图：先上传，再引用附件 id
aid = upload_file(bot, "chart.png")          # POST /attachments (multipart, 字段名 file)
bot.request("POST", "/api/v1/posts",
            body={"content": "今日图表", "visibility": "public", "attachment_ids": [aid]})

# 转发/引用已有帖子
bot.request("POST", "/api/v1/posts",
            body={"content": "值得一读", "quoted_post_id": 12345})

# 在营地里发（必须先是营地成员）
bot.request("POST", "/api/v1/posts",
            body={"content": "营地专属公告", "camp_id": 66})
```

- 只发不看的定时任务，不需要 WebSocket；
- 想对**新帖**作出反应，可以监听 `post.created`（`public`/`login` 的帖子会广播给所有已连接客户端），
  或者用 `GET /posts?page=1` 轮询——注意帖子是页码分页，判断“新”要靠记录见过的最大 id；
- 发帖内容上限 5000 字符、附件最多 9 个，超了直接 `400`。

---

## 11. E2EE 与聊天

会话可能是**端到端加密**的（`Conversation.Encrypted`，`pkg/model/model.go:500`）：

> `Encrypted` 标记该会话为端到端加密：消息以密文存储，只有持有群密钥的客户端能解密。

对 bot 的含义很直接：

- 在**加密会话**里，`content` 字段拿到的是密文，bot **读不懂**；
- bot 自己发的内容也必须由客户端侧加密，服务端不代劳；
- 想真正参与加密会话，bot 需要实现整套群密钥流程：成员各自发布 X25519 公钥，
  群密钥用每个成员的公钥封装成信封（`E2EEKeyEnvelope`），通过
  `GET/POST /api/v1/conversations/:id/e2ee-keys` 存取并在加人/轮换时更新版本
  （`services/chat/internal/handler/conversation.go:1248`、`:1304`）。这对多数社区 bot 来说代价过高。

**建议**：bot 只在**非加密**会话里工作。启动或加入会话时检查 `conversation.encrypted`，
为 `true` 就跳过并在日志里说明原因，而不是把密文当文本处理（那会产生一堆乱码回复）。

---

## 12. 上限、封禁与生命周期

| 项 | 值 | 来源 |
|---|---|---|
| 单个用户可拥有的 bot 数 | **10** | `repository.MaxBotsPerOwner` |
| 每个 bot 的 token 数 | **1**（重置即替换） | `bot_tokens.user_id` 主键 |
| bot 能否再建 bot | ❌ `403` | `bot.go:51` |
| token 格式 | `ofb_` + 64 位十六进制（68 字符） | `repository.BotTokenPrefix` |
| 待处理举报频率上限 | **20 条/小时** | `repository.MaxReportsPerHour` |
| 通用的“每请求限流” | **服务端没有** | 见下 |

关于限流：服务端在请求路径上**没有**对整个 bot 的通用速率限制——`pkg/ratelimit` 是给登录失败、
支付 PIN 试错这类“错误次数”用的计数器，不作用于正常请求；举报有 20/小时 的独立配额。
但这不代表可以随便刷：运营方完全可能在反向代理层加限流，
而且上面那些“发送缓冲 256 条就断开连接”“消息列表 limit 上限 100”都是实打实的软约束。
**请自觉控制频率，用事件驱动而不是忙轮询。**

**封禁**：bot 可以被运营方处罚（`user.punish` 权限）。被封禁的 bot 在 token 解析阶段即被拒绝，
所有请求返回 `401 {"error":"invalid bot token"}`。封禁到期后 token 自动恢复可用，无需重置。

**生命周期**：

- 删除 bot（或删除 bot 的所有者，`bot_owner_id` 级联）→ bot 及其全部内容（帖子/回复/消息/附件）一并消失；
- 重置 token → 旧 token 立即失效，bot 本身与内容不受影响；
- bot 无权“改嫁”：`bot_owner_id` 不可通过 API 变更——唯一能改自己的接口 `PUT /users/me`
  只接受 `username` / `nickname` / `bio` 三个字段（`services/account/internal/handler/user.go:343`），
  改不到 bot 标记与归属。

---

## 13. 错误码速查

| 状态 | 典型响应体 | 常见原因与处理 |
|---|---|---|
| `400` | `{"error":"missing file field"}` | 附件上传没带 `file` 字段 / 字段类型不对 |
| `400` | `{"error":"invalid request body"}` | JSON 解析失败、必填字段缺失 |
| `400` | `{"error":"content exceeds maximum length of 5000"}` | 内容超长 |
| `400` | `{"error":"too many attachments (max 9)"}` | 附件超 9 个 |
| `400` | `{"error":"unknown visibility"}` | `visibility` 不在 `public/login/friends/private` 里 |
| `400` | `{"error":"message cannot be empty"}` | 消息三个字段（content/attachments/check）全空 |
| `400` | `{"error":"invalid burn_seconds (allowed: 0, or 5..86400)"}` | 阅后即焚秒数非法 |
| `401` | `{"error":"missing authorization token"}` | 没带 `Authorization` 头 |
| `401` | `{"error":"invalid bot token"}` | token 被重置/不存在/**bot 被封禁** → 需重新取 token 或联系运营 |
| `403` | `{"error":"permission denied","permission":"chat.send"}` | 缺权限（看 `permission` 字段） |
| `403` | `{"error":"bot accounts cannot create bots"}` | 用 bot token 去建 bot |
| `403` | `{"error":"bot limit reached","limit":10}` | 已达 10 个上限 |
| `403` | `{"error":"not your bot"}` | 操作别人的 bot |
| `403` | `{"error":"join the camp before posting into it"}` | 未加入营地就发营地帖 |
| `403` | `{"error":"only camp admins may post in this camp"}` | 营地关闭了成员发帖 |
| `403` | `{"error":"you are not a member of this conversation"}` | 读非公开群的消息 |
| `404` | `{"error":"bot not found"}` | bot id 不存在 |
| `404` | `{"error":"post not found"}` | 帖子不存在或对当前身份不可见 |
| `409` | `{"error":"username already taken"}` | 建 bot 时用户名被占 |
| `409` | `{"error":"a chat request is already pending"}` | 重复发起私聊请求 |
| `409` | `{"error":"you have already reported this"}` | 重复举报同一目标 |
| `429` | `{"error":"too many reports, please try again later"}` | 举报超过 20 条/小时 |
| `500` | `{"error":"…"}` | 服务端故障，可退避重试 |

---

## 14. 不支持的能力（明确的空白）

写 bot 之前请先确认需求没有落在这里——这些是**当前实现里确实没有**的：

- **没有出站 Webhook / HTTP 回调**。服务端只会通过 WebSocket 推送，不会主动 POST 到你的地址。
  想接收事件只有两条路：连 WebSocket，或者轮询 REST。
- **没有事件订阅过滤**。连上 `/api/v1/ws` 就收你自己该收的全部事件（消息、帖子、输入中……），
  没有“只订阅 mention”这类服务端筛选，也没有事件重放/补发。
- **没有细粒度 token 作用域**。一个 bot 一个 token，权限=该 bot 的权限组，
  不能签发“只能发帖”的子 token。
- **消息没有 `after` 游标**，只有 `before`（往前翻）。取新消息要轮询最新页 + 自己去重。
- **没有斜杠命令 / 交互式组件框架**。命令解析完全由你在 bot 侧实现（服务端只是普通消息）。
- **bot 不能创建 bot**，也不能转让归属。
- **没有公开的 bot 目录**（`GET /bots` 只返回自己的 bot）。
- **审核类能力不可得**：默认组没有 `user.punish`，bot 不能封禁/处罚用户，只能举报。

---

## 15. 最佳实践

1. **Token 只放环境变量 / 密钥管理**，不要进代码库、日志或 URL query；泄露立刻 regenerate。
2. **启动先自检**：`GET /users/me` 确认身份、`GET /users/me/permissions` 确认能力集，
   拿到 `401` 就退出并告警，而不是拿去刷请求。
3. **一个功能一个 bot**：便于按 bot 追溯内容、按 bot 重置 token / 停用。
4. **防自激三连**：跳过 `sender_id == BOT_ID`、跳过 `sender_is_bot`、跳过 `kind != "text"`。
5. **首选事件，轮询兜底**：WebSocket 做低延迟触发，定时全量对账补齐掉线期间的事件。
6. **退避重试**：对 `5xx` 用指数退避；对 `429` 按小时窗口等；**不要对 `401/403` 盲目重试**。
7. **判“是不是新内容”靠 id**，不要靠时间戳（同一秒内多条消息时间戳可能相同）。
8. **别碰加密会话**：`encrypted: true` 的会话跳过，除非你真实现了 E2EE 密钥流程。
9. **注意删除的连带影响**：删 bot 会连带删掉它发过的所有帖子和消息；运营方谨慎操作。
10. **态度上别假装人类**：bot 有机器人徽章，社区规则通常要求自动账号行为可识别。

---

## 16. 实现位置索引（维护者用）

| 关注点 | 位置 |
|---|---|
| bot 创建/列表/重置/删除接口 | `services/account/internal/handler/bot.go` |
| bot 路由注册 | `services/account/internal/handler/router.go:174` |
| bot 存储、token 哈希、解析、归属校验 | `pkg/repository/bot.go` |
| 表结构（`is_bot`、`bot_owner_id`、`bot_tokens`） | `pkg/database/migration.go:352`（v17 `bots`） |
| 网关路由表（方法/路径/鉴权级别/权限键） | `services/gateway/cmd/main.go`（bot 段在 `:191`，token 解析在 `:530`） |
| 身份头约定 | `pkg/middleware/auth.go:44`（`X-User-ID`） |
| 权限键定义与默认组权限 | `pkg/permission/permission.go` |
| 实时事件信封与事件类型 | `pkg/events/events.go` |
| WebSocket 连接与票据 | `services/push/internal/handler/{ws.go,ticket.go,hub.go,listener.go}` |
| 帖子/评论/可见性 | `services/posts/internal/handler/post.go` |
| 消息发送/事件广播 | `services/chat/internal/handler/message.go` |
| 举报 | `services/account/internal/handler/report.go`、`pkg/repository/report.go` |

---

## 附录 A：完整 Python 示例

只用标准库（`urllib`），可直接跑。把 `BASE` / `BOT_TOKEN` / `CONV_ID` 换成自己的。

```python
#!/usr/bin/env python3
"""OpenField 社区 bot 示例：连接会话，响应 /ping 与 @提及。"""

import json
import time
import urllib.error
import urllib.parse
import urllib.request


class OpenFieldBot:
    def __init__(self, base_url: str, token: str):
        self.base = base_url.rstrip("/")
        self.token = token

    def request(self, method: str, path: str, body=None, query=None):
        """发一个请求，返回 (status, payload)。HTTP 错误也返回而不抛出。"""
        url = f"{self.base}{path}"
        if query:
            url += "?" + urllib.parse.urlencode(query)
        data = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                raw = resp.read()
                return resp.status, (json.loads(raw) if raw else None)
        except urllib.error.HTTPError as exc:
            raw = exc.read()
            try:
                payload = json.loads(raw) if raw else None
            except json.JSONDecodeError:
                payload = {"error": raw.decode("utf-8", "replace")}
            return exc.code, payload

    # --- 常用封装 ---
    def me(self):
        return self.request("GET", "/api/v1/users/me")

    def send_message(self, conv_id, content, reply_to=None):
        body = {"content": content}
        if reply_to:
            body["reply_to_id"] = reply_to
        return self.request("POST", f"/api/v1/conversations/{conv_id}/messages", body=body)

    def messages(self, conv_id, limit=50, before=0):
        query = {"limit": limit}
        if before:
            query["before"] = before
        return self.request("GET", f"/api/v1/conversations/{conv_id}/messages", query=query)

    def create_post(self, content, visibility="public"):
        return self.request("POST", "/api/v1/posts",
                            body={"content": content, "visibility": visibility})

    def upload(self, path, mime="application/octet-stream"):
        """multipart/form-data 上传，字段名必须是 file。"""
        boundary = "----openfieldbot"
        with open(path, "rb") as fh:
            blob = fh.read()
        parts = [
            f"--{boundary}\r\n".encode(),
            f'Content-Disposition: form-data; name="file"; filename="{path}"\r\n'.encode(),
            f"Content-Type: {mime}\r\n\r\n".encode(),
            blob,
            f"\r\n--{boundary}--\r\n".encode(),
        ]
        req = urllib.request.Request(f"{self.base}/api/v1/attachments",
                                     data=b"".join(parts), method="POST")
        req.add_header("Authorization", f"Bearer {self.token}")
        req.add_header("Content-Type", f"multipart/form-data; boundary={boundary}")
        with urllib.request.urlopen(req, timeout=120) as resp:
            return json.loads(resp.read())


def main():
    bot = OpenFieldBot("http://localhost:8080", "ofb_把这里换成你的token")
    status, me = bot.me()
    if status != 200:
        raise SystemExit(f"bot 认证失败：HTTP {status} {me}")
    bot_id = me["id"]
    print(f"已登录：{me['nickname']} (#{bot_id}, is_bot={me.get('is_bot')})")

    conv_id = 33  # 换成你的会话 id

    # 对齐游标，避免回复历史消息
    _, page = bot.messages(conv_id, limit=1)
    last_seen = page["messages"][-1]["id"] if page.get("messages") else 0
    print(f"当前最大消息 id = {last_seen}")

    while True:
        status, page = bot.messages(conv_id, limit=50)
        if status != 200:
            print(f"拉取失败 HTTP {status} {page}，稍后重试")
            time.sleep(10)
            continue

        for msg in page.get("messages", []):
            if msg["id"] <= last_seen:
                continue
            last_seen = msg["id"]

            # 防自激：跳过自己、其它 bot、以及系统消息
            if msg.get("sender_id") == bot_id or msg.get("sender_is_bot"):
                continue
            if msg.get("kind") != "text":
                continue

            text = (msg.get("content") or "").strip()
            mentioned = bot_id in (msg.get("mentions") or []) or -1 in (msg.get("mentions") or [])
            if text == "/ping":
                bot.send_message(conv_id, "pong", reply_to=msg["id"])
            elif mentioned and "天气" in text:
                bot.send_message(conv_id, "今天晴，21°C。", reply_to=msg["id"])

        time.sleep(3)


if __name__ == "__main__":
    main()
```

---

## 附录 B：权限键全表

默认组（新建 bot 所在组）持有的权限 = 下表**除「管理权限」外的全部**。

| 域 | 权限键 | 默认组 |
|---|---|---|
| 账号 | `account.view` | ✅ |
| 账号 | `account.profile.edit` | ✅ |
| 账号 | `account.avatar.edit` | ✅ |
| 账号 | `account.banner.edit` | ✅ |
| 账号 | `account.follow` | ✅ |
| 账号 | `account.verify` | ❌ 管理 |
| 账号 | `user.adjust_exp` | ❌ 管理 |
| 账号 | `user.membership.grant` | ❌ 管理 |
| 账号 | `user.punish` | ❌ 管理 |
| 存储 | `storage.upload` / `storage.list` / `storage.get` / `storage.delete` | ✅ |
| 聊天 | `chat.view` / `chat.send` / `chat.edit` / `chat.delete` | ✅ |
| 聊天 | `chat.request.send` / `chat.request.approve` | ✅ |
| 聊天 | `chat.group.create` / `chat.group.invite` / `chat.group.manage` | ✅ |
| 聊天 | `chat.note.edit` / `chat.group.nickname` | ✅ |
| 帖子 | `posts.view` / `posts.create` / `posts.edit` / `posts.delete` | ✅ |
| 帖子 | `posts.reply.create` / `posts.reply.edit` / `posts.reply.delete` | ✅ |
| 帖子 | `posts.react` / `posts.favorite` | ✅ |
| 钱包 | `wallet.view` | ✅ |
| 钱包 | `wallet.manage` | ❌ 管理 |
| 插件 | `plugin.manage` | ❌ 管理 |
| 公告 | `app.announcements.manage` | ❌ 管理 |

---

## 附录 C：验证口径与未验证项

**本文档的事实来源**：逐行阅读 `server/` 源码（接口签名、SQL、路由表、权限常量、迁移定义），
而非运行联调。文中标注的 `file:line` 都可以直接跳过去核对。

**尚未经过真实环境验证的部分**（写 bot 时请以实际联调为准，如有出入请更新本文档）：

- **WebSocket 端到端连通**：票据流程、`{"type":"connected"}` 问候帧、事件信封格式均来自源码，
  但没有在真实网关（尤其是带反向代理、TLS 的环境）上跑过一次完整往返；
  特别是代理对 WebSocket 升级与 30 秒票据窗口的影响。
- **封禁 bot 返回 `401`**：由“token 解析早于封禁检查”的代码顺序推出，未实测。
- **各类上限的具体表现**：发送缓冲 256 条导致的断连、限流相关的实际吞吐，未压测。
- **`post.created` 广播范围**：`public`/`login` 帖子广播给所有已连接客户端的结论来自
  `postRecipients` 返回 `nil` 的实现，未在多客户端环境观测。
- **附件的 multipart 细节**（字段名 `file`）来自 `c.Request.FormFile("file")`，
  未验证单请求大小上限与允许的 MIME 类型（后者由服务端存储配置决定）。

如需把这份文档从“读码结论”升级为“实测结论”，最小验证集是：
用两个真实账号 + 一个 bot 跑通「建 bot → 发帖 → 进群 → 收发消息 → 断线重连」，
以及一次刻意触发的 `401`/`403`/`409`/`429`。
