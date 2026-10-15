//! systemd 单元文件的**文档模型 + 键点表 + 表单纯逻辑**（纯数据，无 UI 依赖）。
//!
//! 对应 GOAL Phase 11 的 11.1（文档模型与保真往返）与 11.2（键点表与表单纯逻辑）：
//! - 文档模型沿用 [`crate::model::fstab`] 的**行交错**思路：注释行、空行、节头、
//!   键值行按原顺序存进 [`UnitDoc::lines`]，未被编辑的行带 `raw` 原样输出，
//!   往返（parse → render）逐字节保真；改过的行才按 `Key=Value` 重新生成。
//! - 键点表 [`KEY_SPECS`] 按**单元类型**分流（service/timer/socket/mount/… 各有专用节），
//!   页面按 [`sections_for_type`] 出分组、按 [`specs_for_type`] 出控件。
//! - [`apply_form`] 只写回表单动过的键，[`validate_form`] 执行 D11.5-② 的按类型必填与值域；
//!   权威判定仍交给 `systemd-analyze verify`（在 `utils::unitfile`），这里只拦「明显坏值」。
//!
//! 关键事实（2026-10-14 本机 man 实测，白名单取值来源）：
//! - `Type=` 8 值：simple / exec / forking / oneshot / dbus / notify / notify-reload / idle
//! - `Restart=` 7 值：no / on-success / on-failure / on-abnormal / on-watchdog / on-abort / always
//! - `KillMode=` 4 值：control-group / mixed / process / none（`man systemd.kill`）
//! - `StandardOutput=`：inherit / null / tty / journal / kmsg / journal+console / kmsg+console / socket
//!   以及 `file:` `append:` `truncate:` `fd:` 参数化形式（`man systemd.exec`）
//!
//! 约束（规划书 §3.7 同款）：本文件禁止 `use gtk` / `use adw` / `use glib`。

/// 全部 11 类单元后缀（GOAL D11.3：编辑与删除的类型白名单）。
pub const UNIT_TYPES: &[&str] = &[
    "service",
    "timer",
    "target",
    "socket",
    "mount",
    "automount",
    "device",
    "path",
    "slice",
    "scope",
    "swap",
];

/// 可新建的 9 类（GOAL D11.9-⑤：`.device` 由 udev 动态生成、`.scope` 由 systemd
/// 运行时创建且 `systemd-analyze verify` 对其报 `Unit … not found.`（GOAL 摸底 14），故不开放新建）。
pub const NEWABLE_TYPES: &[&str] = &[
    "service",
    "timer",
    "target",
    "socket",
    "mount",
    "automount",
    "path",
    "slice",
    "swap",
];

// ---------------------------------------------------------------------------
// 类型识别
// ---------------------------------------------------------------------------

/// 按单元名后缀识别类型（11 类白名单）。
///
/// `sshd.service` → `Some("service")`、`mnt-foo.mount` → `Some("mount")`、
/// `foo@.service`（模板名）→ `Some("service")`；无后缀 / 未知后缀 / 空串 /
/// **含 `/` 的路径**（只认名字不认路径）→ `None`。
/// 大小写敏感：后缀必须小写（systemd 单元名本身大小写敏感）。
pub fn type_of(name: &str) -> Option<&'static str> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let (base, suffix) = name.rsplit_once('.')?;
    if base.is_empty() {
        return None;
    }
    UNIT_TYPES.iter().copied().find(|t| *t == suffix)
}

/// 判断文本是否 `systemctl cat` 的合并输出（**不能当编辑源**，GOAL 摸底 1）。
///
/// cat 输出的特征：靠前的行里出现 `# /绝对路径/xxx.<unit后缀>` 或 `# …/<名字>.d/<文件>`。
/// 只在前 8 行里找，避免把正文里的普通注释误判（真实单元文件几乎不写 `# /…`）。
pub fn looks_like_systemctl_cat(text: &str) -> bool {
    for (i, line) in text.lines().enumerate() {
        if i >= 8 {
            break;
        }
        let Some(rest) = line.strip_prefix("# /") else {
            continue;
        };
        let rest = rest.trim_end();
        // 掉进 .d/ 目录的 drop-in 头行，或以已知单元后缀结尾的文件头行
        if rest.contains(".d/") {
            return true;
        }
        if UNIT_TYPES.iter().any(|t| rest.ends_with(&format!(".{t}"))) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 文档模型（行交错 + raw 保真）
// ---------------------------------------------------------------------------

/// 单元文件里的一条键值行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitEntry {
    /// 所属节名（不含方括号），如 `Unit` / `Service` / `Install`。
    pub section: String,
    /// 键名（含可能的限定符；systemd 单元文件没有 locale 变体键）。
    pub key: String,
    /// 值（已去掉行尾 `\r`；**不含**续行部分）。
    pub value: String,
    /// 解析时的原始行。`Some` = 未修改，序列化原样输出；setter 置 `None`。
    pub raw: Option<String>,
    /// 值以反斜杠结尾（systemd 续行）。续行键**只读保真**，表单禁止编辑
    /// （本机 `/usr/lib/systemd/system` 实测 0 处续行，但外部单元可能有）。
    pub continued: bool,
}

impl UnitEntry {
    fn new(section: &str, key: &str, value: &str, raw: Option<String>, continued: bool) -> Self {
        UnitEntry {
            section: section.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            raw,
            continued,
        }
    }

    /// 规范行文本（不含换行）：`Key=Value`。`raw` 存在时由 [`UnitLine::to_line_string`] 优先用 raw。
    pub fn canonical_line(&self) -> String {
        format!("{}={}", self.key, self.value)
    }

    /// 改值（清 raw → 此后按 canonical 输出）。续行键由上层拦截，这里不重复判。
    fn set_value(&mut self, v: impl Into<String>) {
        self.value = v.into();
        self.raw = None;
    }
}

/// 单元文件里的一行（行交错文档模型的最小单元）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitLine {
    /// 注释 / 空行 / 节外行 / 续行，原样保留。
    Raw(String),
    /// 节头（如 `[Service]`）；`raw` 保留原文（含两侧空白/大小写）。
    Section { raw: String, name: String },
    /// 一条键值行。
    Entry(UnitEntry),
}

impl UnitLine {
    /// 序列化为一行文本（不含换行）：条目优先 `raw`（未修改保真），否则生成规范行。
    pub fn to_line_string(&self) -> String {
        match self {
            UnitLine::Raw(s) => s.clone(),
            UnitLine::Section { raw, .. } => raw.clone(),
            UnitLine::Entry(e) => e.raw.clone().unwrap_or_else(|| e.canonical_line()),
        }
    }
}

/// 整个单元文件文档（条目 + 注释/空行/节头交错，保序）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitDoc {
    /// 按文件顺序交错的行。
    pub lines: Vec<UnitLine>,
    /// 原文件是否以换行结尾（`a\nb` 与 `a\nb\n` 是两个不同的文件，往返要分清）。
    pub trailing_newline: bool,
}

impl UnitDoc {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只看键值行，保序。
    pub fn entries(&self) -> impl Iterator<Item = &UnitEntry> {
        self.lines.iter().filter_map(|l| match l {
            UnitLine::Entry(e) => Some(e),
            _ => None,
        })
    }

    /// 可变遍历键值行（就地编辑用）。
    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut UnitEntry> {
        self.lines.iter_mut().filter_map(|l| match l {
            UnitLine::Entry(e) => Some(e),
            _ => None,
        })
    }

    /// 按文件顺序去重列出节名。
    pub fn section_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for l in &self.lines {
            if let UnitLine::Section { name, .. } = l
                && !out.iter().any(|s| s == name)
            {
                out.push(name.clone());
            }
        }
        out
    }

    /// 是否存在该节。
    pub fn has_section(&self, section: &str) -> bool {
        self.entries().any(|e| e.section == section)
            || self
                .lines
                .iter()
                .any(|l| matches!(l, UnitLine::Section { name, .. } if name == section))
    }

    /// 该节是否存在但**没有任何键**（只有节头）。
    fn section_header_index(&self, section: &str) -> Option<usize> {
        self.lines
            .iter()
            .position(|l| matches!(l, UnitLine::Section { name, .. } if name == section))
    }

    /// 某键是否用了续行（表单必须只读）。
    pub fn key_is_continued(&self, section: &str, key: &str) -> bool {
        self.entries()
            .any(|e| e.section == section && e.key == key && e.continued)
    }

    /// 取某键的全部值（多值键保序），不存在返回空 Vec。
    pub fn get_values(&self, section: &str, key: &str) -> Vec<String> {
        self.entries()
            .filter(|e| e.section == section && e.key == key)
            .map(|e| e.value.clone())
            .collect()
    }

    /// 取某键的第一个值。
    pub fn get_value(&self, section: &str, key: &str) -> Option<String> {
        self.entries()
            .find(|e| e.section == section && e.key == key)
            .map(|e| e.value.clone())
    }

    /// 该键是否存在（任意一节内）。
    pub fn has_key(&self, section: &str, key: &str) -> bool {
        self.entries().any(|e| e.section == section && e.key == key)
    }

    /// 确保节存在（不存在则在文档**末尾**追加节头；节名不带方括号）。
    pub fn ensure_section(&mut self, section: &str) {
        if self.section_header_index(section).is_none()
            && !self
                .lines
                .iter()
                .any(|l| matches!(l, UnitLine::Section { name, .. } if name == section))
        {
            self.lines.push(UnitLine::Section {
                raw: format!("[{section}]"),
                name: section.to_string(),
            });
        }
    }

    /// 整组替换某键的值。
    ///
    /// - `values` 为空 → 删除该键的全部行；
    /// - 有值 → 首个既有行改成 `values[0]`（位置不变），多余旧行删除，其余值紧随其后插入；
    ///   该键不存在 → 追加到该节末尾（节不存在则先建节）。
    ///
    /// 返回 `false` = 没有任何可改的行且没写入（键不存在且 `values` 为空）；
    /// **续行键一律拒绝**（返回 `false`，调用方按错误处理，避免把续行拆成孤儿行）。
    pub fn set_values(&mut self, section: &str, key: &str, values: &[String]) -> bool {
        if self.key_is_continued(section, key) {
            return false;
        }
        let idxs: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| match l {
                UnitLine::Entry(e) if e.section == section && e.key == key => Some(i),
                _ => None,
            })
            .collect();

        if idxs.is_empty() {
            if values.is_empty() {
                return false;
            }
            // 节不存在 → 末尾建节
            self.ensure_section(section);
            // 找节范围：从节头（或文件末）到下一个节头
            let start = self
                .section_header_index(section)
                .map(|i| i + 1)
                .unwrap_or(self.lines.len());
            let mut end = self.lines.len();
            for (i, l) in self.lines.iter().enumerate().skip(start) {
                if matches!(l, UnitLine::Section { .. }) {
                    end = i;
                    break;
                }
            }
            let first = UnitEntry::new(section, key, &values[0], None, false);
            let mut new_lines = vec![UnitLine::Entry(first)];
            for v in &values[1..] {
                new_lines.push(UnitLine::Entry(UnitEntry::new(
                    section, key, v, None, false,
                )));
            }
            let tail = self.lines.split_off(end);
            self.lines.extend(new_lines);
            self.lines.extend(tail);
            return true;
        }

        if values.is_empty() {
            // 从后往前删
            for i in idxs.into_iter().rev() {
                self.lines.remove(i);
            }
            return true;
        }

        // 首个位置改值，其余重复行删除
        let first = idxs[0];
        for i in idxs[1..].iter().rev() {
            self.lines.remove(*i);
        }
        {
            let UnitLine::Entry(e) = &mut self.lines[first] else {
                return false;
            };
            e.set_value(values[0].clone());
        }
        // 其余值紧随其后插入
        for (i, v) in values[1..].iter().enumerate() {
            self.lines.insert(
                first + 1 + i,
                UnitLine::Entry(UnitEntry::new(section, key, v, None, false)),
            );
        }
        true
    }

    /// 删除某键的全部行（不存在则返回 false）。
    pub fn remove_key(&mut self, section: &str, key: &str) -> bool {
        if self.key_is_continued(section, key) {
            return false;
        }
        let before = self.lines.len();
        self.lines
            .retain(|l| !matches!(l, UnitLine::Entry(e) if e.section == section && e.key == key));
        self.lines.len() != before
    }

    /// 追加一行（新建文档用）。
    pub fn push_line(&mut self, line: UnitLine) {
        self.lines.push(line);
    }
}

