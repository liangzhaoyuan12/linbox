//! desktop 文件（`.desktop`）的**文档模型与表单纯逻辑**（GOAL Phase 12 的 12.1 / 12.2）。
//!
//! 职责：
//! - [`DesktopDoc`]：组序列 + 键值对保序可重复 + **locale 变体键**（`Name[zh_CN]`）
//!   + 注释/空行/原始行交错（逐字保真，D12.4-⑤）；
//! - [`parse_desktop_text`] / [`render_desktop_text`]：往返逐字节一致；
//! - [`desktop_kind`]：按目录判 Menu / Autostart（D12.2 五目录）；
//! - [`KEY_SPECS`] / [`apply_form`] / [`validate_form`]：结构化表单的键点表与
//!   校验闸门（D12.4 / D12.7-②）。
//!
//! 与 unitfile 模型的关键差异：desktop 规范**没有续行**（`\` 结尾只是普通字符），
//! 注释只认 `#`（`;` 是列表分隔符不是注释），键名可带 locale 后缀 `Key[locale]`。
//!
//! 约束（Phase 7/10/11 同款）：本文件禁止 `use gtk` / `use adw` / `use glib`。

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 类型与目录（D12.2 扫描范围 + 优先级）
// ---------------------------------------------------------------------------

/// desktop 文件的模块归属：应用菜单启动器 / 开机登录自启。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopKind {
    /// 应用菜单（`~/.local/share/applications` > `/usr/local/share/applications` > `/usr/share/applications`）。
    Menu,
    /// 开机自启（`~/.config/autostart` > `/etc/xdg/autostart`）。
    Autostart,
}

impl DesktopKind {
    /// 中文标签（页面标题用）。
    pub fn label(self) -> &'static str {
        match self {
            DesktopKind::Menu => "应用菜单",
            DesktopKind::Autostart => "开机自启",
        }
    }
}

/// `[Desktop Entry]` 组名（表单只改这个组，D12.10）。
pub const ENTRY_GROUP: &str = "Desktop Entry";

/// 菜单侧搜索目录（**XDG 优先级降序**，D12.2）。
pub fn menu_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".local/share/applications"));
    }
    v.push(PathBuf::from("/usr/local/share/applications"));
    v.push(PathBuf::from("/usr/share/applications"));
    v
}

/// 自启侧搜索目录（**XDG 优先级降序**，D12.2）。
pub fn autostart_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".config/autostart"));
    }
    v.push(PathBuf::from("/etc/xdg/autostart"));
    v
}

