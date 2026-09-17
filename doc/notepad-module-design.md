# Linbox 备忘录模块开发文档

> **目标读者**：打算在另一个平台/语言上实现同等功能的开发者或 AI。
>
> 本文档基于 Linbox（Rust + GTK4/libadwaita）的完整实现撰写，覆盖数据模型、本地存储、
> 两种云同步后端、UI 交互、生命周期闭环，以及**开发过程中踩过的所有坑和解法**。

---

## 一、概述

备忘录是一个支持**正文编辑 + 图片内联 + 文件附件 + 云同步**的富文本笔记模块。
每个条目独立存储为一个 ZIP，支持两种云后端（可同时配置）：

| 后端 | 同步协议 | 冲突策略 |
|------|----------|----------|
| HTTP | REST API（GET / PUT / DELETE） | `modified_at` 秒级 last-write-wins，平手本地胜 |
| Git  | git clone/pull/push             | `modified_at` 秒级 last-write-wins，平手本地胜 |

---

## 二、数据模型

### 2.1 条目元信息（MemoMeta）

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MemoMeta {
    pub id: String,         // UUID
    pub title: String,
    pub created_at: String, // Unix 秒数，字符串格式（如 "1726400000"）
    pub modified_at: String,// 每次写盘时更新，同步比对的核心字段
}
```

### 2.2 条目 ZIP 格式

```
entries/<uuid>.zip
  ├── meta.json
  ├── content.txt           正文（纯文本，U+FFFC = 图片占位）
  ├── inline/0.png          内联图片（第 1 张）
  ├── inline/1.png          内联图片（第 2 张）
  ├── inline/2.png
  └── files/report.docx    附件（任意文件）
```

- content.txt 里的图片用 U+FFFC（`\u{FFFC}`）占位，按出现顺序对应 `inline/0.png`、`inline/1.png`…
- `GtkTextBuffer::text()` / 普通文本 API **会跳过图片**，返回的字符串里不含 U+FFFC。
  要用 `extract_buffer()` 手动遍历 iter，遇到 `paintable()` 就输出 OBJ 字符。

### 2.3 本地存储路径

```
~/.local/share/linbox/notepad/     (dirs::data_local_dir())
├── entries/
│   ├── <uuid1>.zip
│   ├── <uuid2>.zip
│   └── ...
└── tmp/                           解压出的真实文件
    ├── <id>_报告.docx
    ├── <id>_inline0.png
    └── ...
```

### 2.4 内联图片生命周期

1. 用户粘贴/插入图片 → Pixbuf → `buffer.insert_paintable()` → buffer 里出现一个 paintable 对象（不是 U+FFFC 字符）
2. 保存时 `extract_buffer()` 遍历 buffer，遇到 `paintable()` → 提取像素 → base64 编码 → 存进 `inline/{index}.png`
3. 加载时 `fill_buffer()` 遍历 content.txt，遇到 U+FFFC → `storage::read_inline()` → Pixbuf → `insert_paintable()`
4. 写盘判断：计算每张图的指纹，只在指纹变化时才重新写 ZIP

---

## 三、本地存储（storage.rs）

### 3.1 条目 CRUD

| 操作 | 函数 | 说明 |
|------|------|------|
| 创建 | `create(title) -> id` | 生成 UUID，写空条目 ZIP |
| 读元信息 | `load_meta(id) -> MemoMeta` | 读 meta.json |
| 读正文 | `read_content(id) -> String` | 读 content.txt |
| 保存 | `save(id, &meta, content)` | 整体重写 ZIP |
| 列表 | `load_all() -> Vec<MemoMeta>` | 遍历所有 ZIP |
| 删除 | `delete_entry(id)` | 删除 ZIP + tmp 下的解压副本 |

### 3.2 内联图片

| 函数 | 说明 |
|------|------|
| `read_inline(id, index) -> Option<Vec<u8>>` | 从 ZIP 读 inline/{index}.png |
| `save_inline(id, &[Vec<u8>])` | 整体重写 inline/ 目录 |
| `inline_image_path(id, index) -> Option<PathBuf>` | 解压到 tmp/{id}_inline{index}.png |

### 3.3 附件

| 函数 | 说明 |
|------|------|
| `write_file(id, filename, data)` | 追加到 ZIP 的 files/ |
| `read_file(id, filename) -> Option<Vec<u8>>` | 从 ZIP 读 |
| `list_files(id) -> Vec<String>` | 列 files/ 下的文件名 |
| `delete_file(id, filename)` | 从 ZIP 移除 |
| `file_path(id, filename) -> PathBuf` | 解压到 tmp/{id}_{filename} 并返回路径 |

### 3.4 ZIP 导入/导出

- 导出：遍历 entries/ 下所有 .zip → 打成一个合并 ZIP
- 导入：从合并 ZIP 逐个提取，已有 ID 跳过

---

## 四、云同步 — HTTP 后端

### 4.1 API 数据结构

```rust
struct CloudEntry {
    id: String,
    title: String,
    content: String,
    created_at: String,
    modified_at: String,
    inline_images: Vec<String>,   // base64 编码的 PNG
    attachments: Vec<CloudAttachment>,
    deleted_at: Option<String>,   // 逻辑删除
}

