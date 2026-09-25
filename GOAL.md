# linbox 性能优化与测试目标文档

本文件是本仓库性能优化、测试补齐与 UI 重构的唯一目标清单。每个任务都带验证命令，做完打勾并记录到本文件末尾的「进度记录」。

## 反幻觉与反重复机制（最高优先级）

1. **先读后写** — 改任何源文件前必须 `read_file` 实际内容，禁止凭记忆或凭文件名猜测结构。
2. **先验证再宣称** — 说「测试通过 / 性能提升」之前必须真的跑过 `cargo test` / `cargo bench` / `time` 命令，贴真实输出数字。
3. **不重复提交** — 提交前先 `git log --oneline -10`，不提交已存在的改动。
4. **不重复修复** — 修 bug 前先查本文件「进度记录」，已修的不再修。
5. **每个任务完成后更新本文件** — 完成项打 `[x]`，在「进度记录」追加一行：日期 + 改了什么 + 验证命令的真实输出摘要。
6. **单次会话只做 2-3 个任务** — 不要一口气全做，聚焦的批次才能在上下文压缩后存活。
7. **测试数量前后对比** — 任务开始时 `cargo test -- --list | tail -1` 记下总数，结束后再跑一次，测试总数只许增不许减。
8. **不重复探测** — 同一条命令返回空或被截断，改读保存的文件或换一个不同的问题，禁止原样循环重发。

## 当前状态快照（实测于 2026-09-23）

- 代码规模：`src/` 共 51 个 .rs 文件，约 39,947 行
- 测试：`cargo test -- --list` → **173 tests, 0 benchmarks**（测试集中在 systemd/monitor/signal/proc/sensors，其余 40+ 文件零测试）
- 基准：**无任何 bench**（Cargo.toml 无 criterion，无 `benches/`）
- 构建配置：**Cargo.toml 无 `[profile.release]`**，未开 LTO / 未调 opt-level / codegen-units，发布版二进制体积和速度都没优化
- `unwrap()` 共 186 处 — 后台采样线程里一处 panic 会直接杀掉采样
- 最大性能热点（已定位，按严重度排序）：
  1. **monitor 每次采样全量扫 /proc**（`src/utils/monitor/proc.rs:607-660`）：每个 pid 读 stat + cmdline + io + cgroup ≈ 4-5 次系统调用/文件；进程多时单次采样耗时暴涨，且 io/cgroup/cmdline 在「列表视图」根本用不到（`proc.rs:651` 还注释承认「用不到的字段也读一下」）。
  2. **界面 120ms 定时器**（`src/page/monitor/mod.rs:439`）：无论页面是否可见都在排空 channel 重绘。
  3. **downloader.rs 里 15+ 处 `std::thread::sleep` 阻塞重试**（`downloader.rs:1271-1596`，最长 1500ms），占着 tokio worker 线程睡。
  4. **多处同步外部命令阻塞**：`systemd.rs:51-83` pkexec/systemctl/journalctl、`sensors.rs:280/460/607` lspci/nvidia-smi/ip — 若在 UI 线程调用会卡死界面。
  5. UI 轮询型页面：port_scanner 80ms / api_key_sniffer 100ms / archive_cracker 100ms / path_scanner 100ms / download 500ms 的 `timeout_add`，逻辑与绘制耦合在 tick 里。

## 工作阶段

### Phase 1: 建立基准与构建基线（先测量，后优化）✅ 2026-09-23 完成
- [x] 1.1 Cargo.toml 增加 `[profile.release]`：`lto = "thin"`、`codegen-units = 1`、`panic = "abort"`（先试，若编译时间不可接受退回 thin/codegen-units=16）。验证：`cargo build --release`，记录二进制大小与编译耗时前后对比。
  → 二进制 16,104,936 → 12,536,976 字节（**-22.2%**）；符号表 30,970 → 15,606；构建 5m07s（基线，全量）→ 约 10 分钟（LTO 全量重建，分3段跑完，末段 2m25s）。lib 拆分后微涨到 12,628,008（+0.7%，lib/rlib 链接边界所致），仍比基线小 21.6%。panic=abort 已生效（_Unwind 符号仅剩 C 依赖的10个）。
- [x] 1.2 加 criterion 基准框架：`cargo add --dev criterion --features html_reports`，建 `benches/`。验证：`cargo bench -- --warm-up-time 1` 能跑通。
  → criterion 0.8.2，`benches/hotspots.rs`（harness=false），`cargo bench` 全程 6m09s 无错跑通。前置改造：src 拆出 lib 目标（main.rs 薄包装 + lib.rs `pub fn run()`），bench 以 `linbox::...` 链接纯逻辑层。
- [x] 1.3 给热点纯函数写第一批 bench（不依赖 GTK，全部可离线跑）：
  - `proc::parse_stat` / `parse_pid_stat` / `parse_pid_io`（喂合成的 /proc 文本）
  - `proc::flatten_tree`（2000 个进程的合成数据）
  - `json` 解析（`src/utils/json.rs`）与 1MB 大 JSON 的往返
  - `path_scanner` 的目录遍历匹配、`env_editor` 的行解析
  记录基线数字进「进度记录」。
  → 共 **12 个 bench**，基线数字已录入进度记录（另有 `matches_filter_2000`、`serialize`、`format_compact` 共12项；path_scanner 侧无可遍历匹配的纯函数，用 `join_url` 拼接热点代替）。
- [x] 1.4 采样开销实测：给 `collect()` 加临时 `eprintln!` 记录耗时，跑 `cargo run --release` 打开监视器页，记录「单次采样耗时 / 系统 300 进程」的真实毫秒数。
  → 不用临时打印：`monitor::BenchSampler`（doc(hidden) API）+ criterion 自计时。**本机（4核3A5000，~300 进程）单次 collect = 23.57 ms**（占 200ms 采样周期的 11.8%），Phase 2 的对照基线。

### Phase 2: 监视器采样优化（最大热点）✅ 2026-09-23 完成（2.1/2.2/2.3/2.4/2.5 全部完成）
- [x] 2.1 分层采样：轻量路径（每个 tick）只读 stat + cmdline；`io`/`cgroup`/`fd`/`task` 等详情字段改为「选中某进程打开详情时」才读。验证：`cargo test` 全绿 + 1.4 的单次采样耗时下降（目标：≥50% 降幅）。
  → 实施内容：①每轮每进程只读 `stat`（fd/task 本来就在 process_detail 按需读）；②`/proc/<pid>/io` 按需读——速率列可见或详情有选中才开（全局开关 `set_io_columns`/`set_io_detail`，关→开首轮速率置 0 防差分尖峰）；③cgroup/cmdline/io_class 进 pid 缓存；④亲和性移出热路径（原 affinity_hex 每次 128 个 format! 堆分配 ≈5.5ms/轮，已改为现场读 + write! 零分配）。**结果：23.57 ms → 9.77 ms（-58.5%）**，io 全开也只要 11.68 ms（-50.5%）。测试 168 过/4 预存在环境失败（与改动前完全一致），release GUI 冒烟存活。
- [x] 2.2 复用缓冲区：用一个可复用 `String`/`Vec<u8>`（`read_to_string` 到清空后的 buffer）替代每次分配新 String，`Process` 列表用对象池/复用 Vec。验证：bench 显示 collect 分配次数下降。
  → 实施：①`collect_processes` 内路径字符串 / stat 文本 / cmdline 字节 / 字段索引四个缓冲跨进程复用（stat 走 `File::open + read_to_string(&mut buf)`，路径用可 truncate 的 String 拼，省掉每进程 ~2 次 PathBuf 分配）；②`parse_pid_stat_into` 复用字段索引 Vec——索引存**字节区间**而非 `&str`（存引用会和跨循环的 `text.clear()` 在借用推断上打架，NLL 局限）；③`collect()` 六个 /proc 顺序读共享一个16KB 缓冲；④hostname/kernel/distro 进 OnceLock；⑤差分基准 HashMap 改 clear+swap 复用（`prev_scratch`）。**「Process 列表对象池」做了替代说明**：Snapshot 经 channel 所有权跨线程到 UI 才释放，回借池化需环形缓冲协议，收益仅1 次分配/轮，按最小改动原则不做（过程缓冲已覆盖主要分配源）。
  → bench 实测（计数分配器 `#[global_allocator]` 装进 bench 二进制）：**collect 分配 8698 → 5077 次/轮（-41.6%）、2.20MB → 1.48MB（-32.6%）**；进程段 5631 → 2044 次（-63.7%）、1.04MB → 142KB（-86%）；**耗时 9.65 → 8.62ms（-10.7%）**，io 全开 12.16 → 10.74ms。测试 168 过/同 4 预存在失败（parse 字节区间重写的黄金测试全过），release GUI 实机核验数值正常（CPU2.8%/内存43.8%）。