/// 按路径判模块归属（五目录判定，越界 → `None`）。
///
/// 目录本身与目录下的文件都算（`/usr/share/applications` 与
/// `/usr/share/applications/x.desktop` 同判）；带 `-backup` 这类**前缀不匹配**
/// （按 `/` 边界判定，`/usr/share/applications-old/x.desktop` → `None`）。
pub fn desktop_kind(path: &Path) -> Option<DesktopKind> {
    let s = path.to_string_lossy();
    for d in menu_dirs() {
        let ds = d.to_string_lossy().to_string();
        if s.as_ref() == ds || s.starts_with(&format!("{ds}/")) {
            return Some(DesktopKind::Menu);
        }
    }
    for d in autostart_dirs() {
        let ds = d.to_string_lossy().to_string();
        if s.as_ref() == ds || s.starts_with(&format!("{ds}/")) {
            return Some(DesktopKind::Autostart);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 文档模型（行交错保真，骨架照抄 model/unitfile.rs）
// ---------------------------------------------------------------------------

/// 一条键值行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopEntry {
    /// 所属组（不带方括号，如 `Desktop Entry` / `Desktop Action New`）。
    pub group: String,
    /// 完整键名（**可带 locale 后缀**，如 `Name[zh_CN]`）。
    pub key: String,
    /// 值（已剥 CRLF 的 `\r`；raw 仍带，未修改行往返不丢）。
    pub value: String,
    /// 原文行（未修改时逐字节输出；改值后清空按 `Key=Value` 生成）。
    pub raw: Option<String>,
}

impl DesktopEntry {
    pub fn new(group: &str, key: &str, value: &str, raw: Option<String>) -> Self {
        Self {
            group: group.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            raw,
        }
    }

    /// 键的 locale 部分：`Name[zh_CN]` → `Some("zh_CN")`；无后缀 → `None`。
    pub fn locale(&self) -> Option<&str> {
        let (base, rest) = self.key.split_once('[')?;
        let loc = rest.strip_suffix(']')?;
        if base.is_empty() || loc.is_empty() {
            return None;
        }
        Some(loc)
    }

    /// 去掉 locale 后缀的基础键名。
    pub fn base_key(&self) -> &str {
        match self.key.split_once('[') {
            Some((base, rest)) if rest.ends_with(']') && !base.is_empty() => base,
            _ => &self.key,
        }
    }

    fn canonical_line(&self) -> String {
        format!("{}={}", self.key, self.value)
    }

    /// 改值（清 raw → 此后按 canonical 输出）。
    fn set_value(&mut self, v: impl Into<String>) {
        self.value = v.into();
        self.raw = None;
    }
}

/// 文档里的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopLine {
    /// 注释 / 空行 / 组外行 / 无 `=` 的行，原样保留。
    Raw(String),
    /// 组头（如 `[Desktop Entry]`）；`raw` 保留原文。
    Group { raw: String, name: String },
    /// 一条键值行。
    Entry(DesktopEntry),
}

impl DesktopLine {
    /// 序列化为一行文本（不含换行）。
    pub fn to_line_string(&self) -> String {
        match self {
            DesktopLine::Raw(s) => s.clone(),
            DesktopLine::Group { raw, .. } => raw.clone(),
            DesktopLine::Entry(e) => e.raw.clone().unwrap_or_else(|| e.canonical_line()),
        }
    }
}

/// 整个 desktop 文件文档。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopDoc {
    /// 按文件顺序交错的行。
    pub lines: Vec<DesktopLine>,
    /// 原文件是否以换行结尾。
    pub trailing_newline: bool,
    /// 原文件是否带 UTF-8 BOM（渲染时原样加回）。
    pub bom: bool,
}

impl DesktopDoc {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只看键值行，保序。
    pub fn entries(&self) -> impl Iterator<Item = &DesktopEntry> {
        self.lines.iter().filter_map(|l| match l {
            DesktopLine::Entry(e) => Some(e),
            _ => None,
        })
    }