struct CloudAttachment {
    name: String,
    data: String,  // base64 编码
}
```

### 4.2 API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/sync` | 拉取所有条目（整体） |
| PUT | `/api/sync` | 推送所有条目（整体替换） |
| DELETE | `/api/sync/{id}` | 逻辑删除 |

所有请求用 **HTTP Basic Auth**（`Authorization: Basic base64(...)`）。

### 4.3 转换函数

**上传（local_to_cloud）**：
```rust
let inline_images: Vec<String> = (0..list_inline(id).len())
    .filter_map(|i| read_inline(id, i).map(|d| B64.encode(&d)))
    .collect();
let attachments: Vec<CloudAttachment> = list_files(id).iter()
    .map(|name| CloudAttachment { name, data: B64.encode(&read_file(id, name)?) })
    .collect();
```

**下载（cloud_to_local）**：
```rust
save(id, &meta, &content);
save_inline(id, &decoded_inlines);
// ⚠️ 先删本地旧附件 → 再写远端的
for name in list_files(id) { delete_file(id, &name); }
for att in &entry.attachments { write_file(id, &att.name, &decoded_data); }
```

### 4.4 冲突策略

```
远端 modified_at > 本地  → 覆盖本地
远端 modified_at == 本地 → 本地胜（不覆盖）
远端 deleted_at 非空     → 检查本地修改时间，本地更新则保留
```

---

## 五、云同步 — Git 后端

### 5.1 仓库结构（与本地 ZIP 格式不同）

```
<repo>/
├── <id>/
│   ├── meta.json
│   ├── content.txt
│   ├── inline/0.png
│   └── files/report.docx
```

### 5.2 push 流程

```
1. ensure_repo() → clone 或 pull
2. 处理 deleted_ids → 从仓库目录删条目
3. 遍历本地所有条目 → write_entry() 写入仓库目录
4. git add -A && git commit
5. git push origin <branch>
6. git ls-remote 验证远端收到（没有这一步会假成功）
```

### 5.3 pull 流程

```
1. ensure_repo() → git pull
2. read_all_entries(work_dir) → 读远端所有 meta.json
3. 远端有本地没有 → apply_remote_entry()
   远端 modified_at > 本地 → apply_remote_entry()
4. apply_remote_entry() 里三件：
   - read_content → storage::save()
   - list_inline + read_inline → storage::save_inline()
   - list_files + read_file → storage::write_file()
```

### 5.4 认证（GIT_ASKPASS）

**不要**把密码嵌在 URL 里（会泄露到 .git/config）。用环境变量：

```rust
// 创建临时 askpass 脚本
let askpass_path = temp_dir().join(format!("linbox-askpass-{id}.sh"));
fs::write(&askpass_path, format!("#!/bin/sh\nexec echo '{password}'"));
fs::set_permissions(&askpass_path, 0o700);
envs.insert("GIT_ASKPASS", askpass_path.to_str());
envs.insert("GIT_TERMINAL_PROMPT", "0");
// Drop 时清理
```

### 5.5 分支名

**坑**：`git init` 后无 commit 时，`rev-parse --abbrev-ref HEAD` 报错（exit 128）。
**解法**：用 `git symbolic-ref --short HEAD`（无 commit 时返回 `master`）。失败返回空串。

### 5.6 push 后验证