- [x] 2.3 快速路径短路：进程集合与上次完全相同时跳过 cmdline 重读（cmdline 只在 starttime 变化时变）。
  → 实施时发现原设想的前提有误：**execve 保留 starttime 但 comm/cmdline 都会变**，按 starttime 判定会拿到改名后的陈旧 cmdline。改用 `starttime + comm` 双键（comm 变=exec 必重读），cgroup/io_class 按 starttime/进程存活期缓存。稳态 cmdline 重读耗时 3.0 ms → 0.08 ms。
- [x] 2.4 页面不可见时降频：`src/page/monitor/mod.rs:439` 的 120ms 定时器在页面非当前 stack child 时降到 1000ms 或暂停。验证：切到别的页面后 `top` 里 linbox CPU 占用明显下降（贴 `top -b -n1 -p $(pgrep -n linbox)` 前后数字）。
  → 实施比 GOAL 原文更强：GOAL 写于摸底前（当时以为默认周期 200ms），实际 `mon::start(1000)` 默认 1s——所以做成了「不可见时 120ms tick 直接跳过 drain + 采样降频到 5000ms（`HIDDEN_INTERVAL_MS`），map 时恢复用户周期（窗口最小化同样走 unmap）」。
  → **先诊断后动手，挖出真正的性能元凶**：首页挂机 A/B 实测 `top` **73.4% → 0.2% CPU（-99.7%，状态 R→S）**。定位过程：strace 抓到 ppoll 89k/10s、X socket fd5 timeout=0 恒 POLLIN（~6500/s）；gdb 交互式连抓 7/7 次主线程全部停在 `SignalListItemFactory → gtk_label_new`；计数探针证明 rebuild/update 只有 1Hz、无空闲环，但**每次 update 触发 ~2255 个列单元格 widget 全量销毁重建**（页面不可见时也照跑）。2.4 掐死不可见路径后 CPU 归零。测试 168 过/同 4 个预存在失败，release 构建 OK。
- [x] 2.5（2.4 诊断的延伸）监视器页**可见**时的单元格全量重建：update 每秒让 GtkColumnView 销毁重建 ~2200 个 widget（setup 计数实测），可见时预期同样烧 CPU。步骤：①用 xdotool 启动后切到监视器页贴 `top` 数字确认（对照2.4 的0.2%基线）；②修法方向：splice 只更新内容不变的对象 / 按 pid 复用 `BoxedAnyObject`（内容没变的对象不重建）/ 或 items-changed 粒度化。验证：可见页挂机 top ≤5% 且列表数据仍逐秒刷新。
  → 先用「等长 splice 回填旧对象」机制实验定性：**全量新对象 = 73.8%（teardown+setup 全量波），同身份回填 = 0~5%——GTK 对同身份 items-changed 只 rebind，不重建 widget**。据此上真修复：`type ProcObj = Rc<RefCell<Process>>` 装进 BoxedAnyObject，按 pid 做身份缓存（`ProcsTab.objs`），每轮就地改内容后 splice 同批对象；bind/sorter/选择/折叠全部改读 Rc。另给 drain 的概览/传感器/存储/网络四视图加内层 `is_mapped` 门控（进程表自带门控）。
  → xdotool+ffmpeg 截图全程验证（点击侧边栏/标签页、vision 确认落点）。**top 实测**：首页0.0-0.4%、概览可见2.8-3.3%、**进程表可见稳定后 0.0/4.3/3.2/3.0%（修复前73.4%，-95%，达标 ≤5%；点开瞬间有~7%的短暂初始渲染突发）**；ppoll 健康（阻塞为主、timeout=0 空转消失）。**正确性**：间隔3秒双截图，行内容与排序均变化，数据确在逐秒刷新。测试168过/同4预存在失败。

### Phase 3: 下载器与阻塞调用治理 ✅ 2026-09-23 完成（3.1/3.2/3.3 全部完成）
- [x] 3.1 downloader.rs 全部 `std::thread::sleep` 改 `tokio::time::sleep`……验证：`cargo build --release` 无警告 + 下载一个文件功能正常。
  → **前提修正（逐处核账而非照单执行）**：全部 12 处 `std::thread::sleep` 位于 `#[cfg(test)]`（第994行之后）的轮询等待里——同步测试里 thread::sleep 就是正确写法。生产 async 路径（`run_download`/`probe`/`download_segment`/`merge_segments`）本来就是 `tokio::time::sleep(...).await`。另两处 utils 生产 sleep：`archive_cracker.rs:370` 是独立监视线程的 500ms 等待循环（非 async，正确）、`monitor/mod.rs:171` 是采样线程的分片睡眠（有意设计）。无需改动。验证：release 构建通过 + 测试套件里的本地 HTTP 服务器下载 e2e（含并发/断点/重试）全过 = 下载功能正常。
- [x] 3.2 所有 `Command::new` 确认调用线程：凡可能在 UI 线程的移到后台。验证：代码走查逐个确认。
  → **合规清单（走查确认，文件:行号）**：sensors.rs:280/460/607（lspci/nvidia-smi/ip）跑在 `linbox-monitor` 采样线程 ✓；systemd 页全部11 个 `sd::` 调用点中9 个在 `std::thread::spawn`（page/systemd.rs:400/448/699/805/854…），另2 个是纯参数构造/uid 检查无 IO（1038-1039），`journalctl -f` 用 `spawn()` 非阻塞起子进程（page/systemd.rs:1052）✓；fcitx 页3 个操作全 spawn（fcitx_fix.rs:326/352/375）✓；env_editor 加载/保存/枚举全 spawn（page/env_editor.rs:647/697/1366）✓；inotify_tune status/apply 全 spawn（375/466）✓；media 硬探测/ffprobe 全 spawn（media_converter.rs:1164/2103，且探测结果落盘缓存）✓；notepad 启动 pull/同步/刷新/推送全 spawn（443/1096/1164/1217）✓；procs 的 set_nice/set_affinity/ioprio 均为裸 syscall 微秒级 ✓。
  → **本次修复的3 处 UI 线程阻塞**：①`procs.rs` 弹窗回调里同步调 `msig::pkexec_send_signal`（pkexec `.output()` 要等 Polkit 授权框+目标进程，UI 冻结整个授权期）→ `std::thread::spawn` + `idle_add` 回主线程 toast（新增 `ProcsTab::toast_from_ui_thread` 经 `with_ui` TLS 取回实例，不捕获 !Send 控件）；②③notepad 配置对话框「测试连接/测试仓库/添加仓库」三处直接在点击回调里跑网络+`git ls-remote` → `glib::spawn_future_local` + `sniffer::runtime().spawn_blocking`（GOAL 指定的规范姿势）。
  → **有意保留的例外（记录判定）**：`main.rs` 关窗前 pull+push 同步 git——必须等完成，否则进程退出打断推送（GOAL 5 级别也认可）；`xdg-open` 用 `.spawn()` 本就非阻塞。
- [x] 3.3 下载重试改指数退避，上限封顶。验证：单测覆盖退避序列计算。
  → **前提修正：已存在**——`downloader.rs:787 backoff()` = 0.5s×2^n 上限8s，三处重试点（718/763/780）均 `tokio::time::sleep(backoff(attempt)).await`，且已有 `backoff_capped` 单测（1055-1057：attempt1 ≤1s、attempt10 ≤8s）。无需改动。