    /// 某组的键值行（保序）。
    pub fn entries_in_group<'a>(
        &'a self,
        group: &'a str,
    ) -> impl Iterator<Item = &'a DesktopEntry> {
        self.entries().filter(move |e| e.group == group)
    }

    /// 按文件顺序去重列出组名。
    pub fn group_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for l in &self.lines {
            if let DesktopLine::Group { name, .. } = l
                && !out.iter().any(|s| s == name)
            {
                out.push(name.clone());
            }
        }
        out
    }

    /// 取某键（指定组）的第一个值。
    pub fn get_value(&self, group: &str, key: &str) -> Option<String> {
        self.entries()
            .find(|e| e.group == group && e.key == key)
            .map(|e| e.value.clone())
    }

    /// 取 `[Desktop Entry]` 组某键的第一个值（表单/校验常用捷径）。
    pub fn get(&self, key: &str) -> Option<String> {
        self.get_value(ENTRY_GROUP, key)
    }

    /// 取某键的全部值（保序；desktop 规范不鼓励重复键但现实存在）。
    pub fn get_values(&self, group: &str, key: &str) -> Vec<String> {
        self.entries()
            .filter(|e| e.group == group && e.key == key)
            .map(|e| e.value.clone())
            .collect()
    }

    /// 该键是否存在（指定组）。
    pub fn has_key(&self, group: &str, key: &str) -> bool {
        self.entries().any(|e| e.group == group && e.key == key)
    }

    /// 写值：已有键改第一个出现（清 raw 走 canonical），没有则插到该组末尾。
    ///
    /// 只允许写 `[Desktop Entry]` 组（D12.10：Action 组只读保真）。
    pub fn set_value(&mut self, group: &str, key: &str, value: &str) -> bool {
        if group != ENTRY_GROUP {
            return false;
        }
        if let Some(e) = self.lines.iter_mut().find_map(|l| match l {
            DesktopLine::Entry(e) if e.group == group && e.key == key => Some(e),
            _ => None,
        }) {
            e.set_value(value);
            return true;
        }
        // 组不存在 → 先补组头；组存在 → 插到该组最后一条键值行之后
        let insert_at = self
            .lines
            .iter()
            .rposition(|l| matches!(l, DesktopLine::Group { name, .. } if name == group))
            .or_else(|| {
                self.lines
                    .iter()
                    .rposition(|l| matches!(l, DesktopLine::Group { .. }))
            });
        let entry = DesktopEntry::new(group, key, value, None);
        match insert_at {
            Some(idx) => {
                // 找该组之后的第一条非键值行位置（组末尾）
                let mut at = idx + 1;
                while at < self.lines.len()
                    && matches!(&self.lines[at], DesktopLine::Entry(e) if e.group == group)
                {
                    at += 1;
                }
                self.lines.insert(at, DesktopLine::Entry(entry));
            }
            None => {
                // 无任何组头（坏文件）：开头补组头再插入
                self.lines.insert(
                    0,
                    DesktopLine::Group {
                        raw: format!("[{group}]"),
                        name: group.to_string(),
                    },
                );
                self.lines.insert(1, DesktopLine::Entry(entry));
            }
        }
        true
    }

    /// 删键（指定 `[Desktop Entry]` 组；表单 v1 只在开关语义里用，如 Hidden=false → 删行）。
    pub fn remove_key(&mut self, group: &str, key: &str) -> bool {
        let before = self.lines.len();
        self.lines
            .retain(|l| !matches!(l, DesktopLine::Entry(e) if e.group == group && e.key == key));
        self.lines.len() != before
    }

    /// `Type=` 值（`[Desktop Entry]` 组）。
    pub fn entry_type(&self) -> Option<&str> {
        self.entries()
            .find(|e| e.group == ENTRY_GROUP && e.key == "Type")
            .map(|e| e.value.as_str())
    }
}

/// 备份路径：`<target>.linbox.bak`（与 `fstab::backup_path` / `unitfile::backup_path` 同名同规则）。
pub fn backup_path_of(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".linbox.bak");
    PathBuf::from(s)
}

/// BOM 前缀常量。
const BOM: &str = "\u{feff}";

/// 解析 desktop 文本 → 文档。
///
/// 分类规则（desktop 规范 + 保真优先）：
/// - 首字符 BOM → 剥掉记 `bom=true`（渲染加回）；
/// - `[…]`（trim 后）→ 组头；
/// - 位于组内、非 `#` 注释、含 `=` → 键值行（键 = 第一个 `=` 前并 trim，
///   可带 locale 后缀；值剥行尾 `\r`，raw 保留）；
/// - 其余（注释、空行、组外行、无 `=` 的裸键）→ `Raw` 原样保留。
///
/// **无续行语义**（desktop 规范没有续行；`\` 结尾只是普通字符值）。
pub fn parse_desktop_text(text: &str) -> DesktopDoc {
    let mut doc = DesktopDoc::new();
    if text.is_empty() {
        return doc;
    }
    let text = if let Some(rest) = text.strip_prefix(BOM) {
        doc.bom = true;
        rest
    } else {
        text
    };
    doc.trailing_newline = text.ends_with('\n');
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut group: Option<String> = None;

    for raw in body.split('\n') {
        let t = raw.trim();
        if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
            let name = t[1..t.len() - 1].to_string();
            doc.lines.push(DesktopLine::Group {
                raw: raw.to_string(),
                name,
            });
            group = Some(t[1..t.len() - 1].to_string());
            continue;
        }
        // 注释只认 #（; 是列表分隔符，不是注释）
        let is_comment = t.starts_with('#');
        if let Some(g) = &group
            && !is_comment
            && !t.is_empty()
            && let Some(eq) = t.find('=')
        {
            let key = t[..eq].trim().to_string();
            let mut value = t[eq + 1..].to_string();
            if value.ends_with('\r') {
                value.pop();
            }
            if !key.is_empty() {
                doc.lines.push(DesktopLine::Entry(DesktopEntry::new(
                    g,
                    &key,
                    &value,
                    Some(raw.to_string()),
                )));
                continue;
            }
        }
        doc.lines.push(DesktopLine::Raw(raw.to_string()));
    }
    doc
}