```rust
git push origin <branch>;
// 必须验证，否则远端空仓库/认证失败会假成功
let (code, _, _) = git_exec(&work_dir, &["ls-remote", "--exit-code", plain, &format!("refs/heads/{branch}")], config);
if code != 0 { return Err("push 后验证失败") }
```

---

## 六、同步编排（sync.rs）

### 6.1 后端抽象

```rust
enum BackendConfig {
    Http(CloudConfig),  // url + password
    Git(GitConfig),     // url + username + password + branch
}
```

### 6.2 同步触发点

| 触发 | 方向 | 入口 |
|------|------|------|
| App 启动 | 只拉 | `pull_only()`（spawn 线程） |
| 「同步」按钮 | 双向 | `save(force=true)` + `pull_only()` + `push_only()` |
| 「刷新」按钮 | 只拉 | `save(force=true)` + `pull_only()` |
| 「推送」按钮 | 只推 | `save(force=true)` + `push_only()` |
| 关窗 | 双向 | `flush()` + `pull_only()` + `push_only()` |

### 6.3 删除传播

- 本地删除 → 记录到 `deleted_ids`，push 时写 `deleted_at` 标记（HTTP）或删目录（Git）
- 远端删除 → 拉取后对比 `deleted_at`，保护期（默认 7 天）内不删本地
- `clear_deleted_ids()` 门控：只有所有后端成功后才清空

### 6.4 已删除条目同步状态

```rust
// 跟踪哪些后端还没同步删除
let mut unsynced: HashMap<String, HashSet<String>>; // id → 已同步的后端名
```

每次 push 成功后把对应后端加入已同步集合，全部后端都同步完才从本地删。

---

## 七、UI 交互

### 7.1 条目列表与切换

- 左栏 ListBox：标题 + 修改时间
- 新建条目：生成 UUID → refresh_list → select_row_by_id
- 切换条目：`row-selected` → `load_entry()`
- `loading` 标志位防止切换时自动保存回调覆盖内容

### 7.2 正文编辑与自动保存

```
buffer.changed → save_current(inner, false)
                  ├── loading == true  → 跳过
                  ├── 距上次写盘 < 200ms → 节流跳过（⚠️ 无延迟补写）
                  └── 否则 → storage::save() + sync_inline_images()
```

**节流陷阱**：节流窗口内的改动没有定时补写，关窗前必须 `flush()`。

### 7.3 图片内联

**插入**：
- Ctrl+V：捕获剪贴板 → Pixbuf → `insert_paintable()`
- 文件选择器：PNG/JPG → Pixbuf → `insert_paintable()`

**存盘时**（`extract_buffer()`）：
```rust
let mut iter = buffer.start_iter();
while !iter.is_end() {
    if let Some(paintable) = iter.paintable() {
        text.push(OBJ);               // U+FFFC 占位
        images.push(render_pixbuf(&paintable)?); // PNG 字节
    } else {
        text.push(iter.char());
    }
    iter.forward_char();
}
```

**加载时**（`fill_buffer()`）：
```rust
let mut idx = 0;
for ch in content.chars() {
    if ch == OBJ {
        if let Some(data) = read_inline(id, idx) {
            if let Ok(pb) = Pixbuf::from_bytes(&glib::Bytes::from(&data), ...) {
                buffer.insert_paintable(&mut iter, &pb);
            }
        }
        idx += 1;
    } else {
        buffer.insert(&mut iter, &ch.to_string());
    }
}
```

### 7.4 附件

- 添加：FileChooserDialog → `storage::write_file()`
- 选中：单击 FlowBox 子项 → 按钮启用
- 打开：`file_path()`（解压到 tmp/）→ `gio::AppInfo::launch_default_for_uri_async`
- 在文件夹中显示：`file_path()` 的 parent → `launch`
- 删除：二次确认 → `delete_file()`
- FlowBox 双击：用 `child_activated` 信号（不用自挂手势，避免和 FlowBox 内建手势冲突）

### 7.5 URL 处理

**检测**（`scan_url_at_iter`）：
- 用字符下标（不是字节！`TextIter::offset()` 是字符偏移量）
- 词边界匹配（空格/换行/引号/括号等分隔）
- 尾部标点 trim（中文句子 `https://x.com。` 里的句号）
- `www.` 开头自动补 `https://`
- `#fragment` 完整保留