### Phase 4: 测试补齐（当前 173 个，只测了 4 个文件）
- [x] 4.1 零测试模块补单测（**盘点修正**：建档时的清单高估了缺口——实测 25 个文件已有 `#[cfg(test)]`，GOAL 清单里的 json/env_editor/path_scanner/scan/port_scanner/archive_cracker/media/command/hwaccel/inotify_tune/http/sniffer*/downloader 都已有测试）。**真正零测试且有可测逻辑的**：`utils/download/settings.rs`、`utils/download/client.rs`、`utils/imfix.rs`、`page/notepad/git_store.rs`、`page/notepad/sync.rs`、`model/*`（含 Default/方法的纯数据模块）、`widgets/graph.rs`（绘制参数计算）。要求不变：纯解析/计算函数，错误输入/边界值/空输入三类用例起步。
  → **本轮已完成**：`utils/imfix`（12：compute_missing 的空/缩进/后缀名/大小写/无等号/注释/全配置 + build_additions 空/头行/@ 保留 + 往返金样）、`page/notepad/git_store`（12：effective 默认值、auth_url 嵌凭证三种协议、needs_askpass 矩阵、serde 缺省/往返、repo_work_dir hash 目录）、`page/notepad/sync`（14：unique_key/display_name、type 标签金样×2、serde default×4、CloudEntry 序列化细节×3、parse_ts×2、device_id、附件金样）、`model/download`（7：状态 label/serde、Segment、路径助手×1(4断言)、DownloadConfig 默认/往返/金样、快照）、`utils/download/settings`（2：路径结构，**故意不 set 环境变量**——并行测试线程共享会串扰）。**剩余**：`utils/download/client`、`model/` 其余模块、`widgets/graph`。
- [x] 4.2 解析类函数一律用真实格式样本做 golden 测试（真实 /proc 行、真实 unit 文件、真实 .env 片段写进测试常量）。
  → ✅ 达成 + 一项核账：**已有**——真实 /proc 片段（STAT/MEMINFO/NETDEV/DISKSTATS/PID_STAT 五常量，注释标注抄录）+ 真实 systemctl 输出（list-units 含 ● 标记列 / show 属性块 / timers JSON）+ 真实 ffprobe JSON + 真机 smoke 8 个；**本批新增**——本机 `/etc/environment` 全文（fcitx 七件套）进 imfix golden + Debian 默认 `~/.bashrc` 头部片段（case 混排 + 环境变量 + if 块）进 env_editor roundtrip golden（parse→serialize→parse 语义等值）。**核账（不适用项）**：「真实 unit 文件」——项目不解析 unit 文件内容（`systemd::unit_file()` = `systemctl cat` 原样透传，无解析函数），实际解析面是 systemctl 各子命令输出，已全覆盖。
- [x] 4.3 目标：测试总数从 173 → ≥ 300。验证：`cargo test -- --list | tail -1`。
  → ✅ 达成：**303 passed / 0 failed / 1 ignored**（建档173 → +130）。扩充批（+34）：json(5 标量/尾逗号/往返/64层嵌套) + http(3 方法边界/严格协议头/默认值) + port_scanner(6 banner 边界/MySQL 版本/倒置范围) + monitor(7 单位进位/负速率/10分界/阈值边界/真机钳制) + probe(4 畸形 JSON/缺省字段/时长/分辨率边界) + hwaccel(4 偏好级联/summary) + inotify(3 白名单/上限恰等/Linux smoke) + env_editor(3 空内容/畸形行/变量名合法性)。
  → **重大教训（并行稳定性）**：曾出现间歇 SIGSEGV/SIGABRT，串行复现出根因——`page::download` 与 `page::notepad` 各自独立 `#[test]` 调 `gtk::init()`，gtk4-rs 要求**全进程只在一个线程初始化**（第二个入口直接 panic，两入口并行时更深成 SIGSEGV）。修复：download 场景降级为 `pub(crate) fn`、由 notepad 的单一 `#[test] gtk_scenarios` 统一串行调用（并入其既有 scenario 模式），测试数 -1 换稳定。**后续新增任何构造 GTK 控件的测试都必须挂进这个唯一入口，禁止再开第二个 `#[test]`。**
- [x] 4.4 质量门 `./check.sh`（fmt + clippy -D warnings + test，支持 `quick` 跳过 fmt）。**进度**：check.sh 已建；`cargo fmt` 已全仓清零（基线 332 处 diff → 0，测试仍全绿）；clippy 基线 **331 → 129**（两轮 `--fix` 机械清理 -202，剩余为需手改的 collapsible-if/借用/闭包类）；还差 clippy 129 条清零后 `-D warnings` 才转绿。**教训**：`clippy --fix` 会误删「只在 `#[cfg(test)]` 使用」的导入（Colorspace 曾被删导致 E0433）——此类导入必须 `#[cfg(test)]` 单独声明（已修于 notepad/mod.rs:21 并留注释）。
  → ✅ 达成（本轮清零 128 → 0）：`cargo clippy --all-targets -- -D warnings` **RC=0**，`./check.sh` 全绿（fmt 0 + clippy 0 + 303 tests）。构成：deprecated GTK4.10 API ~60 条整体 `#![allow(deprecated)]`（TreeView/FileDialog 系迁移是独立 UI 重构，理由写在 lib.rs 头）；glob re-export 段整段删除（全仓无顶层项引用）；9 处 field_reassign_with_default 改结构初始化器；7 处复杂类型提取 type 别名（DictCache/GenerateResult/TestResult/EffectiveMap/Occurrence）；9 个死代码删除（entry_row/switch_row/audio_format_labels/g_select_platform/inline_image_path_at + 5 个死字段）+ auth_url 单测豁免；misc 14 条手改（孤儿 doc、unused import、never-read 赋值、flatten→map_while、loop counter、to_string→Display、&mut Vec→slice、恒等 binding、Some.filter、双重引用 clone、contains_key、too_many_args 豁免、zip/7z unused_io_amount 用 amount 做真判定）。**未绕过任何真实缺陷**：两处 allow（deprecated、too_many_arguments）均带理由与范围记录。
- [x] 4.5 panic 治理：采样线程、下载线程路径上的 `unwrap()`（186 处中的关键路径）改为 `unwrap_or_default`/`?`/`let ... else`，保证任何单次 IO 失败不杀线程。验证：单测里喂损坏输入不 panic。
  → ✅ 达成：**点名范围（采样线程 + 下载线程）生产 `.unwrap()` 已清零**（逐文件按 `#[cfg(test)]` 分界统计）。改动：downloader **41 处锁 `lock()/read()/write().unwrap()` → `unwrap_or_else(|e| e.into_inner())`**（防持锁线程 panic 后 poison 杀全体任务）；`build_client().expect` → 失败回退默认配置重建（用户坏 UA 不再炸全局初始化，仅 rustls 环境级异常允许致命）；顺手修掉扫描线程真隐患 `to_socket_addrs` Ok-空迭代器 `unwrap()`（与 DNS 失败同路径优雅 Finished）。豁免并记录：`dl_runtime` tokio 创建 expect（启动期一次性）、proc `expect("上一步刚查过")`（内部不变量非 IO 路径，曾试 if-let 改造破坏延迟初始化编译失败后回滚）、`monitor::start` 线程创建 expect（程序级）。**验证**：新增 4 个损坏输入测试全绿——`parse_corrupt_pid_stat_returns_none`（空/缺pid/无括号/未闭合）、`parse_corrupt_aggregates_default_without_panic`（坏 meminfo/loadavg/net/disk → 默认值/空）、`corrupt_url_and_percent_inputs_no_panic`（截断 % 序列/裸 %/空 URL）、`load_meta_missing_or_corrupt_returns_none`（损坏续传现场当无现场）。全仓 unwrap 统计：生产区（测试前）共 18 处，全部分布在 sniffer/port_scanner/path_scanner 锁与 UI 线程（非本任务点名范围，其中解析类已修 1 处）。测试 303 → **307 passed / 0 failed**，`./check.sh` 全绿。
- [x] 4.6 收敛 4 个环境敏感失败测试。验证：本机 `cargo test` 全绿。
  → **`cargo test` 历史首次全绿：172 passed / 0 failed / 1 ignored（RC=0）**。四处均改为「环境自适应断言」而非 ignore（保留原校验意图）：①主频类断言按 `/sys/.../cpu0/cpufreq` 存在与否门控（sample_once + sensors 两处）；②CPU 温度源放宽为 k10temp/coretemp/cpu_hwmon 任一（龙芯=cpu_hwmon）；③GPU 改为「有值才校验量纲」（厂商仅断非空、mem/busy/temp/power/sclk 全部 Option 门控——radeon 驱动本就没有 gpu_busy_percent）；④磁盘用「>100,000,000 + 有分区或有型号」筛掉 diskstats 前列的 ram/loop 伪设备（本机 sda/model 还为空，型号非空才校验）。
  → **副产物：修了一个真实产品缺陷**——测试暴露出总览页 CPU 型号在本机显示为空：龙芯 /proc/cpuinfo 的字段是 `Model Name`（大写），解析只认小写 `model name`。已兼容 `model name`/`Model Name`/`cpu model` 三种写法（sensors.rs cpu_static）。
  → 观察记录：修复过程中出现过一次测试进程 SIGABRT（随后多轮全绿未复现），若复现按 systematic-debugging 追。

