# 备忘录云同步 REST API 规范

客户端（linbox）已实现，本文档供后端开发者参考。

---

## 认证

所有请求使用 **HTTP Basic Auth**：

```
Authorization: Basic base64(username:password)
```

## 通用约定

- 时间戳：Unix 秒级，**字符串格式**（如 `"1726400000"`）
- 编码：请求和响应均为 `Content-Type: application/json`
- 字符集：UTF-8
- 错误响应：HTTP 4xx/5xx，body 为 `{"error": "错误描述"}`

---

## 数据结构

### CloudEntry（条目）

```json
{
  "id": "32位十六进制字符串",
  "title": "条目标题",
  "content": "正文纯文本（U+FFFC 表示图片占位符）",
  "created_at": "unix秒时间戳",
  "modified_at": "unix秒时间戳",
  "inline_images": ["base64编码的PNG图片", "..."],
  "attachments": [
    {
      "name": "报告.docx",
      "data": "base64编码的文件内容"
    }
  ],
  "deleted_at": "可选，非空表示已删除的时间戳"
}
```

字段说明：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `id` | string | 是 | 32 位十六进制，客户端生成 |
| `title` | string | 是 | 条目标题 |
| `content` | string | 是 | 正文纯文本，`U+FFFC` 标记图片位置 |
| `created_at` | string | 是 | 创建时间（unix 秒） |
| `modified_at` | string | 是 | 最后修改时间（unix 秒） |
| `inline_images` | string[] | 否 | 正文内嵌图片，base64(PNG)，顺序对应 content 中的 U+FFFC |
| `attachments` | object[] | 否 | 附件列表，每个含 `name`（文件名）和 `data`（base64） |
| `deleted_at` | string | 否 | 逻辑删除标记 |

### SyncRequest（同步请求体）

```json
{
  "entries": [ ... ],
  "deleted_ids": ["被删除的条目ID"]
}
```

### SyncResponse（同步响应体）

```json
{
  "entries": [ ... ],
  "deleted_ids": ["被删除的条目ID"],
  "server_time": 1726400000
}
```

---

## API 端点

### 1. 连接测试

```
GET /api/ping

→ 200 OK（无 body）
```

验证认证凭据和服务器连通性。

---

### 2. 批量同步（核心端点）

#### 拉取远端变更

```
GET /api/sync?since={timestamp}

→ 200 OK
{
  "entries": [ ... ],
  "deleted_ids": [ ... ],
  "server_time": 1726400000
}
```

返回 `modified_at > since` 的所有条目，以及 `since` 之后被删除的条目 ID。
首次同步传 `since=0`，获取全部条目。

#### 推送本地变更

```
POST /api/sync
Content-Type: application/json

{
  "entries": [ ... ],
  "deleted_ids": [ ... ]
}

→ 200 OK
```

服务端应在一个事务中处理所有 entries（创建或更新）和 deleted_ids（删除）。

---

### 3. 单条目操作（可选，用于调试）

```
GET    /api/entries/{id}     → 200: CloudEntry | 404
PUT    /api/entries/{id}     → 200/201: OK
DELETE /api/entries/{id}     → 200 | 404
```

---

## 同步流程

客户端每次同步执行以下步骤：

```
1. 读取本地 last_sync 时间戳
2. GET  /api/sync?since={last_sync}  → 获取远端变更
3. POST /api/sync                    → 推送本地变更
4. 处理远端变更（写入本地存储）
5. 更新 last_sync = 响应中的 server_time
```

### 冲突解决策略

**Last-Write-Wins（按 modified_at 时间戳，大者胜出）**

- 条目同时存在于本地和远端 → 比较 `modified_at`，保留较新的版本
- 远端删除 + 本地有修改 → 保留本地（远端删除被忽略）
- 本地删除 + 远端有修改 → 保留远端（下载覆盖本地删除）

---

## 后端实现要点

1. **数据存储**：每个条目按 `id` 为主键存储，需要持久化 `modified_at` 字段
2. **增量查询**：`GET /api/sync?since=T` 高效返回 `modified_at > T` 的记录
   - 建议在 `modified_at` 列上建索引
3. **批量写入**：`POST /api/sync` 应在一个事务中处理，保证原子性
4. **server_time**：服务端当前时间戳，客户端用它更新 `last_sync`
5. **base64 缡码**：`inline_images` 和 `attachments[].data` 是 base64 编码的原始文件内容
6. **无状态**：服务端不需要维护会话，`since` 参数由客户端传入
7. **设备区分**（可选）：客户端有 `device_id` 字段，服务端可记录来源设备用于审计