/// 解析单元文件文本 → [`UnitDoc`]（**不 panic**，坏输入尽量原样保留）。
///
/// 分类规则：
/// - `[…]`（trim 后）→ 节头；
/// - 位于节内、非注释（`#` / `;` 开头）、含 `=` → 键值行（键取第一个 `=` 前并 trim）；
/// - 其余（注释、空行、节外行、**续行**）→ `Raw` 原样保留。
///
/// 续行：若上一行的值以 `\` 结尾，下一行**强制按 Raw 处理**（不解析成键），
/// 并把上一行标记 `continued = true`（表单据此只读）。
pub fn parse_unit_text(text: &str) -> UnitDoc {
    let mut doc = UnitDoc::new();
    if text.is_empty() {
        return doc;
    }
    doc.trailing_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut section: Option<String> = None;
    let mut expect_continuation = false;

    for raw in body.split('\n') {
        if expect_continuation {
            // 续行原样保留，绝不能被当成键
            doc.lines.push(UnitLine::Raw(raw.to_string()));
            expect_continuation = raw.trim_end().ends_with('\\');
            continue;
        }

        let t = raw.trim();
        if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
            let name = t[1..t.len() - 1].to_string();
            doc.lines.push(UnitLine::Section {
                raw: raw.to_string(),
                name,
            });
            section = Some(t[1..t.len() - 1].to_string());
            continue;
        }

        let in_section = section.is_some();
        let is_comment = t.starts_with('#') || t.starts_with(';');
        if in_section
            && !is_comment
            && !t.is_empty()
            && let Some(eq) = t.find('=')
        {
            let key = t[..eq].trim().to_string();
            let mut value = t[eq + 1..].to_string();
            // CRLF：值里剥掉行尾 \r（raw 仍带 \r，未修改行往返不丢）
            if value.ends_with('\r') {
                value.pop();
            }
            if !key.is_empty() {
                let continued = value.ends_with('\\');
                let sec = section.clone().unwrap_or_default();
                doc.lines.push(UnitLine::Entry(UnitEntry::new(
                    &sec,
                    &key,
                    &value,
                    Some(raw.to_string()),
                    continued,
                )));
                expect_continuation = continued;
                continue;
            }
        }
        doc.lines.push(UnitLine::Raw(raw.to_string()));
    }
    doc
}

/// 序列化 → 文本（未修改行逐字节保真，修改行按 `Key=Value` 生成）。
pub fn render_unit_text(doc: &UnitDoc) -> String {
    if doc.lines.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for line in &doc.lines {
        out.push_str(&line.to_line_string());
        out.push('\n');
    }
    if !doc.trailing_newline {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// 键点表（KEY_SPEC）
// ---------------------------------------------------------------------------

/// 表单控件类型（页面据此渲染输入方式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// 单行文本。
    Text,
    /// 下拉（值域见 [`KeySpec::options`]）。
    Select,
    /// 开关（值只有 true/false）。
    Toggle,
    /// 数字（值仍是字符串，允许 `5s` 这类时长，见 [`validate_form`]）。
    Number,
    /// 可增删列表（多值键，整组替换）。
    List,
}

/// 一个可编辑键点的完整定义。
#[derive(Clone, Copy, Debug)]
pub struct KeySpec {
    /// 所属节名（不带方括号）。
    pub section: &'static str,
    /// 键名。
    pub key: &'static str,
    /// 中文标签（页面行标题）。
    pub label: &'static str,
    /// 一句话说明（页面副标题 / tooltip）。
    pub hint: &'static str,
    /// 控件类型。
    pub control: Control,
    /// `Select` 的取值清单（其余控件为空切片）。
    pub options: &'static [&'static str],
    /// 适用单元类型（`UNIT_TYPES` = 全类型通用；专用键只列自己的类型）。
    pub types: &'static [&'static str],
    /// 是否该类型的常见必填项（精确规则见 `validate_form` 的 per-type 判定）。
    pub required: bool,
}

/// `Type=` 取值（`man systemd.service` 本机 systemd 255 实测，8 值）。
pub const SERVICE_TYPES: &[&str] = &[
    "simple",
    "exec",
    "forking",
    "oneshot",
    "dbus",
    "notify",
    "notify-reload",
    "idle",
];

/// `Restart=` 取值（`man systemd.service` 本机实测，7 值）。
pub const RESTART_VALUES: &[&str] = &[
    "no",
    "on-success",
    "on-failure",
    "on-abnormal",
    "on-watchdog",
    "on-abort",
    "always",
];

/// `KillMode=` 取值（`man systemd.kill` 本机实测，4 值）。
pub const KILLMODE_VALUES: &[&str] = &["control-group", "mixed", "process", "none"];

/// `StandardOutput=` / `StandardError=` 基础取值（`man systemd.exec` 本机实测）。
pub const STD_STREAM_VALUES: &[&str] = &[
    "inherit",
    "null",
    "tty",
    "journal",
    "kmsg",
    "journal+console",
    "kmsg+console",
    "socket",
];

/// `StandardOutput=` / `StandardError=` 的参数化前缀（后接路径/名称）。
pub const STD_STREAM_PREFIXES: &[&str] = &["file:", "append:", "truncate:", "fd:"];

/// 时长类数字允许的单位（`man systemd.time` 的常用子集）。
pub const TIME_UNITS: &[&str] = &["ms", "us", "s", "min", "h", "d", "w", "month", "year"];

/// 全部类型（用于 `[Unit]` / `[Install]` 通用键的 `types` 字段）。
const ALL: &[&str] = UNIT_TYPES;