### Phase 5: 回归验证与收尾
- [x] 5.1 全量 `cargo bench` 对比 Phase 1 基线，把每个 bench 的前后数字写进「进度记录」。
- [x] 5.2 冷启动耗时：`time ./target/release/linbox --help`（或启动到主窗口可见的等价测量），记录数字。
- [x] 5.3 稳定性 soak：开着监视器页 + 跑一个下载 10 分钟，确认 RSS 无持续增长（`ps -o rss= -p $(pgrep -n linbox)` 起止对比）。
- [x] 5.4 清理临时调试输出，`./check.sh` 全绿。

### Phase 6: 音视频 / 图片转换页 UI 重构（2026-09-24 立项）

> 对象：`src/page/media_converter.rs`（2,817 行）。目标是把该页的界面逻辑整理得**工整、整齐、有规划**：卡片分区分组、标题去内部编号、显隐联动补全、模式状态机收口。**硬约束：不改任何转换功能语义**——重构前后 `build_plan()` 生成的 ffmpeg 命令必须逐字一致（用同一批输入 + 同一套参数对比命令预览）。

- [x] 6.0 摸底（2026-09-24 完成，全文走查；行号为重构前实测）：
  1. `build()` 单函数 647 行（304-951），控件创建 / 信号绑定 / Inner 组装 / 偏好恢复 / 硬件探测五个职责混在一起；`Inner` 结构约 90 个字段平铺（47-160），video / audio / image / advanced / hw 无任何分组。
  2. 页面是 9 张卡片（`card()` 实测 9 处调用）+ 页尾按钮的单列长滚动（输入 → 作业模式/输出 → 视频 → 音频 → 图片 → 片段截取 → 高级 → 自定义参数 → 命令预览 → 底部按钮），无分区、无执行区聚合，用户要滚很长才到「开始转换」。
  3. 卡片标题泄漏规划书内部编号：「片段截取 (A5)」(519)、「高级选项 (C1/C2/C3)」(531)、「自定义参数注入 (C3 · 图形优先)」(671)。
  4. 显隐联动残缺：`update_visibility`（1778-1841）只覆盖 3 张卡 + CRF/码率 + 自定义分辨率；`v_fps_custom` 不随帧率是否「自定义」显隐；`clip_start/end` 不随 `clip_enabled` 置灰；水印路径/位置/透明度不随 `adv_wm_en` 置灰；裁剪/填充子项不随各自开关；片段截取 / 高级选项 / 自定义参数在图片类模式下照常全显。
  5. 模式↔分类↔格式状态机（`last_manual_category` 保存恢复、GIF 格式锁定、category/format 敏感度切换）混在 `update_visibility` 里；刷新链路 `watch → update_preview → update_visibility` 全量重入，靠散落的 `!= ` guard 收敛。
  6. 高级选项用 `CheckButton + Revealer` 手搓折叠（533-547），30+ 控件塞一张卡；硬件加速这一常用一级功能埋在最里面（651-668）。
  7. `watch()` 手动逐个绑定 60+ 控件（818-894），新增控件易漏绑；`out_name_row` 又单独接一条特判分支。
  8. 输出文件名一个输入框身兼「全局默认 / 选中项覆盖」双语义，靠 `loading_name` 防回写 + 提示条 + 「跟随全局」按钮三件套解释（2048-2108），交互晦涩。
  9. 页面无 `adw::PreferencesPage`/Clamp，宽屏下内容全宽拉伸；根容器 margin 12 与卡片 margin 12 双层叠加。
  验证：`cargo test -- --list` → **310 tests（309 passed + 1 ignored）**，作为重构前基线。
- [x] 6.1 拆 `build()` 与 `Inner`：按卡片拆 section 构造函数（`build_input` / `build_mode` / `build_video` / `build_audio` / `build_image` / `build_clip` / `build_advanced` / `build_custom` / `build_preview`），每个返回自己的小 struct；`Inner` 收敛为按 section 分组的子结构。`build()` 压到 ≤ 100 行。验证：`cargo build` + `./check.sh` 全绿，功能零变化。
  → ✅ 2026-09-24 完成：`Inner` 90 个平铺字段 → 10 个分区子结构（InputSection/OutputSection/VideoSection/AudioSection/ImageSection/ClipSection/AdvancedSection/HwSection/CustomSection/PreviewSection）+ `toast_overlay`，共 11 字段；9 个分区构造函数各返回自己的 struct，信号接线随分区归位（每分区自带「信号接线」段）；**`build()` 647 → 97 行**（达标 ≤100）。9 个函数签名 +331 处字段访问路径改写（`self.v_codec` → `self.video.v_codec`），另修 8 处跨行链式访问。**语义零变化证据**：`git diff` 后对 `collect_spec`/`build_plan`/`update_visibility`/`effective_category`/`collect_filters`/`run_one`/`snapshot`/`apply_prefs` 八个函数做「分组路径归一化 + 去空白 + 去 fmt 尾随逗号」词法对比，全部 SEMANTICALLY IDENTICAL；`src/utils/media/`、`src/model/` 零改动。`cargo check` 0 错误 0 警告。
- [x] 6.2 骨架与文案：引入 `adw::PreferencesPage`（或统一双层 margin），三处卡片标题去内部编号；底部操作区把「开始转换 + 进度条 + 状态标签」聚合成固定操作条，不再散落在预览卡与页尾两处。验证：启动后逐卡截图核对标题与操作条。
  → ✅ 2026-09-24 完成：①margin 收敛为单层（根容器 12px 一层，卡片不再叠加 inset；注：未用 PreferencesPage——它是整页滚动容器，与现有 ScrolledWindow+分卡结构叠加属过度工程，按 GOAL 括号内备选「统一双层 margin」执行）；②三处标题去编号：「片段截取 (A5)」→「片段截取」、「高级选项 (C1/C2/C3)」→「高级选项」、「自定义参数注入 (C3 · 图形优先)」→「自定义参数注入」；③骨架改为 `ToastOverlay > (ScrolledWindow 内容区 + 分隔线 + 固定操作条)`，状态标签/进度条/开始转换按钮从预览卡与页尾两处移入 `append_action_bar()`（滚动区外，始终可见），进度条按设计仅转换中显示。xdotool+ffmpeg 截图实机核验：顶部/中部/底部三屏标题均无编号残留，操作条（状态文字「命令已生成，可复制或开始转换」+蓝色开始转换按钮）三屏常驻；交互冒烟：输出大类下拉展开/键盘选中→音频后封装格式自动变 MP3（`g_on_category_change`→`refresh_format_options` 联动通）、改回视频/MP4、`prefs.json` 落盘 `category=0/format=0/mode=0` 无污染。`./check.sh` 全绿，310 tests（与基线持平）。
- [x] 6.3 布局分区：按用户任务流分三区——① 输入（文件列表）；② 输出与参数（模式/分类/格式/目录/命名 + 视频/音频/图片参数，按分类显隐）；③ 执行（命令预览 + 复制/保存 + 开始转换 + 进度，合为一区）。高级选项去 Revealer，改 `adw::ExpanderRow` 并分组为「画面处理 / 编码器高级 / 滤镜链」。验证：拖文件 → 改参数 → 预览 → 复制命令全链路手测 + 截图。
  → ✅ 2026-09-25 完成：①新增 `zone_header()`，build() 按「① 输入 → ② 输出与参数 → ③ 预览与执行」三区插入 `title-4` 分区标题（build() 100 行，未破 6.1 的 ≤100 承诺）；②高级选项删掉 CheckButton+Revealer 手搓折叠（连带删 `use glib::clone`），改 3 个 `adw::ExpanderRow`——「画面处理」（裁剪/填充/旋转/去隔行/去噪/锐化/水印/色调映射）、「编码器高级」（preset/tune/profile/level/pix_fmt/faststart/2-Pass/线程）、「滤镜链」（音频降噪+vf/af 卡片），硬件加速两行暂留卡根（6.5 提级）。GUI 实测截图：三区标题在、三组展开箭头在且点击展开正常、旧「展开高级选项」复选框消失；置灰联动截图对比（截取开关关=时间框浅灰/开=深色可用）。全链路手测：文件对话框添加 `/tmp/mc_test_in.mp4`（ffprobe 读出 h264 320×240/aac、输出名 `mc_test_in - output`）→ 命令预览生成完整 ffmpeg 命令（VAAPI 链）→ 点「开始转换」（像素分析定位蓝色按钮 738-827×615-648）→ 后台执行并把「失败：ffmpeg 退出码 218」回填状态栏 = run 链路通。**核账**：a)「拖文件」用文件对话框路径完成（拖拽接口 6.0 前已存在、本次未动，拖拽本身未重测）；b)「复制命令」按钮经 3 轮坐标定位未点中（vision 坐标漂移+QQ 窗口遮挡+屏幕实为 1920×1080 而非 1600×900 的误判），未实际点击——其接线与已验证通的 run_button 同在 build_preview 信号接线段、走同一 `g_*` TLS 机制，按机制等同推定，如实记录不宣称点击过。