**打开**（`open_url`）：
```rust
// ✅ 原样传 URL，不能走 File::for_path（会把 https:// 规范化成 https:/ 然后当本地路径）
gio::AppInfo::launch_default_for_uri_async(url, None::<&AppLaunchContext>, None::<&Cancellable>, ...)
```

**超链接样式**：
```rust
let tag = TextTag::builder().underline(Underline::Single).foreground(link_color()).build();
buffer.tag_table().add(&tag);
// 每次 changed → scan_url_ranges → remove_tag + apply_tag
// apply_tag 不会发 changed（已核 gtktextbuffer.c），不会递归，也不会置脏标记
```

### 7.6 右键菜单

**附件右键**：`GestureClick(button=3)` 挂在 FlowBox 上（Bubble 阶段）。FlowBox 内建手势只认 button-primary，右键畅通。

**内联图片右键**：**不能**挂手势——`GtkGestureClick` 对 `BUTTON_PRESS` 不返回 TRUE（GTK 源码确认），抢不到。必须用 `gtk_text_view_set_extra_menu()` 官方扩展点，配合 `gio::SimpleActionGroup` 和 `hidden-when=action-disabled`。

### 7.7 URL 悬停

- `EventControllerMotion`：位置 + modifier_state → 算出是否在链接上 + 是否按着 Ctrl → 手型光标或恢复原光标
- `EventControllerKey`（Capture）：`connect_modifiers` 覆盖"鼠标不动只按 Ctrl"的场景
- `tooltip`：`query_tooltip` → 鼠标在链接上时提示"Ctrl+左键 打开链接"

---

## 八、生命周期与数据流

```
App 启动
  │
  ├─ build() → wire() → 自动 pull_only（spawn 线程，完成后刷新列表）
  │
  ├─ 编辑正文
  │    └─ changed → save_current(force=false) → [节流] → save()
  │
  ├─ 添加/删除附件/内联图片
  │    └─ save_current(force=true) 或直接 save_inline()
  │
  ├─ 点击「同步」按钮
  │    └─ save(force=true) → pull_only → push_only
  │
  ├─ 关闭窗体
  │    ├─ 没配云端 / 没改动 → 直接关
  │    └─ 有改动 → "关闭前同步？"对话框
  │         ├─ "保存并同步" → flush() → pull_only → push_only → 关
  │         ├─ "直接关闭" → 关
  │         └─ "取消" → 不关
  │
  └─ App 退出 → shutdown() → 清空全局 TLS
```

---

## 九、开发过程中的问题与解决方案（Q&A）

### Q1: RefCell already borrowed panic

**症状**：点同步/切换条目时 panic，GTK abort。

**根因**：Rust 的 `if let` 临时值生命周期——`borrow()` 返回的 `Ref` 活到整个 `if let` 块结束，块内 `borrow_mut()` 冲突。

```rust
// ❌ Ref 活到块尾 → load_entry 里 borrow_mut panic
if let Some(id) = i.current_id.borrow().clone() {
    load_entry(i, &id);
}
// ✅ 先 bind，Ref 在 let 末尾释放
let cur = i.current_id.borrow().clone();
if let Some(id) = cur {
    load_entry(i, &id);
}
```

**自查**：`grep -n "if let Some.*\.borrow()" src/` —— 每处确认块内无对同一 cell 的 `borrow_mut()`。

**易错**：不要误判成"信号重入"——`with_inner` 用 `try_borrow` + clone Rc，闭包运行时 `Ref` 已释放，嵌套安全。

---

### Q2: GTK4 没有 gtk::Menu——右键菜单用 Popover

`gtk::Menu` / `gtk::MenuItem` 在 GTK4 不存在。用 `Popover` + `Button`：

```rust
let popover = gtk::Popover::new();
popover.set_parent(anchor);
popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1))); // 关键
let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
popover.set_child(Some(&vbox));
let btn = gtk::Button::with_label("打开");
btn.set_has_frame(false);
btn.set_halign(gtk::Align::Fill);
// connect_clicked...
vbox.append(&btn);
popover.popup();
```

---

### Q3: 挂在 TextView 上的右键手势永远不生效

**症状**：Capture 阶段 `GestureClick(button=3)`，右键还是出现 GTK 内置的剪切/复制菜单。