/// 序列化 → 文本（未修改行逐字节保真，修改行按 `Key=Value` 生成；BOM/尾换行还原）。
pub fn render_desktop_text(doc: &DesktopDoc) -> String {
    let mut out = String::new();
    if doc.bom {
        out.push_str(BOM);
    }
    if doc.lines.is_empty() {
        return out;
    }
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
// 键点表（KEY_SPEC，D12.4 按 Kind 分流）
// ---------------------------------------------------------------------------

/// 表单控件类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// 单行文本。
    Text,
    /// 下拉（值域见 [`KeySpec::options`]；现实值可不在清单，页面动态补入）。
    Select,
    /// 开关（true/false）。
    Toggle,
    /// 分号列表（整组替换；值渲染带尾分号）。
    SList,
    /// 本地化输入（`Key[zh_CN]`，留空 = 不动）。
    Localized,
}

/// 一个可编辑键点的完整定义。
#[derive(Clone, Copy, Debug)]
pub struct KeySpec {
    /// 表单分组（D12.4 的①-④，中文标签）。
    pub form_group: &'static str,
    /// 键名（可带 locale 后缀）。
    pub key: &'static str,
    /// 中文标签。
    pub label: &'static str,
    /// 一句话说明。
    pub hint: &'static str,
    /// 控件类型。
    pub control: Control,
    /// `Select` 的常用取值（其余为空切片；现实值不在清单由页面补入）。
    pub options: &'static [&'static str],
    /// 是否必填（`Name` / `Exec`——仅 `Type=Application` 时由校验强制）。
    pub required: bool,
    /// 适用模块（`[Menu, Autostart]` = 两页都出）。
    pub kinds: &'static [DesktopKind],
}

/// `Type=` 四值（D12.7-②）。
pub const TYPE_OPTIONS: &[&str] = &["Application", "Link", "Directory", "Service"];

/// 桌面环境名常用清单（OnlyShowIn / NotShowIn 下拉）。
pub const DESKTOP_ENVIRONMENTS: &[&str] = &[
    "GNOME",
    "KDE",
    "Deepin",
    "XFCE",
    "LXQt",
    "Unity",
    "MATE",
    "Cinnamon",
    "Pantheon",
    "Enlightenment",
    "Budgie",
];

const KIND_BOTH: &[DesktopKind] = &[DesktopKind::Menu, DesktopKind::Autostart];
const KIND_AUTOSTART_ONLY: &[DesktopKind] = &[DesktopKind::Autostart];