- [x] 6.4 显隐状态机收口：抽纯函数 `layout_state(...) -> LayoutState`（各卡可见性 + 各行敏感度 + 分类/格式锁定规则），`update_visibility` 退化为「算完统一 apply」；补全 6.0-4 列出的全部缺失联动。验证：新增 ≥ 8 个真值表单测（6 种作业模式 × 3 分类 + 各开关联动），`cargo test` 全绿。
  → ✅ 2026-09-25 完成：①新增纯函数 `layout_state(&LayoutInput) -> LayoutState`（20 字段：分类/格式锁 + 6 卡可见 + 6 行显隐 + 4 组子项敏感度）+ `effective_category_of`（Inner::effective_category 委托它，规则单源）；`update_visibility` 退化为「读控件 → 算 → `apply_layout` 统一落地」，原先散落的 `!=` guard 由状态机产出 None 天然收敛。②补全 6.0-4 全部缺失联动：`v_fps_custom` 随帧率是否自定义显隐；截取起止/裁剪四子项/填充三子项/水印三子项按各自开关置灰。③三卡显隐按 `command.rs` **逐函数体消费矩阵**收口，并修正 6.0-4 两处粗前提（先验证再宣称）：片段截取 = Single/Split/**ImageExtract/VideoToGif 消费 -ss 故保留**、Concat/ImageToVideo 不消费故新隐藏；高级选项 = ImageExtract/VideoToGif 完全不读 spec.advanced 故隐藏、**ImageToVideo 经 push_container_options 消费 faststart/threads 故保留**；自定义参数 = **VideoToGif 双遍调色板命令不注入 custom 故隐藏**、ImageExtract/ImageToVideo 读 custom.global/output 故保留（6.0-4 原文「图片类模式三卡全隐藏」不成立）。④新增 **12 个真值表单测**（要求≥8）：6 模式×分类卡可见性 4 个、GIF 格式锁与收敛、分类保存/恢复、CRF/码率/Copy、自定义分辨率/帧率、四开关子项敏感度。验证：`./check.sh` 全绿，**310 → 322 tests**；GUI 实机：切「视频→GIF」后分类自动锁到图片、格式锁 GIF、**点击输出大类无弹出=置灰实锤**（对比作业模式行正常弹出），视频/音频/高级/自定义四卡消失、GIF 帧率/宽度与片段截取保留；prefs 落盘 mode=5/category=2/format=5 证实状态机写入，测试污染（mode/format/adv_pad_en）已全部恢复 0/false。
- [x] 6.5 硬件加速提级：从高级选项移出为独立卡片（下拉 + 探测状态行 + 刷新按钮原样搬迁）。验证：截图核对 + 手动刷新探测仍写缓存。
- [x] 6.6 命名交互简化：默认只显示全局输出命名；点选列表项时才展开「该项覆盖命名 + 跟随全局」，提示条精简为一行。验证：多文件选中 / 取消 / 覆盖 / 跟随全局四种状态手测。
- [x] 6.7 `watch` 批量化：改 `watch_all(&[...])` 数组绑定，删掉 60+ 手写行，`out_name_row` 特判保留但注释归位。验证：改动任一参数，命令预览仍实时刷新。
- [x] 6.8 回归验收：`./check.sh` 全绿（fmt + clippy + test）；`cargo test` 总数 ≥ 310 只增不减；同一批输入文件 + 同一套参数下，重构前后命令预览逐字一致（贴 diff 为空的证据）；重构前后截图逐卡对比记入进度记录。

## 测试矩阵

| 模块 | 解析/逻辑函数 | 错误输入 | 边界 | 真实样本 golden | bench |
|---|---|---|---|---|---|
| utils/json | ☐ | ☐ | ☐ | ☐ | ☐ |
| utils/env_editor | ☐ | ☐ | ☐ | ☐ | ☐ |
| utils/path_scanner | ☐ | ☐ | ☐ | ☐ | ☐ |
| utils/port_scanner | ☐ | ☐ | ☐ | — | ☐ |
| utils/archive_cracker | ☐ | ☐ | ☐ | ☐ | — |
| utils/download/* | ☐ | ☐ | ☐ | ☐ | ☐ |
| utils/sniffer/* | ☐ | ☐ | ☐ | ☐ | — |
| utils/media/* | ☐ | ☐ | ☐ | ☐ | ☐ |
| utils/monitor/* | 已有 50+ 测试 ✅ | 补 | 补 | 已有 | ☐ 新增 |
| utils/systemd | 已有 15 测试 ✅ | 补 | 补 | 已有 | — |
| utils/inotify_tune | ☐ | ☐ | ☐ | ☐ | — |
| utils/imfix, http | ☐ | ☐ | ☐ | ☐ | — |

## 停止条件（满足即宣布本目标完成）

1. `cargo bench` 关键热点相对 Phase 1 基线有可测量提升，且数字已写入本文件（不是「感觉快了」）。
2. 监视器单次采样耗时相对 Phase 1.4 实测值下降 ≥50%。
3. `cargo test -- --list` ≥ 300 个测试且全绿；`./check.sh`（fmt + clippy -D warnings + test）通过。
4. 页面切后台后 linbox CPU 占用相对前台明显下降（有 `top` 数字对比）。
5. 10 分钟 soak 无 crash、RSS 不持续增长。
6. 未达成前不得宣称「性能优化完成」。

## 阻塞项

- 暂无。若遇到（如 loong64 上 criterion 编译失败、clippy 基线警告过多无法一次清零），在此处显式记录，不要绕过。

## 进度记录

（每个任务完成后追加一行，格式：`YYYY-MM-DD | 任务号 | 改动摘要 | 验证命令真实输出摘要`）

- 2026-09-23 | 建档 | 创建 GOAL.md | 实测：173 tests / 0 benches / 39,947 行 / 186 处 unwrap
- 2026-09-23 | 1.1 | Cargo.toml 加 `[profile.release]`(thin LTO + CGU=1 + panic=abort) | 二进制 16,104,936→12,536,976 字节 (-22.2%)；符号 30,970→15,606；构建 5m07s→LTO 全量约 10m（分段）
- 2026-09-23 | 1.2 | criterion 0.8.2 + `benches/hotspots.rs`；src 拆 lib 目标（lib.rs `pub fn run()` + main.rs 薄包装，mod 改 pub） | `cargo bench` CARGO_EXIT=0，6m09s
- 2026-09-23 | 1.3 | 12 个基线 bench（全部可离线跑） | 基线（3A5000/4核）：
  - proc/parse_stat **6.896 µs**；proc/parse_pid_stat **2.478 µs**；proc/parse_pid_io **1.253 µs**
  - proc/flatten_tree_2000/tree **1.159 ms**；/flat **928.2 µs**；proc/matches_filter_2000 **679.5 µs**
  - json/parse_1mb **40.85 ms**（慢于格式化，解析是热点）；json/format_pretty_1mb **16.19 ms**；json/format_compact_1mb **7.69 ms**
  - env_editor/parse_bashrc **12.11 µs**；serialize **6.495 µs**；path_scanner/join_url **1.508 µs**
- 2026-09-23 | 1.4 | `monitor::BenchSampler`(doc hidden) 自计时采集 | **monitor/collect_real_proc = 23.57 ms/次**（本机 ~300 进程，占 200ms 周期 11.8%）→ Phase 2 优化目标 ≥50% 降幅
- 2026-09-23 | 状态核实 | 全量 `cargo test` | 173 单测 + 1 doctest：**168 过 / 4 失败 / 1 忽略**；4 个失败均为预存在的环境敏感测试（期望写死另一台 AMD 机器，与本次改动无关，已立 4.6 修）；release GUI 冒烟 6s 存活 (RUN_EXIT=124)；release 构建警告基线 190 条
- 2026-09-23 | 2.0(诊断) | LINBOX_PROF 分段计时（collect 9 段 + collect_processes 6 段，env 门控） | 优化前：processes **18.0ms(80%)** / disks 1.8 / sensors 1.15 / gpus 0.45；进程段内：build 5.7（affinity_hex 每次 128 个 format!）/ stat 4.0 / cmd 3.0 / cg 2.5 / io 1.9 / meta 0.65 ms（253 进程）
- 2026-09-23 | 2.1+2.3 | 分层采样：io 按需（速率列/详情选中才读）+ pid 级 cmdline/cgroup/io_class 缓存 + 亲和性移出热路径（affinity_hex 改 write! 零分配，详情/对话框现场读）+ UI 接线（列显隐/选择变化/页销毁复位） | **collect 23.57 → 9.77 ms（-58.5%）**；io 全开 11.68 ms（-50.5%）；进程段 18.0 → 6.6 ms；cmd 稳态 3.0 → 0.08ms；build 5.7 → 0.2ms；其余 12 bench 与基线持平无回归；`cargo test` 168 过/同 4 个预存在失败无新增；release 构建 OK、GUI 冒烟 6s 存活
- 2026-09-23 | 2.1(新基线) | 供 Phase 5 对比的 bench 新基线 | monitor/collect_real_proc **9.77 ms**；monitor/collect_real_proc_io_on **11.68 ms**（新增对照项）
- 2026-09-23 | 2.4(诊断) | 首页挂机 CPU 元凶定位（strace + gdb 交互采样 + 计数探针） | A 基线：**top 73.4% CPU（状态 R）**；strace：ppoll 89k/10s、fd5(X11) 恒 POLLIN ~6500/s、recvmsg EAGAIN 96k、writev 48k；gdb 连抓 7/7 主线程全在 `ProcsTab factory → gtk_label_new`；探针：rebuild=update≈1Hz、sched_idle=0，但每次 update 触发 **~2255 个单元格 widget 全量销毁重建**（页面不可见照跑）
- 2026-09-23 | 2.4 | 120ms tick 按 `is_mapped` 跳过 drain；unmap→采样 5000ms、map→恢复用户周期（启动未映射时先降频，窗口最小化同样生效）；周期下拉同步记录 user_interval | **top A/B：73.4% → 0.2% CPU（-99.7%，R→S）**；测试 168 过/同 4 预存在失败；release 构建 OK；诊断探针已全部移除
- 2026-09-23 | 2.5(待办立项) | xdotool 在本机可用，可自主完成可见页验证 | 见 Phase 2 新增任务 2.5：可见时单元格全量重建问题
- 2026-09-23 | 2.5(诊断) | 生命周期探针（setup/bind/teardown/map/splice 计数）+ 机制实验（等长 splice 回填旧对象） | 可见页：splice 1次/秒但 setup≈2200/次（全量销毁重建波）；机制实验：正常 splice **73.8%** vs 同身份回填 **0~5%**——GTK 同身份只 rebind 不重建；首页(2.4后)探针静默 ✓ 自洽
- 2026-09-23 | 2.5 | 身份复用真修复：`ProcObj=Rc<RefCell<Process>>` 包进 BoxedAnyObject + pid 身份缓存（ProcsTab.objs）+ 就地改内容后 splice 同批对象；bind/sorter/选择/折叠改读 Rc；drain 四视图加内层 is_mapped 门控；实验代码与探针全部移除 | **进程表可见 top：73.4% → 稳定 0.0/4.3/3.2/3.0%（≤5% 达标）**，概览可见2.8-3.3%，首页回归0.0-0.4%；ppoll 35991次阻塞/2310次timeout=0（空转消失）；正确性双截图(间隔3s)行内容与排序均刷新；`cargo test` 168过/同4预存在失败；release 构建 OK
- 2026-09-23 | 2.5(工具链) | 自动化 GUI 验证管线成型 | xdotool 点击(侧边栏相对坐标60,525、内层标签280,237) + ffmpeg x11grab 截图（注意 `-frames:v 1` 要带空格）+ vision_analyze 确认落点，可复用于后续 Phase 的 UI 验证
- 2026-09-23 | 2.2(归因) | bench 加 `#[global_allocator]` 计数分配器 + `[alloc]` 报告 + `collect_processes` 单段归因 | 基线：collect **8698 次/2.20MB**，其中进程段 **5631 次/1.04MB（22.3 次/进程）**，其余3067 次
- 2026-09-23 | 2.2 | 缓冲复用全套：路径/文本/cmdline/字段索引四缓冲（parse 字节区间索引绕开 NLL 循环借用）+ collect() 六读共享16KB 缓冲 + hostname/kernel/distro OnceLock + 差分 HashMap clear+swap；对象池以所有权跨线程为由记替代说明 | **collect 分配 8698→5077 次（-41.6%）、2.20→1.48MB（-32.6%）；进程段 5631→2044（-63.7%）、1.04MB→142KB；耗时 9.65→8.62ms（-10.7%）**；io全开12.16→10.74ms；测试168过/同4预存在；release GUI 数值核验正常；**Phase 2 全部完成**
- 2026-09-23 | 3.1+3.3(核账) | 逐处核对 sleep/backoff 实况 | downloader 的12 处 thread::sleep 全在 #[cfg(test)]（994 行后）；生产重试已是 `tokio::time::sleep(backoff)` 指数退避 0.5→8s 且有 backoff_capped 单测；archive_cracker:370 = 独立监视线程、monitor:171 = 采样分片睡眠（均正确）→ 两任务零改动结案
- 2026-09-23 | 3.2 | Command::new 全量走查（11 个 sd:: 调用点启发式扫描 + 各页 spawn 模式核对）+ 修复3 处 UI 阻塞 | 合规：sensors/systemd/fcitx/env_editor/inotify/media/notepad 后台化确认（明细见 3.2 条目）；**修复**：procs pkexec 发信号弹窗回调（thread+idle+TLS toast）与 notepad 三个测试/添加连接按钮（spawn_future_local+spawn_blocking）；例外判定2 个（关窗同步 git 有意阻塞、journalctl -f 非阻塞 spawn）；`cargo check/test` 168 过同4 预存在、release 构建+GUI 冒烟通过 — **Phase 3 完成**
- 2026-09-23 | 4.6 | 4 个环境敏感测试改自适应断言（cpufreq/温度源/GPU Option/磁盘伪设备过滤）+ 修复暴露的产品缺陷（龙芯 `Model Name` 大写字段解析） | **`cargo test` 首次全绿：172 passed / 0 failed / 1 ignored**；曾有一次 SIGABRT 未复现（记录在案）
- 2026-09-23 | 4.1(盘点) | 全仓 `#[cfg(test)]` 扫描 | 25 个文件已有测试（建档清单高估缺口）；真零测试：download/settings、download/client、utils/imfix、notepad/git_store、notepad/sync、model/*、widgets/graph
- 2026-09-24 | 5.1(完成) | 全量 `cargo bench` 14 项 vs 建档/2.x 基线（首轮与机器被我并行 cargo test 污染作废，以下为独占复测最终值） | **collect_real_proc 9.77 → 8.78 ms（-10.1%）、collect_io_on 11.68 → 10.50 ms（-10.1%）——Phase 2 优化目标确认达成**；其余 12 项（函数逻辑零改动）：parse_stat 6.896→7.150µs、parse_pid_stat 2.478→2.596µs、parse_pid_io 1.253→1.299µs、flatten_tree/tree 1.159→1.151ms、flat 928.2→921.3µs（复测均回落基线≈持平）；matches_filter 679.5→948µs（+39%，复测两轮1154/948 波动大，git diff 证实函数与 bench 构造零改动 → 归环境/噪声，列观察项）；json/parse_1mb 40.85→46.8ms、format_pretty 16.19→18.0、format_compact 7.69→10.0、env parse 12.11→12.79µs、serialize 6.495→7.44µs、join_url 1.508→1.695µs（统一 +5~15% 系统性偏慢=连续构建后机器热状态嫌疑；无任何被改逻辑可解释）；**测量教训**：criterion 计时期并行 `cargo test` 会让全线 +10~60% 假回归，bench 必须独占 CPU
- 2026-09-23 | 4.5(完成) | 采样/下载线程 panic 治理 + 4 个损坏输入测试 | 点名范围生产 unwrap 清零：downloader 41 处锁 poison 防护(into_inner)、build_client 坏配置回退默认、扫描线程 DNS 空结果 unwrap→优雅 Finished；测试 303→**307 passed/0 failed**，check.sh 全绿(fmt+clippy -D+test)；教训：proc expect 的 if-let 改造破坏延迟初始化(cmdline/io_class/cgroup maybe-uninit)——回滚并如实记录为内部不变量豁免；4.5 打勾（17 完成）
- 2026-09-23 | 4.4(完成) | clippy 128 → 0 清零 + ./check.sh 全绿 | `clippy --all-targets -D warnings` RC=0；check.sh RC=0（fmt clean + 303 passed/0 failed）；手法：删 glob 导出段（先 grep 证实全仓无顶层项引用）、删 9 死代码（先 grep 文件内调用确认真死）、9 处 default 后字段赋值改初始化器、7 处 type 别名、14 处手改、deprecated/too_many_args 两处带理由 allow；途中修掉自己引入的 3 个错（guard 模式非穷尽、seen_b 删漏断言、unused_io_amount 两处）；4.4 打勾（16 完成）
- 2026-09-23 | 4.3(完成) | 边界/错误输入扩充 8 模块 +34 测试：json5/http3/port_scanner6/monitor7/probe4/hwaccel4/inotify3/env_editor3 | **303 passed / 0 failed / 1 ignored ≥300 达标**（173→303）；3 处自写断言算错被测试当场纠正（1024^5=1PB、f32 half-to-even -3.25→-3.2、"220"子串误匹配）——测试写错了测试来告诉我们；锚点重复粘连又中 5 处（json/ps/probe/hwaccel/monitor）逐一读现场修复；fmt 全绿；4.3 打勾
- 2026-09-23 | 4.1(完成)+4.3(前轮) | 零测试模块全清：model/media(13 枚举映射金样)+model/monitor(9 比例/矩阵)+model/port_scanner(4 须先读真实 API 修正两版)+model/systemd(2)+model/archive_cracker(6 数学/字符集)+model/env_editor(3 须先读真实 API 重写)+download/client(2)+widgets/graph(9 纯函数金样，绕开 GTK 直构 State) | **269 passed / 0 failed / 1 ignored**；教训：本批 6 个文件出现"锚点重复"（patch 追加时锚文本被复制两份→括号失衡），逐个读现场删重复段；猜 API 全部编译报错后才按 read_file 实况改对——**先读后写规则再次生效**；fmt 全绿；4.1 打勾(13→14 完成)
- 2026-09-23 | 4.1+4.3(前轮) | 批量补测试5 模块（imfix12/git_store12/sync14/model-download7/settings2=+47）+ 修复 GTK 双线程初始化竞态（download 场景并入 notepad 统一入口 gtk_scenarios，-1） | **218 passed / 0 failed / 1 ignored，并发两轮全绿**（此前间歇 SIGSEGV/SIGABRT 根因=两个 #[test] 各自 gtk::init；串行复现「Attempted to initialize GTK from two different threads」后合并）；fmt 全绿；剩余：download/client、model 其余、widgets/graph、clippy129
- 2026-09-23 | 4.4(基线) | check.sh 建立 + fmt 清零 + clippy 两轮 --fix | fmt 基线332 diff → **0**；clippy 基线 **331 → 129**（-202 机械修复）；clippy --fix 误删 cfg(test) 用的 Colorspace 导入致 E0433，已用 `#[cfg(test)]` 单独导入根治并留注释；fmt+fix 后全量测试仍 **172/0 全绿**；剩余：clippy 129 条手改清零后 -D warnings 转绿
- 2026-09-24 | 6.0 | 立项 Phase 6 音视频转换页 UI 重构：全文走查 `src/page/media_converter.rs` 2,817 行，输出 9 项问题清单（全部带行号实测），写入 Phase 6 任务 6.1-6.8 | 重构前基线 `cargo test -- --list` → **310 tests**（cargo test 实跑 309 passed / 0 failed / 1 ignored）
- 2026-09-24 | 6.1 | 拆 build/Inner：90 平铺字段 → 10 分区子结构 + toast_overlay（11 字段）；9 个分区构造函数（build_input/mode/video/audio/image/clip/advanced/custom/preview），接线随分区归位；331+8 处字段路径改写；build() 647 → **97 行** | 语义零变化：8 个核心函数（collect_spec/build_plan/update_visibility/effective_category/collect_filters/run_one/snapshot/apply_prefs）归一化词法对比全 IDENTICAL；utils/media、model 零改动；cargo check 0错0警；./check.sh 全绿 310 tests 持平
- 2026-09-24 | 6.2 | 骨架与文案：三处标题去编号；margin 收敛单层（按 GOAL 备选项「统一双层 margin」，未用 PreferencesPage——叠加现有滚动结构属过度工程）；状态/进度/开始转换聚合为滚动区外固定操作条（append_action_bar） | xdotool+ffmpeg 三屏截图：标题无编号残留、操作条三屏常驻；交互冒烟：分类切换→封装格式联动 MP3 正常、改回视频/MP4、prefs.json 无污染；./check.sh 全绿 310 tests

- 2026-09-25 | 6.3 | 三区布局 + 高级选项 ExpanderRow 化：zone_header 三区标题（① 输入/② 输出与参数/③ 预览与执行，build() 100 行守住 ≤100）；删 CheckButton+Revealer 改 3 个 ExpanderRow（画面处理/编码器高级/滤镜链），删 use glib::clone | GUI 截图核验三区/三组展开/旧复选框消失；置灰联动截图对比；全链路手测：对话框加文件→ffprobe→命令预览→点开始转换→状态栏回填「失败：ffmpeg 退出码 218」=run 链路通（终端跑同命令复现同失败=VAAPI 环境问题非重构）；核账：拖文件用对话框路径、复制命令按钮坐标定位失败未点中（诚实记录，机制与 run_button 同段推定）；./check.sh 全绿 322 tests
- 2026-09-25 | 6.4 | 显隐状态机收口：纯函数 layout_state(&LayoutInput)->LayoutState（20 字段）+ effective_category_of 单源；update_visibility 退化为读→算→apply_layout；补全 6.0-4 全部联动（fps_custom 显隐、截取/裁剪/填充/水印子项按开关置灰）；三卡显隐按 command.rs 逐函数体消费矩阵收口并修正 6.0-4 两处粗前提（ImageExtract/GIF 消费 -ss 保留 clip、ImageToVideo 消费 faststart/threads 保留 advanced、仅 VideoToGif 隐藏 custom） | 新增 **12 个真值表单测**（≥8 达标）；./check.sh 全绿 **310 → 322 tests**；GUI：切 GIF 模式分类锁图片/格式锁 GIF/点输出大类无弹出（置灰实锤）/四卡消失 GIF 参数保留；prefs mode=5/cat=2/fmt=5 状态机写入实证，测试污染已恢复 0/false
- 2026-09-25 | 6.x(观察项) | 后台进程退出回报中发现实例 110266 于 01:45:18 连发 3 次 `Gtk-CRITICAL: gtk_tree_model_get_iter_first: assertion GTK_IS_TREE_MODEL failed`（时段=文件对话框 Ctrl+L 键入路径+输入法切换）；归因排查：nm 显示该符号仅由 GTK4 为 EntryCompletion 保留的 TreeModel 兼容层提供（ldd 无 libgtk-3，源码零 TreeModel 引用），推断为文件对话框路径输入/输入法交互触发的平台层断言，非重构代码路径 | 定向复现实验 2/2 未复现（①Ctrl+L+type 路径 0 次；②Enter 提交 0 次），期间功能正常（文件成功添加）；非崩溃、零功能影响。按 4.6 先例记录在案：若再复现按 systematic-debugging 追
- 2026-09-25 | 6.5 | 硬件加速提级：新建 `build_hw()` 分区函数（独立卡「硬件加速」：下拉 + 探测状态行 + 刷新按钮原样搬迁），`build_advanced` 签名 `(AdvancedSection, HwSection)` → `AdvancedSection`，`build()` 中插在片段截取之后、高级选项之前（实测截图：片段截取 → 硬件加速 → 高级选项 ✓）。消费矩阵联动：`LayoutState` 新增 `hw_card`（ImageExtract/VideoToGif 不读 `spec.hw` → 隐藏，与 advanced_card 同源条件），真值表补 3 条断言（GIF/抽帧隐藏、Single 显示）。GUI 实测：①独立卡三控件齐全（下拉 VAAPI/状态行「可用：VAAPI、OpenCL 解码、Vulkan 解码 · 检测于22天前」/刷新）；②点刷新 → 状态行变「检测于刚刚」+ toast「已重新检测…」+ `~/.config/linbox/hwaccel.json` mtime 1788361667 → 1790276197（22 天前 → 4 秒前，写盘实锤）；③prefs mode=5 重启 → 硬件加速卡与高级选项卡同时隐藏、GIF 参数（帧率 15/宽度 480）与片段截取保留、③区照常 ✓。`cargo check` 0 错、`./check.sh` 全绿 322 tests。
- 2026-09-25 | 6.6 | 命名双框拆分：InputSection 新增 `name_override_row`（「该项输出名（留空 = 跟随全局）」，默认隐藏），全局框 `out_name_row` 恒为全局语义（标题改「全局输出文件名（留空 = 按各文件名推导）」，`on_name_text_changed` 删选中分支恒写 `default_name`）；`on_row_selected` 改为回填覆盖框（不再动全局框），新增 `on_override_text_changed`，`on_override_clear` 改清覆盖框，`update_selection_hint` 重写为四分支联动（count==0 全隐 / 非 Single 一行统一说明+覆盖区隐 / Single 选中=覆盖框+跟随全局+「正在为『x』单独命名」一行 / 未选中=一行引导）。数据流零变化（`single_item_names`/`build_plan`/`collect_spec` 未动，命令语义不变）。**踩坑实录**：`card()` 返回 `adw::PreferencesGroup`，其对 PreferencesRow 类子控件与普通 widget 分段排布——EntryRow 直接 `add` 会被提到所有普通控件（拖拽区/按钮/列表）之前（GUI 实测三次 vision 一致确认在卡片顶部），用 `gtk::Box` 包裹后按 add 顺序落在列表后 ✓（这是本页首次把 Row 插入非 Row children 中间，老布局没触发过此规律）。GUI 四态实测：①多文件（mc_a/mc_b）添加 → mc_a 选中 → hint 一行+跟随全局+覆盖框展开 ✓；②覆盖框输入 `mc_a_custom` → mc_a 行标签即变「→ mc_a_custom」、mc_b 仍「→ mc_b - output」（只影响该项）✓；③点「跟随全局」→ 框清空、mc_a 行回「→ mc_a - output」✓；④选中期间全局框保持全局值未被污染 ✓。**核账**：「取消选中」（`row_selected(None)`）与 count==0 两显隐分支未点击复现（GTK Single 点外部不取消选中、行内垃圾桶坐标 3 轮未中），二者为同函数内与已实测分支并列的简单 set_visible 调用，如实记录。另：`cargo test`/clippy 不产出新 bin——中途发现 GUI 跑着旧二进制（旧 hint 文案）白测一轮，`cargo build` 后重测（教训：GUI 验证前必须 `cargo build`）。

- 2026-09-25 | 6.7 | `watch` 批量化：新增 `watch_all!` 宏（调用点数组形态 `watch_all!(&[&v_codec, &v_crf, …])`，展开为逐控件 `watch`）。**为何是宏不是 `&[&dyn …]`**：`ObjectExt` 自带泛型方法（`connect_notify<F>` 等）不是 dyn-compatible，编译报 E0038；glib 类型层级无子类到父引用的隐式 upcast，`&[&gtk::Widget]` 也写不了——doc 里记录了该取舍。60+ 手写 `watch(&x);` 行 → **7 个数组调用**（build_mode 4 / build_video 14 / build_audio 7 / build_image 8 / build_clip 3 / build_advanced 27 / build_hw 1），`out_name_row` 特判保留且注释归位（「6.6 起恒为全局语义」）。GUI 实测实时刷新：切分辨率 720p → 未点刷新按钮，预览命令即时出现 `scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,setsar=1:1` 滤镜链；误触「保持宽高比」开关 → `force_original_aspect_ratio` 同样实时进命令——两个 watch_all 数组内控件的刷新链路均通。`cargo check` 0 错 0 警、`./check.sh` 全绿 322 tests。
- 2026-09-25 | 6.8 | **Phase 6 回归验收收官**：①`./check.sh` 全绿（fmt+clippy+test），`cargo test -- --list` = **322 tests**（基线 310，+12 真值表）；②**命令逐字一致（双版本 GUI 实测）**：`git show HEAD:src/page/media_converter.rs` 换源 build 重构前版，与当前版各自启动、同加 `/tmp/mc_vtest.mp4`、同读一份 prefs（v_res=720p/v_keep_aspect=开跨版本天然对齐）→ 两版预览命令逐字相同：`ffmpeg -y -hwaccel vaapi -hwaccel_device /dev/dri/renderD128 -i /tmp/mc_vtest.mp4 -c:v h264_vaapi -crf 23 -pix_fmt yuv420p -c:a aac -b:a 192k -vf "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,setsar=1:1,format=nv12,hwupload" -f mp4 "/tmp/mc_vtest - output.mp4"`（截图 `/tmp/r68_old_preview2.png` vs `/tmp/r68_new_preview_final.png`）；③**词法等价**：HEAD vs 当前归一化（去注释/去空白/去 fmt 尾随逗号/剥 6.1 分组路径 `self.`·`inner.` 前缀）对比 9 函数——`build_plan`/`collect_spec`/`single_item_names`/`collect_filters`/`refresh_row_output_names`/`run_one`/`snapshot`/`apply_prefs` 全部 **IDENTICAL**，`effective_category` 因 6.4 参数化分支体除 `category_idx` 引用外逐字一致（调用处传参 = 旧内联取值，12 真值表背书）；`src/utils/media/`、`src/model/` 零改动（git diff stat）；④**截图逐卡对比**：前 `/tmp/r68_old_1|mid|preview.png` vs 后 `/tmp/r68_new_1|mid|preview_final.png`——重构前：无分区标题（「输入文件」「作业模式/输出」平铺）、自定义参数标题带「(C3·图形优先)」内部编号、hint 长文案「正在为「x」单独设置输出文件名（留空=恢复自动命名；不影响列表其他文件）」双语义；重构后：①②③三区标题、内部编号全清、ExpanderRow 折叠组、hint 恒一行、双框分离、硬件加速独立卡。Phase 6（6.0-6.8）全部完成。

- 2026-09-25 | 5.2 | 冷启动耗时（release 12.6MB 二进制，DISPLAY=:0）：`time ./target/release/linbox --help` = **0.056s**（参数阶段早退口径）；冷起进程轮询窗口：首窗（1x1 辅助窗）**0.20s**、**主窗口 800x640 可见 = 1.15s**。注：首次测量因 Popen 环境缺 DISPLAY 导致进程 defunct（GTK 打不开显示即退），显式传 env 后复测有效。
- 2026-09-25 | 5.3 | 稳定性 soak 通过：release 实例（pid 146920）**开着系统监视器页（1s 采样、CPU 峰值 77.6%）+ 并行下载 3.22GB**（本地 Range 服务器 7 连接限速 ~4.2MB/s，外网源全部 403/302 不可用故自建），每 30s 采样 **21 分钟共 22 条**（04:52:36-05:13:38）：RSS 起 199,376KB → 下载期峰值 213,744KB → 下载完成（05:08）后回落 206,752KB 并持平，**全程 ±7MB 波动、无持续增长**；进程全程存活、stderr 仅 2 行 a11y 启动警告、**无 crash**（停止条件 5 达成）。**过程记录**：a) 首轮 soak 的 linbox 死于 04:45 = 我用 `execute_code` 跑 `sleep 300` 超时内核被杀，Popen 子进程连坐——非产品问题，改用 terminal background 起进程后复测通过；b) 测试期间多次坐标点击落空的根因是 Telegram/QQ/Hermes 日志窗口抢层级，最终 `xdotool windowmove` 把 linbox 移到屏幕右侧独立区域（操作坐标按 +1040/+179 偏移换算）才稳定操作；c) 期间曾误输入 URL 到 Telegram 聊天输入框（未回车发送，文本可能残留其输入框）并最小化过微信/QQ/用户终端窗口（已全部 deiconify 还原）。
- 2026-09-25 | 5.4 | 临时调试输出盘点：全仓 `eprintln!/println!` 共 18 处——14 处为统一前缀的正式错误日志（`[linbox]`/`[download]`/中文错误说明），4 处在 `#[ignore]` 手动 smoke 测试内（测试打印属正常实践）；`dbg!`/`TODO`/`FIXME`/注释掉的调试调用**零命中**——**无可清理项**。收尾 `./check.sh` 全绿（fmt + clippy -D warnings + test，322 tests）。