**根因**（读 GTK 源码确认）：
1. `gtkgesture.c` 的 `gtk_gesture_handle_event`：`GDK_BUTTON_PRESS` 分支**不返回 TRUE**——只有 release 才返回。
2. TextView 内建手势 `set_button(0)`（任意键）→ claim + `do_popup()`。
3. GTK4 的 TextView 菜单不走 `popup-menu` 信号，是自己建 `GtkPopoverMenu`。

**解法**：用 `set_extra_menu()`（官方追加到内置菜单后面）+ `hidden-when=action-disabled`（没选中图片时隐藏）。

---

### Q4: URL 打不开——路径被规范成 `https:/`

**根因**：`Path::new(&url)` → `gio::File::for_path` → 规范化双斜杠 → 当本地路径。

**解法**：URL 直接传 `launch_default_for_uri_async`，不走 `File::for_path`。

---

### Q5: `TextIter::offset()` 是字符下标，不是字节

中英混排时拿字节下标切字符串 → URL 高亮和点击位置不匹配。

**解法**：全链路统一用字符：`iter.offset()` 作为 `find_url_at` 的输入；`buffer.iter_at_offset()` 做反向转换。

---

### Q6: 空 Git 仓库 push 报 `源引用规格 main 没有匹配`

`git init` 后无 commit 时，`rev-parse --abbrev-ref HEAD` 直接报错（exit 128）→ `unwrap_or("main")` 兜底拿到伪值 → push 失败。

**解法**：用 `git symbolic-ref --short HEAD`（无 commit 时返回 `master`）。三级兜底。失败返回空串。

---

### Q7: 关窗时最后几个字会丢

**根因**：`save_current(false)` 在 < 200ms 时直接 return，无延迟补写。关窗不调 force。

**解法**：新增 `pub fn flush()`（force 写盘），关窗分支第一行调用。

---

### Q8: GtkTextBuffer apply_tag 不发 changed

从 changed 回调里调 `restyle_links()` → apply_tag → 不会触发 changed → 无递归，无脏标记，无白写盘。

**教训**：写完"防 X"的代码，先把 X 去掉跑一遍测试。测试照样绿 → X 不存在。

---

### Q9: observe_controllers() 按阶段排序

断言时遍历找 KeyController，最后一个覆盖了前面的 → 误判阶段是 Bubble。

**解法**：`phases.contains(Capture)` 替代 `last_matches_phase`。

---

### Q10: hidden-when=action-disabled 的机制

由 `gtkmenutrackeritem.c` 解析 → action 禁用时项从菜单隐藏（不是变灰）→ 启用时自动出现。需在 `notify("has-selection")` + `changed` + `load_entry` 时同步 action 启用状态。

---

### Q11: 右键菜单必须 set_pointing_to

`popover.popup()` 不设 pointing_to 时落在**父控件中央**——大控件上等于飘走。

```rust
popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
popover.popup();
```

---

### Q12: URL 打开要用 launch_default_for_uri，不能用 File::for_path

```rust
// ✅
fn open_url(url: &str) {
    gio::AppInfo::launch_default_for_uri_async(url, None::<&gio::AppLaunchContext>, None::<&gio::Cancellable>, ...);
}
// ❌
fn open_url(url: &str) {
    let uri = gio::File::for_path(url).uri(); // https:// → https:/ → 文件路径
    ...
}
```

---

### Q13: TextTag 有 builder，没有 set_foreground

运行时改属性用 `tag.set_property("foreground", "#3584e4")`。

---

### Q14: 菜单项只能追加在末尾

`set_extra_menu` 追加到 GTK 内置菜单的"全选/插入表情"之后，无法控制顺序。

---

### Q15: 防 X 的守卫/测试必须反向验证

```bash
# 临时去掉被锁的守卫 → 测试必须变红
cp src/file.rs /tmp/bak
sed -i '/守卫代码/d' src/file.rs
cargo test                  # 应该 FAILED
cp /tmp/bak src/file.rs     # 还原
```

---

### Q16: 手势 Capture 阶段 vs Bubble 阶段的选择