### 数据库表参考

```sql
CREATE TABLE notepad_entries (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    content     TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    modified_at TEXT NOT NULL,
    inline_images TEXT DEFAULT '[]',   -- JSON array of base64 strings
    attachments   TEXT DEFAULT '[]',   -- JSON array of {name, data} objects
    deleted_at  TEXT                   -- NULL = active, non-NULL = deleted
);

CREATE INDEX idx_notepad_modified ON notepad_entries(modified_at);

-- 同步查询
SELECT * FROM notepad_entries WHERE modified_at > ?;

-- 批量更新/插入（SQLite 示例）
INSERT OR REPLACE INTO notepad_entries (id, title, content, created_at, modified_at, inline_images, attachments, deleted_at)
VALUES (?, ?, ?, ?, ?, ?, ?, ?);
```

---

## 客户端存储位置

| 文件 | 路径 | 说明 |
|------|------|------|
| 连接配置 | `~/.config/linbox/notepad/cloud.json` | URL、用户名、密码 |
| 同步状态 | `~/.config/linbox/notepad/sync-state.json` | last_sync、device_id |
| 删除记录 | `~/.config/linbox/notepad/deleted-ids.json` | 待同步的本地删除 ID |

### cloud.json 格式

```json
{
  "url": "http://192.168.1.100:8080",
  "username": "admin",
  "password": "***"
}
```

### sync-state.json 格式

```json
{
  "last_sync": 1726400000,
  "device_id": "a1b2c3d4e5f6a7b8"
}
```

---

## 条目数据格式规范（第三方兼容开发指南）

本文节定义条目的完整数据格式，适用于：
- 自建 HTTP 后端的开发者
- 直接读写 Git 仓库的第三方应用
- 本地 ZIP 导入导出的解析器

### 条目 ID

- 格式：32 位小写十六进制字符串（128 bit 随机数）
- 示例：`a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6`
- 全局唯一，客户端生成

### meta.json（元信息）

```json
{
  "id": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
  "title": "会议记录",
  "created_at": "1726400000",
  "modified_at": "1726400500"
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | string | 32 位 hex ID |
| `title` | string | 条目标题 |
| `created_at` | string | 创建时间，unix 秒级时间戳（字符串） |
| `modified_at` | string | 最后修改时间，unix 秒级时间戳（字符串） |

### content.txt（正文）

纯文本文件，UTF-8 编码。

图片位置用 **U+FFFC**（Object Replacement Character，`\u{FFFC}`）标记。
每个 U+FFFC 对应 `inline/` 目录下的一张图片，按出现顺序编号。

示例：`今天开会\u{FFFC}讨论了\u{FFFC}几个问题`

表示正文中有 2 张内嵌图片，分别对应 `inline/0.png` 和 `inline/1.png`。

**规则：**
- 保存正文时，遇到 TextBuffer 中的图片（GdkTexture），写入一个 U+FFFC，并把图片 PNG 字节存入 `inline/<序号>.png`
- 加载正文时，遇到 U+FFFC，从 `inline/` 读取对应序号的 PNG 文件，插入为图片
- 图片丢失时保留 U+FFFC 占位符（防止后续保存时图片位置偏移）
- 正文可完全脱离图片独立阅读（纯文本兼容）

### inline/ 目录（内嵌图片）

```
inline/
  0.png     # 第 1 张（对应 content.txt 中第 1 个 U+FFFC）
  1.png     # 第 2 张
  2.png     # 第 3 张
```

- 文件名：`<序号>.png`（从 0 开始的整数）
- 格式：PNG（插入时从 GdkTexture 导出为 PNG）
- 顺序必须与 content.txt 中的 U+FFFC 严格对应
- 可以为空目录（没有图片的条目）

### files/ 目录（附件）

```
files/
  报告.docx       # 任意文件，保留原始文件名
  slides.pptx
  screenshot.png  # 图片也可以作为附件（与 inline 不同）
```

- 文件名：保留用户选择时的原始文件名
- 同名文件自动追加 `-1`、`-2` 后缀（不覆盖）
- 文件类型不限：docx、pptx、pdf、png、mp4、任意格式
- 附件与正文完全独立（附件不在正文中显示）

### 完整条目示例

本地 ZIP 文件 `<id>.zip` 内部：

```
meta.json                 # 4字段 JSON
content.txt               # "第一段\u{FFFC}第二段"
inline/
  0.png                   # 第 1 张图（12KB）
  1.png                   # 第 2 张图（8KB）