/// 键点表：**顺序即页面分组顺序**——`[Unit]` → 各类型专用节 → `[Install]`（放最后，
/// 保证 `sections_for_type` 产出 `Unit → Service → Install` 这类正确顺序）。
///
/// 收录范围按 GOAL 摸底 5 的高频键 + 各类型专用必键；沙箱类长尾键（Protect* /
/// SystemCallFilter 等几百个）**不进表单**、由文档模型原样保真（D11.4）。
pub static KEY_SPECS: &[KeySpec] = &[
    // ── [Unit]（全类型通用）─────────────────────────────────────────────
    KeySpec {
        section: "Unit",
        key: "Description",
        label: "描述",
        hint: "出现在应用菜单与 systemctl 里的简介",
        control: Control::Text,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "Documentation",
        label: "文档链接",
        hint: "man:cron(8) 或 https://…，可多条",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "After",
        label: "启动顺序：在其之后",
        hint: "只排序不建依赖，本单元在这些单元之后启动",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "Before",
        label: "启动顺序：在其之前",
        hint: "本单元在这些单元之前启动",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "Wants",
        label: "弱依赖",
        hint: "拉起这些单元，它们失败不影响本单元",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "Requires",
        label: "强依赖",
        hint: "这些单元失败/停止时，本单元一并失败",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "Conflicts",
        label: "互斥单元",
        hint: "与这些单元不能同时运行",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "DefaultDependencies",
        label: "默认依赖",
        hint: "自动加入基础依赖（一般保持开启）",
        control: Control::Toggle,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Unit",
        key: "ConditionPathExists",
        label: "路径存在才启动",
        hint: "路径不存在则跳过启动（不算失败）",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    // ── [Service]（仅 service）──────────────────────────────────────────
    KeySpec {
        section: "Service",
        key: "Type",
        label: "服务类型",
        hint: "进程如何转后台、如何判定启动完成",
        control: Control::Select,
        options: SERVICE_TYPES,
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "ExecStart",
        label: "启动命令",
        hint: "主进程的完整命令（oneshot 可留空，其它类型必填）",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: true,
    },
    KeySpec {
        section: "Service",
        key: "ExecStop",
        label: "停止命令",
        hint: "停止该服务时执行的命令",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "ExecReload",
        label: "重载命令",
        hint: "reload 时执行（提供后 reload 按钮才有效）",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "User",
        label: "运行用户",
        hint: "以哪个用户身份运行（默认 root）",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "Group",
        label: "运行组",
        hint: "以哪个组身份运行",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "WorkingDirectory",
        label: "工作目录",
        hint: "启动进程时的 cwd",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "Environment",
        label: "环境变量",
        hint: "KEY=VALUE 形式，可多条",
        control: Control::List,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "EnvironmentFile",
        label: "环境变量文件",
        hint: "从文件读取环境变量，前导 - 表示缺失不报错",
        control: Control::List,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "Restart",
        label: "重启策略",
        hint: "进程退出后是否自动重启",
        control: Control::Select,
        options: RESTART_VALUES,
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "RestartSec",
        label: "重启间隔",
        hint: "重启前等待，如 5 / 5s / 500ms（不接受负数）",
        control: Control::Number,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "TimeoutSec",
        label: "启停超时",
        hint: "启动/停止的最长等待，如 90s",
        control: Control::Number,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "RemainAfterExit",
        label: "退出后仍算运行中",
        hint: "oneshot 常用：命令跑完仍视为 active",
        control: Control::Toggle,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "KillMode",
        label: "停止时如何杀进程",
        hint: "control-group=全杀；process=只杀主进程",
        control: Control::Select,
        options: KILLMODE_VALUES,
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "StandardOutput",
        label: "标准输出去向",
        hint: "journal=进系统日志；null=丢弃；file:路径=写文件",
        control: Control::Select,
        options: STD_STREAM_VALUES,
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "StandardError",
        label: "标准错误去向",
        hint: "同上，作用于 stderr",
        control: Control::Select,
        options: STD_STREAM_VALUES,
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "Nice",
        label: "优先级 (Nice)",
        hint: "-20（最高）到 19（最低），默认 0",
        control: Control::Number,
        options: &[],
        types: &["service"],
        required: false,
    },
    KeySpec {
        section: "Service",
        key: "BusName",
        label: "D-Bus 名称",
        hint: "Type=dbus 时等待的总线名",
        control: Control::Text,
        options: &[],
        types: &["service"],
        required: false,
    },
    // ── [Timer]（仅 timer）──────────────────────────────────────────────
    KeySpec {
        section: "Timer",
        key: "OnCalendar",
        label: "定时表达式",
        hint: "如 *-*-* 08:00:00（真实样例见本机 timer）",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "OnBootSec",
        label: "开机后 N 秒",
        hint: "如 1min / 30s",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "OnStartupSec",
        label: "用户实例启动后 N 秒",
        hint: "用户级 timer 常用",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "OnUnitActiveSec",
        label: "上次触发后 N 秒",
        hint: "相对上次激活的时间",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "OnUnitInactiveSec",
        label: "上次结束后 N 秒",
        hint: "相对上次结束的时间",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "RandomizedDelaySec",
        label: "随机延迟",
        hint: "触发前的随机抖动，如 12h",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "Persistent",
        label: "错过的触发补跑",
        hint: "关机期间错过的 calendar 任务开机后补跑",
        control: Control::Toggle,
        options: &[],
        types: &["timer"],
        required: false,
    },
    KeySpec {
        section: "Timer",
        key: "Unit",
        label: "触发的目标单元",
        hint: "默认同名 .service，可指定别的单元",
        control: Control::Text,
        options: &[],
        types: &["timer"],
        required: false,
    },
    // ── [Socket]（仅 socket）────────────────────────────────────────────
    KeySpec {
        section: "Socket",
        key: "ListenStream",
        label: "TCP/UNIX 监听地址",
        hint: "8080 / 127.0.0.1:80 /run/foo.sock，可多条",
        control: Control::List,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "ListenDatagram",
        label: "UDP 监听地址",
        hint: "同上，数据报套接字",
        control: Control::List,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "ListenSequentialPacket",
        label: "SEQPACKET 监听",
        hint: "SOCK_SEQPACKET 套接字",
        control: Control::List,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "Accept",
        label: "每连接起服务实例",
        hint: "true 时每个连接激活一个服务实例",
        control: Control::Toggle,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "Service",
        label: "对应服务单元",
        hint: "默认同名 .service",
        control: Control::Text,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "SocketUser",
        label: "套接字属主用户",
        hint: "套接字文件的属主",
        control: Control::Text,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "SocketGroup",
        label: "套接字属组",
        hint: "套接字文件的属组",
        control: Control::Text,
        options: &[],
        types: &["socket"],
        required: false,
    },
    KeySpec {
        section: "Socket",
        key: "SocketMode",
        label: "套接字权限",
        hint: "如 0660",
        control: Control::Text,
        options: &[],
        types: &["socket"],
        required: false,
    },
    // ── [Mount]（仅 mount）──────────────────────────────────────────────
    KeySpec {
        section: "Mount",
        key: "What",
        label: "挂载来源",
        hint: "设备路径或 UUID=…（必填）",
        control: Control::Text,
        options: &[],
        types: &["mount"],
        required: true,
    },
    KeySpec {
        section: "Mount",
        key: "Where",
        label: "挂载点",
        hint: "如 /mnt/data（必须与单元名一致：/mnt/data → mnt-data.mount）",
        control: Control::Text,
        options: &[],
        types: &["mount"],
        required: true,
    },
    KeySpec {
        section: "Mount",
        key: "Type",
        label: "文件系统类型",
        hint: "如 ext4 / btrfs / nfs",
        control: Control::Text,
        options: &[],
        types: &["mount"],
        required: false,
    },
    KeySpec {
        section: "Mount",
        key: "Options",
        label: "挂载选项",
        hint: "逗号分隔，如 defaults,nofail",
        control: Control::List,
        options: &[],
        types: &["mount"],
        required: false,
    },
    KeySpec {
        section: "Mount",
        key: "TimeoutSec",
        label: "挂载超时",
        hint: "如 90s",
        control: Control::Number,
        options: &[],
        types: &["mount"],
        required: false,
    },
    // ── [Automount]（仅 automount）──────────────────────────────────────
    KeySpec {
        section: "Automount",
        key: "Where",
        label: "挂载点",
        hint: "触发挂载的目标路径（必须与单元名一致）",
        control: Control::Text,
        options: &[],
        types: &["automount"],
        required: true,
    },
    // ── [Path]（仅 path）────────────────────────────────────────────────
    KeySpec {
        section: "Path",
        key: "PathExists",
        label: "路径存在即触发",
        hint: "检查路径存在性，可多条",
        control: Control::List,
        options: &[],
        types: &["path"],
        required: false,
    },
    KeySpec {
        section: "Path",
        key: "PathChanged",
        label: "路径变更即触发",
        hint: "监控文件变更（比较 inode/ctime）",
        control: Control::List,
        options: &[],
        types: &["path"],
        required: false,
    },
    KeySpec {
        section: "Path",
        key: "PathModified",
        label: "路径修改即触发",
        hint: "含写入内容的变化",
        control: Control::List,
        options: &[],
        types: &["path"],
        required: false,
    },
    KeySpec {
        section: "Path",
        key: "DirectoryNotEmpty",
        label: "目录非空即触发",
        hint: "目录里出现文件就触发",
        control: Control::List,
        options: &[],
        types: &["path"],
        required: false,
    },
    KeySpec {
        section: "Path",
        key: "MakeDirectory",
        label: "自动创建目录",
        hint: "监控路径不存在时自动建目录",
        control: Control::Toggle,
        options: &[],
        types: &["path"],
        required: false,
    },
    KeySpec {
        section: "Path",
        key: "Unit",
        label: "触发的目标单元",
        hint: "默认同名 .service",
        control: Control::Text,
        options: &[],
        types: &["path"],
        required: false,
    },
    // ── [Swap]（仅 swap）────────────────────────────────────────────────
    KeySpec {
        section: "Swap",
        key: "What",
        label: "swap 设备",
        hint: "如 /dev/sda2（必须与单元名一致：/dev/sda2 → dev-sda2.swap）",
        control: Control::Text,
        options: &[],
        types: &["swap"],
        required: true,
    },
    KeySpec {
        section: "Swap",
        key: "Options",
        label: "挂载选项",
        hint: "逗号分隔",
        control: Control::List,
        options: &[],
        types: &["swap"],
        required: false,
    },
    KeySpec {
        section: "Swap",
        key: "TimeoutSec",
        label: "启用超时",
        hint: "如 90s",
        control: Control::Number,
        options: &[],
        types: &["swap"],
        required: false,
    },
    // ── [Slice]（仅 slice）──────────────────────────────────────────────
    KeySpec {
        section: "Slice",
        key: "MemoryMax",
        label: "内存上限",
        hint: "如 512M / 50%",
        control: Control::Text,
        options: &[],
        types: &["slice"],
        required: false,
    },
    KeySpec {
        section: "Slice",
        key: "CPUQuota",
        label: "CPU 配额",
        hint: "如 200%（最多占两核）",
        control: Control::Text,
        options: &[],
        types: &["slice"],
        required: false,
    },
    KeySpec {
        section: "Slice",
        key: "TasksMax",
        label: "最大进程数",
        hint: "如 512 / infinity",
        control: Control::Text,
        options: &[],
        types: &["slice"],
        required: false,
    },
    // ── [Install]（放最后 → sections_for_type 的 Install 恒在末尾）───────
    KeySpec {
        section: "Install",
        key: "WantedBy",
        label: "开机被谁拉起",
        hint: "必须是 .target，如 multi-user.target / timers.target",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Install",
        key: "RequiredBy",
        label: "强依赖它的单元",
        hint: "必须是 .target",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Install",
        key: "Also",
        label: "同时启用/禁用的单元",
        hint: "enable/disable 时一并操作",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
    KeySpec {
        section: "Install",
        key: "Alias",
        label: "别名",
        hint: "启用时创建的别名单元名",
        control: Control::List,
        options: &[],
        types: ALL,
        required: false,
    },
];

/// 按 (节, 键) 查键点定义。
pub fn spec_for(section: &str, key: &str) -> Option<&'static KeySpec> {
    KEY_SPECS
        .iter()
        .find(|s| s.section == section && s.key == key)
}

/// 该单元类型的全部键点（保持 [`KEY_SPECS`] 顺序 → 分组顺序天然正确）。
pub fn specs_for_type(unit_type: &str) -> Vec<&'static KeySpec> {
    KEY_SPECS
        .iter()
        .filter(|s| s.types.contains(&unit_type))
        .collect()
}