| 交互 | 正确阶段 | 原因 |
|------|----------|------|
| 左键双击（内联图片） | Bubble | TextView 左键不 claim |
| 右键菜单（附件） | Bubble | FlowBox 只认 primary，右键畅通 |
| 右键菜单（内联图片） | **不挂手势** | 用 set_extra_menu |
| URL 悬停光标 | Bubble | motion 需要跟在 TextView 后面 |
| Ctrl 检测 | Capture | 键盘事件要优先于文本处理 |

---

## 十、同步冲突完整矩阵

### 双端都有，时间戳不同

| 本地 modified_at | 远端 modified_at | 结果 |
|:---:|:---:|:---|
| 更大 | 更小 | 保留本地，远端会被覆盖 |
| 更小 | 更大 | 覆盖本地 |
| 相等 | 相等 | 本地胜（不覆盖） |

### 一侧有、另一侧没有

| 本地有 | 远端有 | 结果 |
|:---:|:---:|:---|
| ✓ | ✗ | push 上去 |
| ✗ | ✓ | pull 下来 |

### 一侧删除、另一侧修改

| 本地状态 | 远端状态 | 结果 |
|:---|:---|:---|
| 已删除（deleted_at） | 有更新 | 保留远端 |
| 已删除 | 无更新 | 删除远端 |
| 有修改 | 已删除（deleted_at） | 保护期 7 天内保留本地 |
| 无修改 | 已删除 | 删除本地 |

---

## 十一、移植检查清单

### 数据层
- [ ] MemoMeta 结构（id/title/created_at/modified_at）
- [ ] 本地 ZIP 格式（meta.json + content.txt + inline/ + files/）
- [ ] 条目 CRUD
- [ ] 内联图片读写 + 落盘一致性（与附件同一 tmp 目录）
- [ ] 附件读写 + 解压到 tmp/
- [ ] content.txt ↔ buffer 转换（extract_buffer / fill_buffer，处理 OBJ ↔ paintable）
- [ ] 写盘指纹比对（sync_inline_images）

### 云同步
- [ ] HTTP 后端（CloudEntry 序列化 / GET+PUT / Basic Auth）
- [ ] Git 后端（clone/pull/push/commit/ls-remote 验证）
- [ ] GIT_ASKPASS 认证
- [ ] modified_at 冲突对比
- [ ] 删除传播 + 保护期 7 天
- [ ] clear_deleted_ids 门控（全部成功后才清）
- [ ] 同步触发（启动拉取 / 按钮 / 关窗 flush+同步）

### UI 交互
- [ ] 条目列表 + 切换 + loading 防覆盖
- [ ] 自动保存（200ms 节流 + 关窗 flush）
- [ ] 图片粘贴/插入 → insert_paintable
- [ ] content.txt ↔ buffer 往返（extract_buffer / fill_buffer）
- [ ] 附件添加/选中/打开/在文件夹中显示/删除 + 按钮状态联动
- [ ] 单击内联图片选中（1 字符选区，不覆盖拖选）
- [ ] 双击内联图片打开
- [ ] 内联图片右键菜单（set_extra_menu + hidden-when）
- [ ] URL 检测 + 超链接样式（TextTag，apply_tag 不发 changed）
- [ ] URL Ctrl+点击（launch_default_for_uri，不是 Path）
- [ ] URL 悬停（手型光标 + tooltip + connect_modifiers）
- [ ] 附件右键菜单（Popover + set_pointing_to）

### 常见陷阱预防
- [ ] `if let Some(x) = cell.borrow().clone()` → 先 let 再 if let
- [ ] URL 打开走 `launch_default_for_uri`，不用 `File::for_path`
- [ ] `TextIter::offset()` 是字符下标，不是字节
- [ ] `apply_tag` 不发 `changed`，可以从 changed 回调安全调用
- [ ] `observe_controllers()` 按阶段排序，断言用 `contains` 不要最后覆盖
- [ ] `gdk_event_get_position` 返回 surface 坐标；手势回调 x/y 是控件坐标
- [ ] `Popover.popup()` 必须 `set_pointing_to`
- [ ] 关窗必须 `flush()`（200ms 节流无补写定时器）
- [ ] 右键菜单挂 GestureClick 在 TextView 上无效，用 `set_extra_menu()`
- [ ] `TextTag` 有 builder，运行时用 `set_property` 改属性

---

*文档版本：2026-09-16 / Linbox 备忘录模块*