files/
  会议纪要.docx           # 附件（25KB）
  数据.xlsx               # 附件（15KB）
```

对应 meta.json：
```json
{
  "id": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
  "title": "会议记录",
  "created_at": "1726400000",
  "modified_at": "1726400500"
}
```

对应 content.txt（实际字节）：
```
第一段\uFFFC第二段
```

---

## 本地存储格式

每个条目是一个独立的 ZIP 文件：`~/.local/share/linbox/notepad/entries/<id>.zip`

ZIP 内部结构见上方「条目数据格式规范」。

导出时将所有 `<id>.zip` 打包进一个大 ZIP 文件。

导入时从大 ZIP 中提取 `<id>.zip` 文件（已有 ID 跳过）。

---

## 同步后端类型

客户端支持两种同步后端，可混合使用（同时配置多个 HTTP 服务器和 Git 仓库）：

### 1. HTTP REST API（自建服务器）

适用于有自建服务器能力的用户。后端实现参见本文档上半部分。

配置格式：
```json
{
  "type": "http",
  "url": "http://192.168.1.100:8080",
  "username": "admin",
  "password": "***"
}
```

### 2. Git 仓库（GitHub / Gitee / GitLab / 自建 Git 服务器）

适用于不想自建服务器的用户，直接用 Git 仓库作为存储后端。

#### 工作原理

- 每个备忘录条目是仓库中的一个目录：`<id>/meta.json`, `<id>/content.txt`, `<id>/inline/*.png`, `<id>/files/*`
- 同步时：`git pull --rebase` → 比较本地与远端 → 合并（按 `modified_at` 取最新）→ `git commit` → `git push`
- 冲突解决：与 HTTP 后端一致，按 `modified_at` 时间戳，全局最晚者胜出

#### 仓库内目录结构

```
<repo>/
  <id1>/
    meta.json
    content.txt
    inline/0.png
    files/report.docx
  <id2>/
    meta.json
    content.txt
    inline/0.png
  ...
```

#### 认证方式

- **SSH 密钥**（推荐）：用户配置了 SSH 密钥后无需输入密码，客户端直接用 `git@github.com:user/repo.git` 格式的 URL
- **HTTPS + 用户名密码/Token**：用户填入 HTTPS URL + 用户名 + 密码/Personal Access Token
- 客户端验证逻辑：先尝试原样 URL（可能是 SSH），失败后尝试 HTTPS 匿名；如果提供了用户名密码则直接用 HTTPS + 凭证

#### 配置格式

```json
{
  "type": "git",
  "url": "git@github.com:user/notepad-backup.git",
  "branch": "main",
  "username": "",
  "password": ""
}
```

#### 如何搭建 Git 存储后端

**方案 A：GitHub/Gitee/GitLab 免费仓库**

1. 在 GitHub/Gitee/GitLab 创建一个私有仓库（如 `notepad-backup`）
2. 在 linbox 备忘录设置中添加该 Git 仓库
3. 填入仓库地址（SSH 或 HTTPS）+ 认证信息
4. 点击「验证仓库」确认可访问后添加

**方案 B：自建 Git 服务器**

1. 任意 Linux 服务器上创建 bare repo：
   ```bash
   git init --bare /srv/git/notepad-backup.git
   ```
2. 配置 SSH 访问（公钥认证）
3. 在 linbox 中添加该仓库地址：
   ```
   git@your-server:/srv/git/notepad-backup.git
   ```

**方案 C：局域网共享**

1. NAS 或任意电脑上创建 git repo
2. 通过 SSH 或局域网 URL 访问

#### 多仓库同步

与 HTTP 后端一样，可同时配置多个 Git 仓库。同步时向所有仓库推送、从所有仓库拉取，冲突按 `modified_at` 全局取最新。

---

## 配置文件格式

客户端配置文件：`~/.config/linbox/notepad/cloud.json`

支持混合配置 HTTP 和 Git 后端：

```json
[
  {
    "type": "http",
    "url": "http://192.168.1.100:8080",
    "username": "admin",
    "password": "***"
  },
  {
    "type": "git",
    "url": "git@github.com:user/notepad-backup.git",
    "branch": "main",
    "username": "",
    "password": ""
  }
]
```

兼容旧版格式（自动识别）：
- 单个 HTTP 对象 → 自动包装为 `[{"type":"http", ...}]`
- HTTP 对象数组 → 自动转换为 `[{"type":"http", ...}, ...]`