/// 该单元类型的节顺序（去重、保序），如 service → `[Unit, Service, Install]`、
/// timer → `[Unit, Timer, Install]`、target → `[Unit, Install]`。
pub fn sections_for_type(unit_type: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for s in specs_for_type(unit_type) {
        if !out.contains(&s.section) {
            out.push(s.section);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 表单（apply / validate）
// ---------------------------------------------------------------------------

/// 表单里一个字段的值。**`Unset` = 用户没动 → 绝不改文档**（D11.4「空值 = 不改」，
/// 删除必须显式传 `Text("")` / `List(vec![])`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormValue {
    /// 未修改（跳过）。
    Unset,
    /// 单值；空串 = 删除该键。
    Text(String),
    /// 多值（整组替换）；空 vec = 删除该键。
    List(Vec<String>),
    /// 开关。
    Bool(bool),
}

/// 一次表单提交：`(节, 键, 值)` 序列。
#[derive(Clone, Debug, Default)]
pub struct FormValues {
    pub fields: Vec<(String, String, FormValue)>,
}

impl FormValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_text(mut self, section: &str, key: &str, v: impl Into<String>) -> Self {
        self.fields
            .push((section.into(), key.into(), FormValue::Text(v.into())));
        self
    }

    pub fn set_list(mut self, section: &str, key: &str, v: Vec<String>) -> Self {
        self.fields
            .push((section.into(), key.into(), FormValue::List(v)));
        self
    }

    pub fn set_bool(mut self, section: &str, key: &str, v: bool) -> Self {
        self.fields
            .push((section.into(), key.into(), FormValue::Bool(v)));
        self
    }

    pub fn unset(mut self, section: &str, key: &str) -> Self {
        self.fields
            .push((section.into(), key.into(), FormValue::Unset));
        self
    }
}

/// 把表单写回文档：**只写回动过的键**，返回新文档（原 doc 不变）。
///
/// - 键不在 [`KEY_SPECS`] / 不适用于该类型 → `Err`（防任意键注入）；
/// - 该键是续行键 → `Err`（只读保真，D11.4）；
/// - 值含换行 → `Err`（单元文件一行一键）。
///
/// 签名比 GOAL 11.2 原文多了 `unit_type`（键的适用性按类型判定）与 `Result`
/// （错误通道），属实现期必要细化，已记入进度记录。
pub fn apply_form(doc: &UnitDoc, form: &FormValues, unit_type: &str) -> Result<UnitDoc, String> {
    if !UNIT_TYPES.contains(&unit_type) {
        return Err(format!("未知单元类型：{unit_type}"));
    }
    let mut out = doc.clone();
    for (section, key, value) in &form.fields {
        let Some(spec) = spec_for(section, key) else {
            return Err(format!("未知键：[{section}] {key}"));
        };
        if !spec.types.contains(&unit_type) {
            return Err(format!("键 [{section}] {key} 不适用于 {unit_type} 单元"));
        }
        if value == &FormValue::Unset {
            continue;
        }
        if out.key_is_continued(section, key) {
            return Err(format!(
                "[{section}] {key} 使用了续行写法，已按只读保真处理，不能通过表单修改"
            ));
        }
        match value {
            FormValue::Unset => {}
            FormValue::Text(s) => {
                ensure_no_newline(s)?;
                if s.is_empty() {
                    out.remove_key(section, key);
                } else if !out.set_values(section, key, std::slice::from_ref(s)) {
                    return Err(format!("[{section}] {key} 写入失败（续行键被拒绝）"));
                }
            }
            FormValue::List(items) => {
                if items.iter().any(|i| i.contains('\n')) {
                    return Err(format!("[{section}] {key} 的值不能包含换行"));
                }
                if items.is_empty() {
                    out.remove_key(section, key);
                } else if !out.set_values(section, key, items) {
                    return Err(format!("[{section}] {key} 写入失败（续行键被拒绝）"));
                }
            }
            FormValue::Bool(b) => {
                let v = if *b { "true" } else { "false" };
                if !out.set_values(section, key, &[v.to_string()]) {
                    return Err(format!("[{section}] {key} 写入失败（续行键被拒绝）"));
                }
            }
        }
    }
    Ok(out)
}

fn ensure_no_newline(v: &str) -> Result<(), String> {
    if v.contains('\n') || v.contains('\r') {
        return Err("值不能包含换行".to_string());
    }
    Ok(())
}

/// 校验文档（按类型，D11.5-②）。
///
/// `unit_name` 传当前单元名时，额外校验 `mount`/`automount` 的 `Where=` 与
/// `swap` 的 `What=` 必须与文件名匹配（GOAL 摸底 14 实测 verify 的同一规则，
/// 在自家这道先拦）；传 `None` 跳过该检查（新建时名字由值生成，天然匹配）。
pub fn validate_form(
    doc: &UnitDoc,
    unit_type: &str,
    unit_name: Option<&str>,
) -> Result<(), String> {
    if !UNIT_TYPES.contains(&unit_type) {
        return Err(format!("未知单元类型：{unit_type}"));
    }

    // ① 按类型必填
    for spec in specs_for_type(unit_type) {
        if !spec.required {
            continue;
        }
        let v = doc.get_value(spec.section, spec.key).unwrap_or_default();
        if !v.trim().is_empty() {
            continue;
        }
        // oneshot 允许没有 ExecStart（权威判定交给 systemd-analyze verify）
        if unit_type == "service" && spec.key == "ExecStart" {
            let t = doc.get_value("Service", "Type").unwrap_or_default();
            if t == "oneshot" {
                continue;
            }
        }
        return Err(format!(
            "[{}] {} 是 {unit_type} 单元的必填项",
            spec.section, spec.key
        ));
    }

    // ② 值域（只管表单收录的键；未知长尾键交给 verify）
    for e in doc.entries() {
        let Some(spec) = spec_for(&e.section, &e.key) else {
            continue;
        };
        if !spec.types.contains(&unit_type) {
            // 该类型不适用的键（例如在 timer 里出现 Type=）：文件里本来就可能有，
            // 不做值域校验，但也不算错——权威判定在 verify。
            continue;
        }
        ensure_no_newline(&e.value)?;
        validate_single(spec, &e.value)?;
    }

    // ③ 名值匹配（Where/What ↔ 单元名）
    if let Some(name) = unit_name {
        let Some(kind) = type_of(name) else {
            return Err(format!("单元名不合法（无法识别类型）：{name}"));
        };
        match kind {
            "mount" | "automount" => {
                let sec = if kind == "mount" {
                    "Mount"
                } else {
                    "Automount"
                };
                if let Some(where_v) = doc.get_value(sec, "Where") {
                    let expect = unit_name_from_path(&where_v, kind);
                    if expect != name {
                        return Err(format!(
                            "Where={where_v} 与单元名 {name} 不匹配（按规则应命名为 {expect}）"
                        ));
                    }
                }
            }
            "swap" => {
                if let Some(what) = doc.get_value("Swap", "What") {
                    let expect = unit_name_from_path(&what, "swap");
                    if expect != name {
                        return Err(format!(
                            "What={what} 与单元名 {name} 不匹配（按规则应命名为 {expect}）"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 路径 → 单元名（systemd 规则：去掉开头 `/`，其余 `/` 换成 `-`，再加类型后缀）。
///
/// `/mnt/foo` + mount → `mnt-foo.mount`；`/dev/sda2` + swap → `dev-sda2.swap`。
pub fn unit_name_from_path(path: &str, unit_type: &str) -> String {
    let body = path.trim_start_matches('/').replace('/', "-");
    format!("{body}.{unit_type}")
}

/// 单个值按控件类型校验。
fn validate_single(spec: &KeySpec, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(()); // 空值 = 键不存在/已删，必填判定在上层
    }
    let bad = |msg: String| Err(format!("[{}] {}：{msg}", spec.section, spec.key));
    match spec.control {
        Control::Text => Ok(()),
        Control::Select => {
            if spec.options.contains(&value) {
                return Ok(());
            }
            // StandardOutput/Error 允许参数化形式 file:/append:/truncate:/fd:
            if (spec.key == "StandardOutput" || spec.key == "StandardError")
                && STD_STREAM_PREFIXES.iter().any(|p| value.starts_with(p))
                && value.len() > value.find(':').map_or(0, |i| i + 1)
            {
                return Ok(());
            }
            bad(format!(
                "取值 {value:?} 不在允许集合（可用：{}）",
                spec.options.join(" / ")
            ))
        }
        Control::Toggle => {
            if value == "true" || value == "false" {
                Ok(())
            } else {
                bad(format!("{value:?} 不是 true/false"))
            }
        }
        Control::Number => {
            if spec.key == "Nice" {
                match value.parse::<i32>() {
                    Ok(n) if (-20..=19).contains(&n) => Ok(()),
                    _ => bad(format!("{value:?} 需为 -20..=19 的整数")),
                }
            } else {
                validate_time_span(value)
                    .map_err(|e| format!("[{}] {}：{e}", spec.section, spec.key))
            }
        }
        Control::List => match spec.key {
            "Environment" => {
                if env_item_ok(value) {
                    Ok(())
                } else {
                    bad(format!("{value:?} 不是 KEY=VALUE 形式"))
                }
            }
            "WantedBy" | "RequiredBy" => {
                if value.ends_with(".target") {
                    Ok(())
                } else {
                    bad(format!("{value:?} 必须以 .target 结尾"))
                }
            }
            _ => Ok(()),
        },
    }
}

/// `Environment=` 单项：`KEY=VALUE`（KEY 为标识符），或整串被 `"` / `'` 包裹。
pub fn env_item_ok(v: &str) -> bool {
    if v.is_empty() {
        return false;
    }
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return true;
    }
    let Some((k, _)) = v.split_once('=') else {
        return false;
    };
    let mut cs = k.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 时长/计数类数字：非负数，可带 [`TIME_UNITS`] 单位；`infinity` 合法。
pub fn validate_time_span(v: &str) -> Result<(), String> {
    let v = v.trim();
    if v.is_empty() {
        return Err("不能为空".to_string());
    }
    if v == "infinity" {
        return Ok(());
    }
    let cut = v.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(v.len());
    let (num, unit) = (&v[..cut], &v[cut..]);
    if num.is_empty() {
        return Err(format!("{v:?} 缺少数值部分"));
    }
    if !unit.is_empty() && !TIME_UNITS.contains(&unit) {
        return Err(format!(
            "{v:?} 的单位 {unit:?} 不合法（可用：{}）",
            TIME_UNITS.join("/")
        ));
    }
    match num.parse::<f64>() {
        Ok(n) if n >= 0.0 => Ok(()),
        Ok(_) => Err(format!("{v:?} 不能为负数")),
        Err(_) => Err(format!("{v:?} 不是数字")),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 golden ①：本机 `/usr/lib/systemd/system/cron.service` 全文（GOAL 11.1-①）。
    const CRON: &str = "\
[Unit]
Description=Regular background program processing daemon
Documentation=man:cron(8)
After=remote-fs.target nss-user-lookup.target

[Service]
EnvironmentFile=-/etc/default/cron
ExecStart=/usr/sbin/cron -f $EXTRA_OPTS
IgnoreSIGPIPE=false
KillMode=process
Restart=on-failure

[Install]
WantedBy=multi-user.target
";

    /// 真实 golden ②：本机 `/etc/systemd/system/lastore-abort-auto-download.timer`。
    const TIMER: &str = "\
[Unit]
Description=Abort auto download every day

[Timer]
OnCalendar=*-*-* 20:00:00
Unit=lastore-abort-auto-download.service

[Install]
WantedBy=timers.target
";

    /// 真实 golden ③：本机 `/usr/lib/systemd/system/dev-hugepages.mount` 全文
    /// （含 LGPL 注释头与三个 Condition，2026-10-14 `repr()` 逐字抄录）。
    const MOUNT: &str = "\
#  SPDX-License-Identifier: LGPL-2.1-or-later
#
#  This file is part of systemd.
#
#  systemd is free software; you can redistribute it and/or modify it
#  under the terms of the GNU Lesser General Public License as published by
#  the Free Software Foundation; either version 2.1 of the License, or
#  (at your option) any later version.

[Unit]
Description=Huge Pages File System
Documentation=https://docs.kernel.org/admin-guide/mm/hugetlbpage.html
Documentation=https://www.freedesktop.org/wiki/Software/systemd/APIFileSystems
DefaultDependencies=no
Before=sysinit.target
ConditionPathExists=/sys/kernel/mm/hugepages
ConditionCapability=CAP_SYS_ADMIN
ConditionVirtualization=!private-users

[Mount]
What=hugetlbfs
Where=/dev/hugepages
Type=hugetlbfs
Options=nosuid,nodev
";

    // ── 11.1：文档模型与保真往返 ──────────────────────────────────────

    /// ① 真实 cron.service：parse → render **逐字节一致**，再 parse 语义等值。
    #[test]
    fn cron_golden_roundtrip_byte_identical() {
        let doc = parse_unit_text(CRON);
        assert_eq!(render_unit_text(&doc), CRON, "往返必须逐字节一致");
        let again = parse_unit_text(&render_unit_text(&doc));
        assert_eq!(again, doc, "再解析语义等值");
    }

    /// ①b 真实 cron.service：关键值解析正确（含 `EnvironmentFile=-` 前导 `-`）。
    #[test]
    fn cron_golden_values_parsed() {
        let doc = parse_unit_text(CRON);
        assert_eq!(doc.section_names(), vec!["Unit", "Service", "Install"]);
        assert_eq!(
            doc.get_value("Unit", "Description").as_deref(),
            Some("Regular background program processing daemon")
        );
        assert_eq!(
            doc.get_value("Service", "ExecStart").as_deref(),
            Some("/usr/sbin/cron -f $EXTRA_OPTS")
        );
        assert_eq!(
            doc.get_value("Service", "EnvironmentFile").as_deref(),
            Some("-/etc/default/cron")
        );
        assert_eq!(
            doc.get_value("Install", "WantedBy").as_deref(),
            Some("multi-user.target")
        );
        // 未收录键（IgnoreSIGPIPE）也必须读得到（查得到，即使表单不给它）
        assert_eq!(
            doc.get_value("Service", "IgnoreSIGPIPE").as_deref(),
            Some("false")
        );
    }

    /// ② 另两份真实多类型 golden（timer / mount）往返逐字节。
    #[test]
    fn timer_and_mount_goldens_roundtrip() {
        for g in [TIMER, MOUNT] {
            let doc = parse_unit_text(g);
            assert_eq!(render_unit_text(&doc), g);
        }
        let t = parse_unit_text(TIMER);
        assert_eq!(t.section_names(), vec!["Unit", "Timer", "Install"]);
        assert_eq!(
            t.get_value("Timer", "OnCalendar").as_deref(),
            Some("*-*-* 20:00:00")
        );
        let m = parse_unit_text(MOUNT);
        assert_eq!(m.section_names(), vec!["Unit", "Mount"]);
        assert_eq!(m.get_value("Mount", "What").as_deref(), Some("hugetlbfs"));
        assert_eq!(
            m.get_value("Mount", "Where").as_deref(),
            Some("/dev/hugepages")
        );
    }

    /// ③ `systemctl cat` 合并输出必须能识别（拒绝当编辑源）；原始文件不误判。
    #[test]
    fn cat_output_detected_raw_file_not() {
        let cat = "# /usr/lib/systemd/system/cron.service\n[Unit]\nDescription=x\n";
        assert!(looks_like_systemctl_cat(cat));
        let dropin =
            "# /etc/systemd/system/foo.service.d/bar.conf\n[Service]\nExecStart=/bin/true\n";
        assert!(looks_like_systemctl_cat(dropin));
        assert!(!looks_like_systemctl_cat(CRON), "原始文件没有头行");
        assert!(
            !looks_like_systemctl_cat("# 普通注释\n[Unit]\n"),
            "普通注释不误判"
        );
    }

    /// ④ 空文本 / 只有注释 / 节外行：不 panic 且保真。
    #[test]
    fn parse_empty_and_comment_only() {
        assert_eq!(parse_unit_text(""), UnitDoc::new());
        assert_eq!(render_unit_text(&parse_unit_text("")), "");
        let c = "# 只有注释\n\n# 第二段\n";
        let doc = parse_unit_text(c);
        assert_eq!(render_unit_text(&doc), c);
        assert_eq!(doc.entries().count(), 0);
    }

    /// ④b 节外行（第一个节头之前的任何东西）按 Raw 保留。
    #[test]
    fn parse_lines_before_first_section_are_raw() {
        let t = "#!/bin/sh\nSomeGarbageLine\n[Unit]\nDescription=x\n";
        let doc = parse_unit_text(t);
        assert_eq!(render_unit_text(&doc), t);
        assert_eq!(doc.entries().count(), 1);
    }

    /// ④c 裸键无 `=` → Raw 保留，不 panic（`Description` 不构成键）。
    #[test]
    fn parse_bare_key_without_equals_kept_raw() {
        let t = "[Unit]\nDescription\n# c\nEndsWithoutEquals\n";
        let doc = parse_unit_text(t);
        assert_eq!(render_unit_text(&doc), t);
        assert_eq!(doc.entries().count(), 0, "没有带 = 的行就没有键");
        assert!(!doc.has_key("Unit", "Description"), "裸行不算键");
        assert_eq!(doc.lines.len(), 4, "四行原样保留");
    }

    /// ④d 键前有空格：按 trim 后的键解析，raw 保真。
    #[test]
    fn parse_key_with_leading_space_kept_as_entry() {
        let t = "[Unit]\n    Description=带前导空白\n";
        let doc = parse_unit_text(t);
        assert_eq!(render_unit_text(&doc), t, "raw 原样输出");
        assert_eq!(
            doc.get_value("Unit", "Description").as_deref(),
            Some("带前导空白")
        );
    }

    /// ④e CRLF：raw 带 `\r`，往返不丢；值里剥掉 `\r`。
    #[test]
    fn parse_crlf_preserved() {
        let t = "[Unit]\r\nDescription=x\r\n";
        let doc = parse_unit_text(t);
        assert_eq!(render_unit_text(&doc), t, "CRLF 往返保真");
        assert_eq!(doc.get_value("Unit", "Description").as_deref(), Some("x"));
    }

    /// ④f 尾部换行有无两种形态分清。
    #[test]
    fn trailing_newline_flag_both_ways() {
        let with = parse_unit_text("[Unit]\nDescription=x\n");
        assert!(with.trailing_newline);
        assert_eq!(render_unit_text(&with), "[Unit]\nDescription=x\n");
        let without = parse_unit_text("[Unit]\nDescription=x");
        assert!(!without.trailing_newline);
        assert_eq!(render_unit_text(&without), "[Unit]\nDescription=x");
    }

    /// ⑤ 同键多值：顺序与内容都保留（Environment 两行）。
    #[test]
    fn multi_value_keys_order_preserved() {
        let t = "[Service]\nEnvironment=A=1\nEnvironment=B=2\nEnvironment=C=3\n";
        let doc = parse_unit_text(t);
        assert_eq!(
            doc.get_values("Service", "Environment"),
            vec!["A=1", "B=2", "C=3"]
        );
        assert_eq!(render_unit_text(&doc), t, "多值行原样往返");
    }

    /// ⑥ type_of：11 类全识别。
    #[test]
    fn type_of_accepts_all_11_types() {
        for t in UNIT_TYPES {
            let name = format!("x.{t}");
            assert_eq!(type_of(&name), Some(*t), "应识别 {name}");
        }
        assert_eq!(type_of("mnt-dev-hugepages.mount"), Some("mount"));
        assert_eq!(type_of("dev-sda2.swap"), Some("swap"));
    }

    /// ⑥b type_of：非法名 / 未知后缀 → None。
    #[test]
    fn type_of_rejects_bad_names() {
        for bad in [
            "",
            "sshd",
            ".service",
            "x.conf",
            "x.SERVICE",
            "x.service.service.bak",
            "/etc/systemd/system/x.service", // 只认名字，不认路径
        ] {
            assert_eq!(type_of(bad), None, "{bad:?} 应当被拒");
        }
    }

    /// ⑥c 模板名 `foo@.service` 也能识别类型（GOAL 摸底 7）。
    #[test]
    fn type_of_template_unit() {
        assert_eq!(type_of("getty@.service"), Some("service"));
        assert_eq!(type_of("app-x@autostart.service"), Some("service"));
        assert_eq!(type_of("getty@.timer"), Some("timer"));
    }

    /// ①b 续行：下一行强制 Raw、上一行标 continued（表单据此只读）。
    #[test]
    fn continuation_line_kept_raw_and_flagged() {
        let t = "[Service]\nExecStart=/bin/sh -c 'foo' \\\n        bar\nRestart=no\n";
        let doc = parse_unit_text(t);
        assert_eq!(render_unit_text(&doc), t, "续行往返逐字节一致");
        assert!(doc.key_is_continued("Service", "ExecStart"));
        assert!(!doc.key_is_continued("Service", "Restart"));
        // 续行的第二行绝不能被当成键（哪怕它含 =）
        assert!(doc.get_values("Service", "bar").is_empty());
        assert_eq!(
            doc.get_value("Service", "ExecStart").as_deref(),
            Some("/bin/sh -c 'foo' \\")
        );
    }

    /// 改一行只影响那一行，其它行仍逐字节保真。
    #[test]
    fn edit_single_line_keeps_others_byte_identical() {
        let mut doc = parse_unit_text(CRON);
        assert!(doc.set_values("Service", "Restart", &["always".into()]));
        let out = render_unit_text(&doc);
        let before: Vec<&str> = CRON.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        assert_eq!(before.len(), after.len(), "行数不变");
        let diff: Vec<usize> = before
            .iter()
            .enumerate()
            .filter(|(i, b)| after[*i] != **b)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(diff.len(), 1, "只有一行变化：{diff:?}");
        assert_eq!(after[diff[0]], "Restart=always");
        // 改过的行 raw 失效，其它行 raw 仍在
        assert_eq!(
            doc.entries().find(|e| e.key == "Restart").unwrap().raw,
            None
        );
        assert!(
            doc.entries()
                .find(|e| e.key == "KillMode")
                .unwrap()
                .raw
                .is_some()
        );
    }

    /// 节头原文（大小写/空白）保留。
    #[test]
    fn section_headers_preserved() {
        let t = "[Unit]\n[Service]\n[INSTALL]\n  [Slice]  \n";
        let doc = parse_unit_text(t);
        assert_eq!(render_unit_text(&doc), t);
        assert_eq!(
            doc.section_names(),
            vec!["Unit", "Service", "INSTALL", "Slice"]
        );
    }

    // ── 11.2：键点表与表单纯逻辑 ──────────────────────────────────────

    /// 键点表质量：无重复 (节,键)、数量下限、标签/说明非空、Select 必须有取值。
    #[test]
    fn key_spec_counts_and_uniqueness() {
        assert!(KEY_SPECS.len() >= 60, "键点过少：{}", KEY_SPECS.len());
        let mut seen = Vec::new();
        for s in KEY_SPECS {
            assert!(!s.label.is_empty(), "{} 缺中文标签", s.key);
            assert!(!s.hint.is_empty(), "{} 缺说明", s.key);
            assert!(!s.section.is_empty());
            let id = (s.section, s.key);
            assert!(!seen.contains(&id), "重复键点：{id:?}");
            seen.push(id);
            if s.control == Control::Select {
                assert!(!s.options.is_empty(), "{} 是下拉却没有取值", s.key);
            }
        }
    }

    /// 键点表覆盖摸底 5 的高频必收键（缺一个就红）。
    #[test]
    fn key_spec_covers_high_frequency_keys() {
        for (sec, key) in [
            ("Unit", "Description"),
            ("Unit", "Documentation"),
            ("Unit", "After"),
            ("Unit", "Before"),
            ("Unit", "Wants"),
            ("Unit", "Requires"),
            ("Service", "ExecStart"),
            ("Service", "Type"),
            ("Service", "User"),
            ("Service", "Environment"),
            ("Service", "Restart"),
            ("Service", "RestartSec"),
            ("Service", "TimeoutSec"),
            ("Service", "StandardOutput"),
            ("Service", "ExecReload"),
            ("Service", "ExecStop"),
            ("Service", "KillMode"),
            ("Service", "RemainAfterExit"),
            ("Timer", "OnCalendar"),
            ("Socket", "ListenStream"),
            ("Mount", "What"),
            ("Mount", "Where"),
            ("Automount", "Where"),
            ("Path", "PathExists"),
            ("Swap", "What"),
            ("Install", "WantedBy"),
        ] {
            assert!(spec_for(sec, key).is_some(), "缺少键点 [{sec}] {key}");
        }
    }

    /// 11 类键集矩阵（GOAL 11.2）：每类的专用节正确、非本类键不出现。
    #[test]
    fn key_spec_per_type_sections_matrix() {
        let expect: &[(&str, &[&str])] = &[
            ("service", &["Unit", "Service", "Install"]),
            ("timer", &["Unit", "Timer", "Install"]),
            ("socket", &["Unit", "Socket", "Install"]),
            ("mount", &["Unit", "Mount", "Install"]),
            ("automount", &["Unit", "Automount", "Install"]),
            ("path", &["Unit", "Path", "Install"]),
            ("swap", &["Unit", "Swap", "Install"]),
            ("slice", &["Unit", "Slice", "Install"]),
            // 无专用节的三类
            ("target", &["Unit", "Install"]),
            ("device", &["Unit", "Install"]),
            ("scope", &["Unit", "Install"]),
        ];
        for (t, secs) in expect {
            assert_eq!(
                &sections_for_type(t),
                secs,
                "{t} 的节序列不对（摸底 13 实测模板）"
            );
        }
        assert_eq!(expect.len(), 11, "必须覆盖 11 类");
    }

    /// 专用键不跨类：service 没有 OnCalendar、timer 没有 ExecStart、target 没有专用节。
    #[test]
    fn key_spec_no_cross_type_leakage() {
        let timer_keys: Vec<&str> = specs_for_type("timer").iter().map(|s| s.key).collect();
        assert!(!timer_keys.contains(&"ExecStart"));
        assert!(!timer_keys.contains(&"Type"));
        let svc_keys: Vec<&str> = specs_for_type("service").iter().map(|s| s.key).collect();
        assert!(!svc_keys.contains(&"OnCalendar"));
        assert!(!svc_keys.contains(&"ListenStream"));
        assert!(svc_keys.contains(&"ExecStart"));
        // target 只有 Unit + Install 的键
        let target_keys: Vec<&str> = specs_for_type("target").iter().map(|s| s.key).collect();
        assert!(!target_keys.contains(&"ExecStart"));
        assert!(target_keys.contains(&"Description"));
        assert!(target_keys.contains(&"WantedBy"));
    }

    /// 白名单真值表：Type 8 值（man 实测）。
    #[test]
    fn select_type_values_truth_table() {
        assert_eq!(SERVICE_TYPES.len(), 8, "man systemd.service 255 实测 8 值");
        let spec = spec_for("Service", "Type").unwrap();
        assert_eq!(spec.options, SERVICE_TYPES);
        for ok in SERVICE_TYPES {
            // 带 ExecStart，避免被「非 oneshot 必填 ExecStart」规则干扰本表
            let d = doc_with2("[Service]", &format!("Type={ok}"), "ExecStart=/bin/true");
            assert!(validate_form(&d, "service", None).is_ok(), "{ok} 应放行");
        }
        for bad in ["Simple", "SIMPLE", "fork", "notify_relay"] {
            let d = doc_with2("[Service]", &format!("Type={bad}"), "ExecStart=/bin/true");
            assert!(validate_form(&d, "service", None).is_err(), "{bad} 应被拒");
        }
        // 空值 = 键不存在（Type 无必填），单独验
        let d = parse_unit_text("[Service]\nExecStart=/bin/true\nType=\n");
        assert!(validate_form(&d, "service", None).is_ok());
    }

    /// 白名单真值表：Restart 7 值（man 实测）。
    #[test]
    fn select_restart_values_truth_table() {
        assert_eq!(RESTART_VALUES.len(), 7, "man systemd.service 255 实测 7 值");
        let spec = spec_for("Service", "Restart").unwrap();
        for ok in RESTART_VALUES {
            assert!(validate_single(spec, ok).is_ok(), "{ok} 应放行");
        }
        for bad in ["on_failure", "ON-FAILURE", "yes", "if-failed", "retry"] {
            assert!(validate_single(spec, bad).is_err(), "{bad} 应被拒");
        }
    }

    /// 白名单真值表：KillMode 4 值（man systemd.kill 实测）。
    #[test]
    fn select_killmode_values_truth_table() {
        assert_eq!(KILLMODE_VALUES.len(), 4);
        let spec = spec_for("Service", "KillMode").unwrap();
        for ok in KILLMODE_VALUES {
            assert!(validate_single(spec, ok).is_ok());
        }
        for bad in ["control_group", "all", "group", "Process"] {
            assert!(validate_single(spec, bad).is_err(), "{bad} 应被拒");
        }
    }

    /// 白名单真值表：StandardOutput/Error —— 8 基础值 + 4 参数化前缀。
    #[test]
    fn standard_output_select_and_prefix_forms() {
        assert_eq!(STD_STREAM_VALUES.len(), 8);
        let spec = spec_for("Service", "StandardOutput").unwrap();
        for ok in STD_STREAM_VALUES {
            assert!(validate_single(spec, ok).is_ok(), "{ok} 应放行");
        }
        for ok in [
            "file:/var/log/x.log",
            "append:/tmp/a",
            "truncate:/tmp/b",
            "fd:main",
        ] {
            assert!(validate_single(spec, ok).is_ok(), "{ok} 应放行");
        }
        for bad in ["file:", "fd:", "journal2", "LOG", ""] {
            if bad.is_empty() {
                assert!(validate_single(spec, bad).is_ok());
                continue;
            }
            assert!(validate_single(spec, bad).is_err(), "{bad} 应被拒");
        }
        // StandardError 同规则
        let err_spec = spec_for("Service", "StandardError").unwrap();
        assert!(validate_single(err_spec, "file:/tmp/e.log").is_ok());
        assert!(validate_single(err_spec, "nope").is_err());
    }

    /// WantedBy / RequiredBy 必须 .target 结尾。
    #[test]
    fn wantedby_requires_target_suffix() {
        let spec = spec_for("Install", "WantedBy").unwrap();
        for ok in ["multi-user.target", "timers.target", "graphical.target"] {
            assert!(validate_single(spec, ok).is_ok(), "{ok} 应放行");
        }
        for bad in ["multi-user", "ssh.service", "targets", ".target"] {
            // ".target" 结尾为空名字仍以 .target 结尾 → 放行（verify 兜底）
            let r = validate_single(spec, bad);
            if bad == ".target" {
                assert!(r.is_ok());
            } else {
                assert!(r.is_err(), "{bad} 应被拒");
            }
        }
        let req = spec_for("Install", "RequiredBy").unwrap();
        assert!(validate_single(req, "basic.target").is_ok());
        assert!(validate_single(req, "nope").is_err());
    }

    /// Environment 单项格式真值表（KEY=VALUE / 引号整串）。
    #[test]
    fn environment_item_format_truth_table() {
        for ok in [
            "A=1",
            "_X=hello world",
            "PATH=/usr/bin:/bin",
            "EMPTY=",
            "\"FOO=bar BAZ=qux\"",
            "'SINGLE=1'",
            "a1=2",
        ] {
            assert!(env_item_ok(ok), "{ok:?} 应放行");
        }
        for bad in ["", "NOEQ", "=v", "1A=v", "-bad=x", "A B=1", "\"unbalanced"] {
            assert!(!env_item_ok(bad), "{bad:?} 应被拒");
        }
        let spec = spec_for("Service", "Environment").unwrap();
        assert!(validate_single(spec, "A=1").is_ok());
        assert!(validate_single(spec, "NOEQ").is_err());
    }

    /// 数字/时长键：非负 + 合法单位；负数与垃圾拒。
    #[test]
    fn number_time_span_non_negative() {
        for ok in [
            "0", "5", "1.5", "5s", "10min", "2h", "500ms", "1w", "infinity", " 3s ",
        ] {
            assert!(validate_time_span(ok).is_ok(), "{ok:?} 应放行");
        }
        for bad in ["-1", "-5s", "abc", "5 x", "5parsec", "s", ""] {
            assert!(validate_time_span(bad).is_err(), "{bad:?} 应被拒");
        }
        let spec = spec_for("Service", "RestartSec").unwrap();
        assert!(validate_single(spec, "5s").is_ok());
        assert!(validate_single(spec, "-3").is_err());
    }

    /// Nice 允许负数（-20..=19）——systemd 语义，与「数字键非负」的通用规则不同。
    #[test]
    fn nice_range_allows_negative() {
        let spec = spec_for("Service", "Nice").unwrap();
        for ok in ["-20", "-5", "0", "19"] {
            assert!(validate_single(spec, ok).is_ok(), "{ok} 应放行");
        }
        for bad in ["-21", "20", "5s", "abc", "1.5"] {
            assert!(validate_single(spec, bad).is_err(), "{bad} 应被拒");
        }
    }

    /// 必填矩阵：service 非 oneshot 缺 ExecStart 必拦；oneshot 放行；其它类型按各自必填。
    #[test]
    fn required_by_type_truth_table() {
        // simple 缺 ExecStart → 拦
        let d = parse_unit_text("[Service]\nType=simple\n");
        assert!(validate_form(&d, "service", None).is_err());
        // oneshot 缺 ExecStart → 放行（交 verify）
        let d = parse_unit_text("[Service]\nType=oneshot\n");
        assert!(validate_form(&d, "service", None).is_ok());
        // 默认类型（无 Type= 即 simple 语义）→ 拦
        let d = parse_unit_text("[Service]\nExecStop=/bin/true\n");
        assert!(validate_form(&d, "service", None).is_err());
        // 有 ExecStart → 过
        let d = parse_unit_text("[Service]\nExecStart=/bin/true\n");
        assert!(validate_form(&d, "service", None).is_ok());
        // timer / target 无 ExecStart 要求
        assert!(validate_form(&parse_unit_text(TIMER), "timer", None).is_ok());
        let d = parse_unit_text("[Unit]\nDescription=x\n");
        assert!(validate_form(&d, "target", None).is_ok());
    }

    /// mount 必填 What + Where（缺一即拦）。
    #[test]
    fn mount_requires_what_and_where() {
        let d = parse_unit_text("[Mount]\nWhat=/dev/sda1\n");
        assert!(validate_form(&d, "mount", None).is_err(), "缺 Where");
        let d = parse_unit_text("[Mount]\nWhere=/mnt/x\n");
        assert!(validate_form(&d, "mount", None).is_err(), "缺 What");
        let d = parse_unit_text("[Mount]\nWhat=/dev/sda1\nWhere=/mnt/x\n");
        assert!(validate_form(&d, "mount", None).is_ok());
    }

    /// automount 必填 Where。
    #[test]
    fn automount_requires_where() {
        let d = parse_unit_text("[Automount]\n");
        assert!(validate_form(&d, "automount", None).is_err());
        let d = parse_unit_text("[Automount]\nWhere=/mnt/x\n");
        assert!(validate_form(&d, "automount", None).is_ok());
    }

    /// swap 必填 What。
    #[test]
    fn swap_requires_what() {
        let d = parse_unit_text("[Swap]\n");
        assert!(validate_form(&d, "swap", None).is_err());
        let d = parse_unit_text("[Swap]\nWhat=/dev/sda2\n");
        assert!(validate_form(&d, "swap", None).is_ok());
    }

    /// 名值匹配：Where/What ↔ 单元名（GOAL 摸底 14 的同款规则，自家先拦）。
    #[test]
    fn where_and_what_must_match_unit_name() {
        let d = parse_unit_text("[Mount]\nWhat=/dev/sda1\nWhere=/mnt/data\n");
        assert!(validate_form(&d, "mount", Some("mnt-data.mount")).is_ok());
        assert!(
            validate_form(&d, "mount", Some("wrong-name.mount")).is_err(),
            "名字不匹配必须拦"
        );
        let d = parse_unit_text("[Swap]\nWhat=/dev/sda2\n");
        assert!(validate_form(&d, "swap", Some("dev-sda2.swap")).is_ok());
        assert!(validate_form(&d, "swap", Some("swap2.swap")).is_err());
        // automount 同规则
        let d = parse_unit_text("[Automount]\nWhere=/mnt/a\n");
        assert!(validate_form(&d, "automount", Some("mnt-a.automount")).is_ok());
        assert!(validate_form(&d, "automount", Some("other.automount")).is_err());
        // 路径 → 名字推导
        assert_eq!(unit_name_from_path("/mnt/foo", "mount"), "mnt-foo.mount");
        assert_eq!(unit_name_from_path("/dev/sda2", "swap"), "dev-sda2.swap");
        assert_eq!(unit_name_from_path("/a/b/c", "mount"), "a-b-c.mount");
    }

    /// 值含换行拒绝（文档与表单两个入口都要拦）。
    #[test]
    fn value_with_newline_rejected() {
        let form = FormValues::new().set_text("Unit", "Description", "第一行\n第二行");
        let doc = parse_unit_text(CRON);
        assert!(apply_form(&doc, &form, "service").is_err());
        assert!(ensure_no_newline("a\r").is_err());
        assert!(ensure_no_newline("a").is_ok());
    }

    /// 续行键禁止通过表单编辑（D11.4 保真红线）。
    #[test]
    fn continued_key_edit_rejected_by_apply_form() {
        let t = "[Service]\nExecStart=/bin/sh -c 'foo' \\\n        bar\n";
        let doc = parse_unit_text(t);
        let form = FormValues::new().set_text("Service", "ExecStart", "/bin/true");
        let err = apply_form(&doc, &form, "service").unwrap_err();
        assert!(err.contains("续行"), "错误要说明原因：{err}");
        // 文档未被改动
        assert_eq!(render_unit_text(&doc), t);
    }

    /// `Unset` = 没动 → 文档逐字节不变。
    #[test]
    fn apply_form_untouched_when_unset() {
        let doc = parse_unit_text(CRON);
        let form = FormValues::new()
            .unset("Service", "Restart")
            .unset("Unit", "Description");
        let out = apply_form(&doc, &form, "service").unwrap();
        assert_eq!(render_unit_text(&out), CRON);
    }

    /// 未知键 → Err（防任意键注入）。
    #[test]
    fn apply_form_rejects_unknown_key() {
        let doc = parse_unit_text(CRON);
        let form = FormValues::new().set_text("Service", "MaliciousKey", "x");
        assert!(apply_form(&doc, &form, "service").is_err());
        // 节名也不许乱来
        let form = FormValues::new().set_text("Evil", "ExecStart", "x");
        assert!(apply_form(&doc, &form, "service").is_err());
    }

    /// 键不适用于该类型 → Err（timer 单元不能写 ExecStart）。
    #[test]
    fn apply_form_rejects_key_not_for_type() {
        let doc = parse_unit_text(TIMER);
        let form = FormValues::new().set_text("Service", "ExecStart", "/bin/true");
        let err = apply_form(&doc, &form, "timer").unwrap_err();
        assert!(err.contains("不适用"), "{err}");
        let form = FormValues::new().set_text("Timer", "OnCalendar", "*-*-* 01:00:00");
        assert!(apply_form(&doc, &form, "timer").is_ok());
    }

    /// 未知单元类型 → Err。
    #[test]
    fn apply_form_rejects_unknown_unit_type() {
        let doc = parse_unit_text(CRON);
        let form = FormValues::new().set_text("Unit", "Description", "x");
        assert!(apply_form(&doc, &form, "socket.io").is_err());
        assert!(apply_form(&doc, &form, "").is_err());
    }

    /// 列表键整组替换（多 → 少保持首行位置；空 = 删除）。
    #[test]
    fn apply_form_list_replaces_group() {
        let t = "[Unit]\nDescription=x\nAfter=a.target b.target\nWants=w.target\n";
        let doc = parse_unit_text(t);
        let form =
            FormValues::new().set_list("Unit", "After", vec!["c.target".into(), "d.target".into()]);
        let out = apply_form(&doc, &form, "service").unwrap();
        assert_eq!(
            out.get_values("Unit", "After"),
            vec!["c.target", "d.target"]
        );
        assert_eq!(
            out.get_value("Unit", "Wants").as_deref(),
            Some("w.target"),
            "别的键不动"
        );
        assert!(render_unit_text(&out).contains("Description=x"));
        // 空列表 = 删除该键
        let form = FormValues::new().set_list("Unit", "After", vec![]);
        let out = apply_form(&doc, &form, "service").unwrap();
        assert!(!out.has_key("Unit", "After"));
        assert!(out.has_key("Unit", "Wants"));
    }

    /// 空文本 = 删除该键；非空 = 改值。
    #[test]
    fn apply_form_text_empty_removes_key() {
        let doc = parse_unit_text(CRON);
        let form = FormValues::new().set_text("Service", "KillMode", "");
        let out = apply_form(&doc, &form, "service").unwrap();
        assert!(!out.has_key("Service", "KillMode"));
        let form = FormValues::new().set_text("Service", "KillMode", "control-group");
        let out = apply_form(&doc, &form, "service").unwrap();
        assert_eq!(
            out.get_value("Service", "KillMode").as_deref(),
            Some("control-group")
        );
        assert!(render_unit_text(&out).contains("KillMode=control-group"));
    }

    /// 开关写 true/false。
    #[test]
    fn apply_form_toggle_writes_true_false() {
        let doc = parse_unit_text("[Unit]\nDescription=x\n");
        let form = FormValues::new().set_bool("Unit", "DefaultDependencies", false);
        let out = apply_form(&doc, &form, "service").unwrap();
        assert_eq!(
            out.get_value("Unit", "DefaultDependencies").as_deref(),
            Some("false")
        );
        let form = FormValues::new().set_bool("Unit", "DefaultDependencies", true);
        let out = apply_form(&doc, &form, "service").unwrap();
        assert_eq!(
            out.get_value("Unit", "DefaultDependencies").as_deref(),
            Some("true")
        );
    }

    /// 节不存在 → 在文档末尾建节；新键追加到该节末尾（下一个节头之前）。
    #[test]
    fn apply_form_creates_missing_section_at_end() {
        let doc = parse_unit_text("[Unit]\nDescription=x\n");
        assert!(!doc.has_section("Install"));
        let form =
            FormValues::new().set_list("Install", "WantedBy", vec!["multi-user.target".into()]);
        let out = apply_form(&doc, &form, "service").unwrap();
        assert!(out.has_section("Install"));
        let names = out.section_names();
        assert_eq!(names, vec!["Unit", "Install"], "新节在末尾");
        let rendered = render_unit_text(&out);
        assert!(rendered.ends_with("[Install]\nWantedBy=multi-user.target\n"));
        // 已有节时新键插在节末尾（下一个节头之前）
        let doc = parse_unit_text("[Unit]\nDescription=x\n\n[Install]\nWantedBy=a.target\n");
        let form = FormValues::new().set_text("Unit", "Before", "x.target");
        let out = apply_form(&doc, &form, "service").unwrap();
        let r = render_unit_text(&out);
        let unit_pos = r.find("[Unit]").unwrap();
        let install_pos = r.find("[Install]").unwrap();
        let before_pos = r.find("Before=x.target").unwrap();
        assert!(unit_pos < before_pos && before_pos < install_pos, "{r}");
        assert!(r.contains("WantedBy=a.target"), "Install 节内容不动");
    }

    /// 注释、空行、未知键、续行全部存活（D11.4 保真）。
    #[test]
    fn apply_form_preserves_comments_unknown_keys_and_continuations() {
        let t = "# 头注释\n[Unit]\nDescription=x\n# 段内注释\n\n[Service]\nExecStart=/bin/sh -c 'a' \\\n        b\nIgnoreSIGPIPE=false\n";
        let doc = parse_unit_text(t);
        let form = FormValues::new().set_text("Unit", "Description", "改过了");
        let out = apply_form(&doc, &form, "service").unwrap();
        let r = render_unit_text(&out);
        assert!(r.contains("# 头注释"));
        assert!(r.contains("# 段内注释"));
        assert!(r.contains("IgnoreSIGPIPE=false"), "未知键原样保留");
        assert!(r.contains("ExecStart=/bin/sh -c 'a' \\"), "续行原样保留");
        assert!(r.contains("        b"));
        assert!(r.contains("Description=改过了"));
        // 只有 Description 那一行变化
        let diff: Vec<&str> = t
            .lines()
            .zip(r.lines())
            .filter(|(a, b)| a != b)
            .map(|(_, b)| b)
            .collect();
        assert_eq!(diff, vec!["Description=改过了"]);
    }

    /// 表单跑完 → validate 通过（真实 cron 改两个键的整链）。
    #[test]
    fn apply_then_validate_realistic_edit() {
        let doc = parse_unit_text(CRON);
        let form = FormValues::new()
            .set_text("Service", "Restart", "always")
            .set_text("Service", "User", "daemon")
            .set_list("Unit", "After", vec!["network.target".into()]);
        let out = apply_form(&doc, &form, "service").unwrap();
        assert!(validate_form(&out, "service", Some("cron.service")).is_ok());
        assert_eq!(
            out.get_value("Service", "Restart").as_deref(),
            Some("always")
        );
        assert_eq!(out.get_value("Service", "User").as_deref(), Some("daemon"));
        assert_eq!(out.get_values("Unit", "After"), vec!["network.target"]);
        // 类型名不匹配（cron.service 是 service）→ 名值校验不涉及 service
        assert!(validate_form(&out, "service", Some("cron.timer")).is_ok());
    }

    /// 写入坏值后 validate 必须报错（表单与文档两条路径同源）。
    #[test]
    fn validate_rejects_bad_select_after_apply() {
        let doc = parse_unit_text(CRON);
        // 绕过表单值域（直接改文档），validate 仍要拦
        let mut out = doc.clone();
        out.set_values("Service", "Restart", &["sometimes".into()]);
        let err = validate_form(&out, "service", None).unwrap_err();
        assert!(err.contains("Restart"), "{err}");
        // WantedBy 非 target 同理
        let mut out = doc.clone();
        out.set_values("Install", "WantedBy", &["nope".into()]);
        assert!(validate_form(&out, "service", None).is_err());
        // Environment 非 KEY=VALUE
        let mut out = doc.clone();
        out.set_values("Service", "Environment", &["BROKEN".into()]);
        assert!(validate_form(&out, "service", None).is_err());
        // 布尔键写垃圾
        let mut out = doc.clone();
        out.set_values("Service", "RemainAfterExit", &["yes".into()]);
        assert!(validate_form(&out, "service", None).is_err());
    }

    /// 未知长尾键不参与值域校验（cron 的 IgnoreSIGPIPE 放行，交给 verify）。
    #[test]
    fn validate_ignores_unknown_keys() {
        let doc = parse_unit_text(CRON);
        assert!(validate_form(&doc, "service", Some("cron.service")).is_ok());
        // 未知键的值再离谱也不拦（必填项该有的还是要有，故补 ExecStart）
        let weird = "[Unit]\nDescription=x\nSomeVendorThing=not-a-known-value\n\n[Service]\nExecStart=/bin/true\n";
        let doc = parse_unit_text(weird);
        assert!(
            validate_form(&doc, "service", None).is_ok(),
            "未知键不参与值域校验"
        );
    }

    /// `[Install]` 不是必填（static 单元常态，GOAL 摸底 15）。
    #[test]
    fn install_section_optional() {
        let doc = parse_unit_text("[Service]\nExecStart=/bin/true\n");
        assert!(validate_form(&doc, "service", None).is_ok());
        assert!(!doc.has_section("Install"));
    }

    /// set_values 的组语义：多值键改少保首行位置、追加紧随、空组删除。
    #[test]
    fn set_values_group_semantics() {
        let mut doc = parse_unit_text("[Unit]\nDescription=x\nAfter=a b\nBefore=z.target\n");
        // 3 → 1
        doc.set_values("Unit", "After", &["only.target".to_string()]);
        assert_eq!(doc.get_values("Unit", "After"), vec!["only.target"]);
        // 1 → 3（首行位置不变，其余紧随）
        doc.set_values(
            "Unit",
            "After",
            &["p.target".into(), "q.target".into(), "r.target".into()],
        );
        assert_eq!(
            doc.get_values("Unit", "After"),
            vec!["p.target", "q.target", "r.target"]
        );
        let r = render_unit_text(&doc);
        let p = r.find("After=p.target").unwrap();
        let b = r.find("Before=z.target").unwrap();
        assert!(p < b, "首行位置保留在 Before 之前：\n{r}");
        // 空组删除
        assert!(doc.set_values("Unit", "After", &[]));
        assert!(!doc.has_key("Unit", "After"));
        // 空组 + 键不存在 = 无可删
        assert!(!doc.set_values("Unit", "NoSuchKey", &[]));
    }

    /// remove_key 只删目标键。
    #[test]
    fn remove_key_only_removes_target() {
        let mut doc = parse_unit_text(CRON);
        assert!(doc.remove_key("Service", "KillMode"));
        assert!(!doc.has_key("Service", "KillMode"));
        assert!(doc.has_key("Service", "Restart"));
        assert!(!doc.remove_key("Service", "KillMode"), "二次删除返回 false");
        assert_eq!(
            render_unit_text(&doc),
            CRON.replace("KillMode=process\n", "")
        );
    }

    /// canonical 行格式与 raw 优先级。
    #[test]
    fn canonical_line_and_raw_precedence() {
        let mut doc = parse_unit_text("[Unit]\nDescription=   保留原样   \n");
        // 未改：raw 原样
        assert_eq!(
            render_unit_text(&doc),
            "[Unit]\nDescription=   保留原样   \n"
        );
        // 改了：按 canonical 重新生成（原值两侧空白被规范化，raw 失效）
        doc.set_values("Unit", "Description", &["新值".into()]);
        assert_eq!(render_unit_text(&doc), "[Unit]\nDescription=新值\n");
        // 手工 push 一条无 raw 的条目
        doc.push_line(UnitLine::Entry(UnitEntry::new(
            "Unit", "Before", "x.target", None, false,
        )));
        assert!(render_unit_text(&doc).ends_with("Before=x.target\n"));
    }

    /// 类型清单常量（GOAL 11.8 断言基础）：11 类 / 新建 9 类、无重复、后缀合法。
    #[test]
    fn type_list_constants_sanity() {
        assert_eq!(UNIT_TYPES.len(), 11);
        assert_eq!(NEWABLE_TYPES.len(), 9);
        for t in NEWABLE_TYPES {
            assert!(UNIT_TYPES.contains(t), "{t} 必须同时在 11 类里");
        }
        for excluded in ["device", "scope"] {
            assert!(!NEWABLE_TYPES.contains(&excluded));
        }
        assert_eq!(
            UNIT_TYPES.len(),
            {
                let mut v = UNIT_TYPES.to_vec();
                v.sort();
                v.dedup();
                v.len()
            },
            "11 类不能有重复"
        );
        for t in UNIT_TYPES {
            assert!(t.chars().all(|c| c.is_ascii_lowercase()), "{t} 必须小写");
        }
    }

    /// 控件类型与常见键的对应关系（页面渲染依赖）。
    #[test]
    fn control_kinds_match_expected_keys() {
        let cases: &[(&str, &str, Control)] = &[
            ("Service", "Type", Control::Select),
            ("Service", "Restart", Control::Select),
            ("Service", "KillMode", Control::Select),
            ("Service", "StandardOutput", Control::Select),
            ("Service", "Environment", Control::List),
            ("Unit", "After", Control::List),
            ("Install", "WantedBy", Control::List),
            ("Service", "RestartSec", Control::Number),
            ("Service", "Nice", Control::Number),
            ("Service", "RemainAfterExit", Control::Toggle),
            ("Unit", "DefaultDependencies", Control::Toggle),
            ("Unit", "Description", Control::Text),
            ("Timer", "OnCalendar", Control::Text),
            ("Mount", "Where", Control::Text),
        ];
        for (sec, key, want) in cases {
            let spec = spec_for(sec, key).unwrap_or_else(|| panic!("缺 [{sec}] {key}"));
            assert_eq!(spec.control, *want, "[{sec}] {key} 控件类型不符");
        }
    }

    /// 辅助：拼一个最小文档（两行键值）。
    fn doc_with2(section: &str, line1: &str, line2: &str) -> UnitDoc {
        parse_unit_text(&format!("{section}\n{line1}\n{line2}\n"))
    }
}