/// 全部键点（顺序 = 表单渲染顺序）。
pub const KEY_SPECS: &[KeySpec] = &[
    // ① 身份与描述
    KeySpec {
        form_group: "身份与描述",
        key: "Name",
        label: "名称",
        hint: "菜单/自启列表里显示的应用名（Application 必填）",
        control: Control::Text,
        options: &[],
        required: true,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "身份与描述",
        key: "GenericName",
        label: "通用名",
        hint: "比 Name 更笼统的称呼，如「终端」",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "身份与描述",
        key: "Comment",
        label: "注释",
        hint: "一句话描述，鼠标悬停提示里显示",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "身份与描述",
        key: "Name[zh_CN]",
        label: "名称（简体中文）",
        hint: "留空 = 不写不删，保留文件里已有的中文名",
        control: Control::Localized,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "身份与描述",
        key: "Comment[zh_CN]",
        label: "注释（简体中文）",
        hint: "留空 = 不写不删，保留文件里已有的中文注释",
        control: Control::Localized,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    // ② 执行
    KeySpec {
        form_group: "执行",
        key: "Type",
        label: "类型",
        hint: "Application=应用 / Link=链接 / Directory=目录 / Service=DBus 服务",
        control: Control::Select,
        options: TYPE_OPTIONS,
        required: true,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "Exec",
        label: "启动命令",
        hint: "Application 必填；字段码只允许 %f %F %u %U %d %D %n %N %i %c %k %v %m %%（摸底 6 的 %o/%z 非法）",
        control: Control::Text,
        options: &[],
        required: true,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "TryExec",
        label: "存在性检查",
        hint: "该可执行文件存在才显示/自启，可留空",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "Path",
        label: "工作目录",
        hint: "启动时的当前目录，可留空",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "Terminal",
        label: "在终端中运行",
        hint: "true = 用终端模拟器启动（终端类应用用）",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "StartupNotify",
        label: "启动进度反馈",
        hint: "true = 桌面显示启动中光标直到应用首窗出现",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "StartupWMClass",
        label: "窗口类匹配",
        hint: "让启动器图标与运行中窗口正确合并（X11 下常用）",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "Icon",
        label: "图标",
        hint: "主题图标名（如 org.gnome.Terminal）或绝对路径",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "执行",
        key: "Version",
        label: "桌面文件规范版本",
        hint: "一般留空；填 1.0 表示遵循 Desktop Entry 规范 1.0",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    // ③ 分类与菜单
    KeySpec {
        form_group: "分类与菜单",
        key: "Categories",
        label: "分类",
        hint: "分号分隔；决定菜单里的归类（如 AudioVideo;Player;）",
        control: Control::SList,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "分类与菜单",
        key: "MimeType",
        label: "关联类型",
        hint: "分号分隔的 MIME 类型（如 text/plain;），决定「打开方式」里出现",
        control: Control::SList,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "分类与菜单",
        key: "Keywords",
        label: "搜索关键词",
        hint: "分号分隔；菜单搜索时额外匹配的词",
        control: Control::SList,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "分类与菜单",
        key: "OnlyShowIn",
        label: "仅在这些桌面显示",
        hint: "分号分隔的桌面环境名（如 GNOME;Deepin;），留空 = 全部",
        control: Control::Select,
        options: DESKTOP_ENVIRONMENTS,
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "分类与菜单",
        key: "NotShowIn",
        label: "不在这些桌面显示",
        hint: "分号分隔的桌面环境名，与 OnlyShowIn 互斥使用",
        control: Control::Select,
        options: DESKTOP_ENVIRONMENTS,
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "分类与菜单",
        key: "NoDisplay",
        label: "不在菜单显示",
        hint: "true = 不出现在应用菜单（但仍有关联/自启能力）；本机 49% 菜单文件带它",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    KeySpec {
        form_group: "分类与菜单",
        key: "Hidden",
        label: "已禁用",
        hint: "true = 彻底隐藏/停用（本机停用惯例，D12.3；自启页停用开关写的就是它）",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_BOTH,
    },
    // ④ 自启专有（仅自启页）
    KeySpec {
        form_group: "自启专有",
        key: "X-GNOME-Autostart-enabled",
        label: "GNOME 启用开关",
        hint: "GNOME 系读它判启停；有它时停用开关同步写 true/false（本机仅 1/20 带）",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-GNOME-Autostart-Delay",
        label: "延迟秒数",
        hint: "登录后延迟 N 秒再启动（本机 0/20 带它）",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-GNOME-Autostart-Phase",
        label: "启动阶段",
        hint: "GNOME 启动顺序阶段（如 Initialization / WindowManager / Desktop）",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-GNOME-AutoRestart",
        label: "崩溃自动重启",
        hint: "true = 进程退出后 GNOME 自动拉起（本机 6/20 带它）",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-GNOME-Autostart-Notify",
        label: "就绪通知",
        hint: "true = 应用就绪后调 RegisterVisualStartupToken",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "AutostartCondition",
        label: "自启条件",
        hint: "GNOME 条件表达式（如 GNotExists / if-gnome-session-failed）",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-KDE-autostart-phase",
        label: "KDE 启动阶段",
        hint: "KDE 登录脚本阶段序号（0=初始 1=桌面就绪 2=用户会话）",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-KDE-autostart-after",
        label: "KDE 顺序跟随",
        hint: "在哪个 autostart 程序之后启动，填程序名",
        control: Control::Text,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
    KeySpec {
        form_group: "自启专有",
        key: "X-systemd-skip",
        label: "跳过 systemd 生成",
        hint: "true = 桌面不要把本条转成 systemd user 单元",
        control: Control::Toggle,
        options: &[],
        required: false,
        kinds: KIND_AUTOSTART_ONLY,
    },
];

/// 某键的定义。
pub fn spec_for(key: &str) -> Option<&'static KeySpec> {
    KEY_SPECS.iter().find(|s| s.key == key)
}

/// 某模块要渲染的键点（保表单顺序）。
pub fn specs_for_kind(kind: DesktopKind) -> impl Iterator<Item = &'static KeySpec> {
    KEY_SPECS.iter().filter(move |s| s.kinds.contains(&kind))
}

/// 分号列表拆分（忽略空段；保留非 ASCII 如中文关键词）。
pub fn split_semicolon(v: &str) -> Vec<String> {
    v.split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// 分号列表拼接（非空则带尾分号，桌面规范推荐写法）。
pub fn join_semicolon(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut s = items.join(";");
    s.push(';');
    s
}

// ---------------------------------------------------------------------------
// 表单纯逻辑（apply_form / validate_form，D12.7-②）
// ---------------------------------------------------------------------------

/// 表单值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormValue {
    /// 未提交（页面只提交动过的键；跳过）。
    Unset,
    /// 单值；**空串 = 不动**（D12.2：空值不动；与 unitfile「空=删」不同）。
    Text(String),
    /// 分号列表（整组替换）；**空表 = 不动**。
    List(Vec<String>),
    /// 开关（总是写 true/false）。
    Bool(bool),
}

/// 一次表单提交：`(键, 值)` 序列（只作用于 `[Desktop Entry]` 组）。
#[derive(Clone, Debug, Default)]
pub struct FormValues {
    pub fields: Vec<(String, FormValue)>,
}

impl FormValues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_text(mut self, key: &str, v: impl Into<String>) -> Self {
        self.fields.push((key.into(), FormValue::Text(v.into())));
        self
    }

    pub fn set_list(mut self, key: &str, v: Vec<String>) -> Self {
        self.fields.push((key.into(), FormValue::List(v)));
        self
    }

    pub fn set_bool(mut self, key: &str, v: bool) -> Self {
        self.fields.push((key.into(), FormValue::Bool(v)));
        self
    }

    pub fn unset(mut self, key: &str) -> Self {
        self.fields.push((key.into(), FormValue::Unset));
        self
    }
}

/// 把表单写回文档：**只写回动过的键**，空值不动，返回新文档（原 doc 不变）。
///
/// - 键不在 [`KEY_SPECS`] → `Err`（防任意键注入）；
/// - 非 `Application`（`Type=Service/Link/Directory`）→ **只放行「身份与描述」组**
///   （D12.10：执行类键不适用）；
/// - 值含换行 → `Err`。
///
/// 签名与 GOAL 12.2 原文一致（kind 的分页分流由页面按 [`specs_for_kind`] 控制，
/// apply 层不再收 kind——键点表本身与 kind 无关，见进度记录实现期说明）。
pub fn apply_form(doc: &DesktopDoc, form: &FormValues) -> Result<DesktopDoc, String> {
    let mut out = doc.clone();
    let is_app = out.entry_type() != Some("Application") && out.entry_type().is_some();
    for (key, value) in &form.fields {
        let Some(spec) = spec_for(key) else {
            return Err(format!("未收录的键，不能通过表单修改：{key}"));
        };
        if value == &FormValue::Unset {
            continue;
        }
        if is_app && spec.form_group != "身份与描述" {
            return Err(format!(
                "{key} 不适用：该文件 Type={}（非 Application），只开放「身份与描述」组（D12.10）",
                out.entry_type().unwrap_or("?")
            ));
        }
        match value {
            FormValue::Unset => {}
            FormValue::Text(s) => {
                if s.is_empty() {
                    continue; // 空值不动
                }
                ensure_no_newline(s)?;
                out.set_value(ENTRY_GROUP, key, s);
            }
            FormValue::List(items) => {
                if items.is_empty() {
                    continue; // 空表不动
                }
                if items.iter().any(|i| i.contains('\n') || i.contains('\r')) {
                    return Err(format!("{key} 的值不能包含换行"));
                }
                let joined = join_semicolon(items);
                out.set_value(ENTRY_GROUP, key, &joined);
            }
            FormValue::Bool(b) => {
                let v = if *b { "true" } else { "false" };
                // Hidden 的 false 语义：写 false 与删行等价，选删行更贴近桌面惯例（D12.3「或删该行」）
                if key == "Hidden" && !*b && !out.has_key(ENTRY_GROUP, key) {
                    continue;
                }
                out.set_value(ENTRY_GROUP, key, v);
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

/// Exec 字段码白名单（D12.7-②，摸底 6 实测 `%o`/`%z` 非法）。
const EXEC_FIELD_CODES: &[char] = &[
    'f', 'F', 'u', 'U', 'd', 'D', 'n', 'N', 'i', 'c', 'k', 'v', 'm', '%',
];

/// 校验 `Exec` 值的字段码（纯函数）。
///
/// 规则：`%%` 是转义的字面 `%`；其余 `%X` 的 `X` 必须在白名单；
/// 结尾孤立 `%` 非法；无 `%` 直接过。
pub fn validate_exec_field_codes(exec: &str) -> Result<(), String> {
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        match chars.peek() {
            None => return Err("Exec 以孤立的 % 结尾（字段码不完整）".to_string()),
            Some(&n) => {
                chars.next();
                if !EXEC_FIELD_CODES.contains(&n) {
                    return Err(format!(
                        "Exec 含非法字段码 %{n}（只允许 %f %F %u %U %d %D %n %N %i %c %k %v %m %%）"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 校验分号列表键的段格式（纯函数，按键分流）。
pub fn validate_semicolon_list(key: &str, value: &str) -> Result<(), String> {
    // 尾分号可有可无；过滤空段后逐段校验
    let segs: Vec<&str> = value.split(';').collect();
    let items: Vec<&&str> = segs.iter().filter(|s| !s.is_empty()).collect();
    if items.is_empty() {
        return Err(format!("{key} 不能只有分隔符"));
    }
    // 空段拦截：首段（;a）与中间段（a;;b）非法；仅允许**末尾**一个空段（a; 的尾巴）
    let body = &segs[..segs.len() - 1];
    if body.iter().any(|s| s.is_empty()) {
        return Err(format!("{key} 含空段（连续分号或开头分号）：{value}"));
    }
    for seg in &segs {
        if seg.is_empty() {
            continue;
        }
        match key {
            "Categories" => {
                if !seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
                {
                    return Err(format!("Categories 含非法字符的段：{seg}"));
                }
            }
            "MimeType" => {
                let ok = seg.contains('/')
                    && seg.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_' | '*' | '/')
                    })
                    && !seg.starts_with('/')
                    && !seg.ends_with('/');
                if !ok {
                    return Err(format!("MimeType 段必须是「类型/子类型」形式：{seg}"));
                }
            }
            // Keywords / OnlyShowIn / NotShowIn：非空、无 '='、首尾无空白
            _ => {
                if seg.contains('=') || seg.trim() != *seg {
                    return Err(format!("{key} 含非法段：{seg:?}"));
                }
            }
        }
    }
    Ok(())
}

/// 校验 Icon 值（纯函数）：主题图标名或绝对路径。
pub fn validate_icon(icon: &str) -> Result<(), String> {
    let ok_name = !icon.is_empty()
        && icon
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '@'));
    let ok_path = icon.starts_with('/')
        && !icon.contains('\0')
        && icon
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+@".contains(c));
    if ok_name || ok_path {
        Ok(())
    } else {
        Err(format!("Icon 既不是合法图标名也不是绝对路径：{icon}"))
    }
}

/// 校验文档（D12.7-② 自家闸门；`desktop-file-validate` 由 utils 层跑）。
///
/// - `Type` 必须存在且 ∈ 四值；
/// - `Type=Application` → `Name` 与 `Exec` 必填；
/// - `Exec` 存在 → 字段码白名单；
/// - 布尔键存在 → 只能 `true`/`false`（精确小写，桌面规范）；
/// - 分号键存在 → 段格式；
/// - `Icon` 存在 → 图标名或路径。
pub fn validate_form(doc: &DesktopDoc, kind: DesktopKind) -> Result<(), String> {
    // ① Type
    let ty = doc.get("Type").ok_or("缺少必填键 Type")?;
    if !TYPE_OPTIONS.contains(&ty.as_str()) {
        return Err(format!(
            "Type 取值非法：{ty}（只允许 Application / Link / Directory / Service）"
        ));
    }
    // ② Application 必填
    if ty == "Application" {
        let name = doc.get("Name").unwrap_or_default();
        if name.trim().is_empty() {
            return Err("Type=Application 必须有 Name".to_string());
        }
        let exec = doc.get("Exec").unwrap_or_default();
        if exec.trim().is_empty() {
            return Err("Type=Application 必须有 Exec".to_string());
        }
    }
    // ③ Exec 字段码
    if let Some(exec) = doc.get("Exec")
        && !exec.trim().is_empty()
    {
        validate_exec_field_codes(&exec)?;
    }
    // ④ 布尔键
    for spec in KEY_SPECS.iter().filter(|s| s.control == Control::Toggle) {
        if let Some(v) = doc.get(spec.key)
            && v != "true"
            && v != "false"
        {
            return Err(format!("{} 只能是 true/false，实际：{v}", spec.key));
        }
    }
    // ⑤ 分号键
    for spec in KEY_SPECS
        .iter()
        .filter(|s| matches!(s.control, Control::SList | Control::Select))
    {
        if let Some(v) = doc.get(spec.key)
            && !v.trim().is_empty()
            && (spec.control == Control::SList
                || v.contains(';')
                || matches!(spec.key, "OnlyShowIn" | "NotShowIn"))
        {
            validate_semicolon_list(spec.key, &v)?;
        }
    }
    // ⑥ Icon
    if let Some(icon) = doc.get("Icon")
        && !icon.trim().is_empty()
    {
        validate_icon(&icon)?;
    }
    // kind 目前与必填规则无关（两模块同表单底座）；保留参数供按模块差异化
    let _ = kind;
    Ok(())
}

// ---------------------------------------------------------------------------
// 新建默认模板（D12.5-④）
// ---------------------------------------------------------------------------

/// 新建条目的默认模板（菜单 / 自启分流；必填项留空等用户填，validate 会把关）。
pub fn default_desktop_text(kind: DesktopKind) -> String {
    // 不预置 Icon=/Categories= 空键：空值写进文件是噪音（apply 的 Text 分支会按需补插新键，
    // 用户填了才有）。只留必填 + 两个常见默认。
    let mut s = String::from("[Desktop Entry]\nType=Application\nName=\nExec=\n");
    match kind {
        DesktopKind::Menu => s.push_str("Terminal=false\nStartupNotify=true\n"),
        // 自启加 NoDisplay=true（本机 16/20 带它，避免同时出现在菜单里）
        DesktopKind::Autostart => {
            s.push_str("NoDisplay=true\nTerminal=false\nStartupNotify=true\n")
        }
    }
    s
}

#[cfg(test)]
mod tests;
