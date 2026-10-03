//! `/etc/fstab` 的数据模型（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：本文件只定义 `struct`/`enum` 与纯数据
//! 变换，解析/序列化/外部命令在 [`crate::utils::fstab`]（GOAL Phase 7 分层）。
//!
//! 关键设计（对应 GOAL D7.4 注释/空白保真）：
//! - 文档模型是**行交错**的：注释行、空行、条目行按原顺序存进 [`FstabDocument::lines`]，
//!   往返（parse → serialize）不丢注释、不改行序。
//! - 条目带 `raw: Option<String>`：`Some` = 该行解析后未被修改，序列化**原样输出**；
//!   任何 setter 都会把它清成 `None`，此后由 [`FstabEntry::canonical_line`] 重新生成。
//!   未修改行逐字节保真、修改行格式化输出，两不相误。

/// 一条 fstab 记录（fstab(5) 的六字段行）。
///
/// 字段值**按文件原文保留**（含 `\040` 等八进制转义，不做反转义）——
/// 转义只在展示层做（[`crate::utils::fstab::unescape_field`]），这样往返零损耗，
/// 且重复 save 不会二次转义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FstabEntry {
    /// 第 1 字段：设备来源（`UUID=...` / `LABEL=...` / `/dev/...` / `host:/path` / `none`）。
    pub source: String,
    /// 第 2 字段：挂载点（swap 等无挂载点条目为 `none`）。
    pub mountpoint: String,
    /// 第 3 字段：文件系统类型（ext4 / btrfs / swap / nfs ...）。
    pub vfstype: String,
    /// 第 4 字段：挂载选项（逗号分隔，如 `defaults,nofail`）。
    pub options: String,
    /// 第 5 字段：dump 标志。**原文字符串保留**（只允许数字，校验在 utils 层）。
    pub dump: String,
    /// 第 6 字段：fsck 顺序（0/1/2）。**原文字符串保留**，理由同 `dump`。
    pub pass: String,
    /// 解析时所在行号（1 起）。仅用于报错定位；文档被编辑后可能过期。
    pub line_no: usize,
    /// 是否被行首 `#` 注释停用（GOAL D7.5 的唯一「移除」方式）。
    pub disabled: bool,
    /// 解析得到的原始行文本。`Some` = 未修改，序列化原样输出；setter 置 `None`。
    pub raw: Option<String>,
}

impl FstabEntry {
    /// 从四个必填字段新建条目（新增条目走这里）：dump/pass 缺省 `0`，未停用，无 raw。
    pub fn new(
        source: impl Into<String>,
        mountpoint: impl Into<String>,
        vfstype: impl Into<String>,
        options: impl Into<String>,
    ) -> Self {
        FstabEntry {
            source: source.into(),
            mountpoint: mountpoint.into(),
            vfstype: vfstype.into(),
            options: options.into(),
            dump: "0".to_string(),
            pass: "0".to_string(),
            line_no: 0,
            disabled: false,
            raw: None,
        }
    }

    /// 生成规范行文本（不含换行）。`raw` 存在时由调用方优先用 raw（见 [`FstabLine::to_line_string`]）。
    ///
    /// source / mountpoint / options 里的空白按 fstab 规则转成 `\040` / `\011`，
    /// 保证生成行永远能被自家解析器读回（字段不因空格错位）。
    pub fn canonical_line(&self) -> String {
        let body = format!(
            "{} {} {} {} {} {}",
            escape_field(&self.source),
            escape_field(&self.mountpoint),
            self.vfstype,
            escape_field(&self.options),
            self.dump,
            self.pass
        );
        if self.disabled {
            format!("# {body}")
        } else {
            body
        }
    }

    /// 停用/恢复该条目（行首 `#`）。会清掉 `raw`（行内容已变化，不能再原样输出）。
    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
        self.raw = None;
    }

    pub fn set_source(&mut self, v: impl Into<String>) {
        self.source = v.into();
        self.raw = None;
    }

    pub fn set_mountpoint(&mut self, v: impl Into<String>) {
        self.mountpoint = v.into();
        self.raw = None;
    }

    pub fn set_vfstype(&mut self, v: impl Into<String>) {
        self.vfstype = v.into();
        self.raw = None;
    }

    pub fn set_options(&mut self, v: impl Into<String>) {
        self.options = v.into();
        self.raw = None;
    }

    pub fn set_dump(&mut self, v: impl Into<String>) {
        self.dump = v.into();
        self.raw = None;
    }

    pub fn set_pass(&mut self, v: impl Into<String>) {
        self.pass = v.into();
        self.raw = None;
    }

    /// swap 条目（fstype=swap）——挂载动作不同（swapon 而非 mount）。
    pub fn is_swap(&self) -> bool {
        self.vfstype == "swap"
    }
}

/// fstab 文件里的一行（行交错文档模型的最小单元）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FstabLine {
    /// 纯空白行，字符串为行内容（可能含空格/制表符，原样保留）。
    Blank(String),
    /// 整行注释（含 `#`，原样保留）。
    Comment(String),
    /// 一条挂载记录。
    Entry(FstabEntry),
}

impl FstabLine {
    /// 序列化为一行文本（不含换行）：条目优先用 `raw`（未修改保真），否则生成规范行。
    pub fn to_line_string(&self) -> String {
        match self {
            FstabLine::Blank(s) | FstabLine::Comment(s) => s.clone(),
            FstabLine::Entry(e) => e.raw.clone().unwrap_or_else(|| e.canonical_line()),
        }
    }
}

/// 整个 fstab 文档（条目 + 注释/空行交错，保序）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FstabDocument {
    /// 按文件顺序交错的行。
    pub lines: Vec<FstabLine>,
    /// 原文件是否以换行结尾（往返保真：`a\nb` 与 `a\nb\n` 是两个不同的文件）。
    pub trailing_newline: bool,
}

impl FstabDocument {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只看条目（跳过注释/空行），保序。
    pub fn entries(&self) -> impl Iterator<Item = &FstabEntry> {
        self.lines.iter().filter_map(|l| match l {
            FstabLine::Entry(e) => Some(e),
            _ => None,
        })
    }

    /// 可变遍历条目（就地编辑用）。
    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut FstabEntry> {
        self.lines.iter_mut().filter_map(|l| match l {
            FstabLine::Entry(e) => Some(e),
            _ => None,
        })
    }

    /// 条目数量（不含注释/空行）。
    pub fn entry_count(&self) -> usize {
        self.entries().count()
    }

    /// 在文档末尾追加一行。
    pub fn push_line(&mut self, line: FstabLine) {
        self.lines.push(line);
    }
}

/// 一条 fstab 条目对照真实挂载状态后的结论（GOAL 7.3）。
///
/// 优先级（[`crate::utils::fstab::mount_status`] 实现，真值表测试背书）：
/// 已挂载（现实优先） > 已停用 > 挂载点不存在 > 源不存在 > 未挂载。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountStatus {
    /// 挂载点出现在 `findmnt --json` 输出里。
    Mounted,
    /// 未挂载，但挂载点目录不存在（挂了会失败/开机报错）。
    DirMissing,
    /// 未挂载，目录存在，但 source（UUID/LABEL/设备）找不到。
    SourceMissing,
    /// 未挂载，其余正常（`noauto` 条目的常态）。
    NotMounted,
    /// 条目被 `#` 停用（且现实中确实没在挂）。
    Disabled,
}

impl MountStatus {
    /// 中文显示名（页面列直接用）。
    pub fn label(self) -> &'static str {
        match self {
            MountStatus::Mounted => "已挂载",
            MountStatus::DirMissing => "挂载点不存在",
            MountStatus::SourceMissing => "源不存在",
            MountStatus::NotMounted => "未挂载",
            MountStatus::Disabled => "已停用",
        }
    }
}

/// fstab 字段空白转义（空格 → `\040`、制表符 → `\011`），生成行时用。
///
/// 与 [`crate::utils::fstab::unescape_field`] 互逆；只处理这两个字符——
/// fstab 字段内不允许真实换行（会被当成新行），无需转义 `\012`。
pub fn escape_field(s: &str) -> String {
    if !s.contains(' ') && !s.contains('\t') {
        return s.to_string();
    }
    s.replace(' ', "\\040").replace('\t', "\\011")
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 新建条目默认值：dump/pass=0、未停用、无 raw（= 将由 canonical_line 生成）。
    #[test]
    fn entry_new_defaults() {
        let e = FstabEntry::new("UUID=aaaa", "/data", "ext4", "defaults,nofail");
        assert_eq!(e.dump, "0");
        assert_eq!(e.pass, "0");
        assert!(!e.disabled, "新条目默认启用");
        assert_eq!(e.raw, None);
        assert_eq!(e.line_no, 0);
        assert!(!e.is_swap());
    }

    /// 六字段规范行生成；swap 判定。
    #[test]
    fn entry_canonical_line_six_fields() {
        let mut e = FstabEntry::new("UUID=aaaa", "/data", "ext4", "defaults,nofail");
        e.dump = "1".to_string();
        e.pass = "2".to_string();
        assert_eq!(
            e.canonical_line(),
            "UUID=aaaa /data ext4 defaults,nofail 1 2"
        );
        let s = FstabEntry::new("UUID=bbbb", "none", "swap", "sw");
        assert!(s.is_swap());
        assert_eq!(s.canonical_line(), "UUID=bbbb none swap sw 0 0");
    }

    /// 停用/恢复：行首 `# ` 前缀 + raw 被清（不能再原样输出旧行）。
    #[test]
    fn set_disabled_prefixes_comment_and_clears_raw() {
        let mut e = FstabEntry::new("UUID=aaaa", "/data", "ext4", "defaults");
        e.raw = Some("UUID=aaaa    /data   ext4   defaults   0   0".into());
        e.set_disabled(true);
        assert!(e.disabled);
        assert_eq!(e.raw, None, "停用后行内容已变，raw 必须失效");
        assert_eq!(e.canonical_line(), "# UUID=aaaa /data ext4 defaults 0 0");
        e.set_disabled(false);
        assert_eq!(e.canonical_line(), "UUID=aaaa /data ext4 defaults 0 0");
    }

    /// 任一 setter 都清 raw —— 保证「改过的行绝不会被 raw 原样输出」（D7.4 的边界）。
    #[test]
    fn any_setter_invalidates_raw() {
        let mut e = FstabEntry::new("UUID=aaaa", "/data", "ext4", "defaults");
        e.raw = Some("old raw line".into());
        e.set_options("noauto,nofail");
        assert_eq!(e.raw, None);
        assert_eq!(e.canonical_line(), "UUID=aaaa /data ext4 noauto,nofail 0 0");

        e.raw = Some("old raw line".into());
        e.set_dump("1");
        assert_eq!(e.raw, None);
        e.raw = Some("old raw line".into());
        e.set_pass("2");
        assert_eq!(e.raw, None);
        e.raw = Some("old raw line".into());
        e.set_source("UUID=cccc");
        assert_eq!(e.raw, None);
        e.raw = Some("old raw line".into());
        e.set_mountpoint("/other");
        assert_eq!(e.raw, None);
        e.raw = Some("old raw line".into());
        e.set_vfstype("xfs");
        assert_eq!(e.raw, None);
    }

    /// 空白字段转义：含空格/制表符的挂载点生成行后仍能按 6 字段读回。
    #[test]
    fn canonical_line_escapes_whitespace() {
        let e = FstabEntry::new("UUID=aaaa", "/mnt/my disk", "ext4", "comment=hello world");
        assert_eq!(
            e.canonical_line(),
            "UUID=aaaa /mnt/my\\040disk ext4 comment=hello\\040world 0 0"
        );
        assert_eq!(e.canonical_line().split_whitespace().count(), 6);
    }

    /// escape_field：无空白零开销原样返回；空格与制表符分别转义。
    #[test]
    fn escape_field_variants() {
        assert_eq!(escape_field("/plain"), "/plain");
        assert_eq!(escape_field("a b"), "a\\040b");
        assert_eq!(escape_field("a\tb"), "a\\011b");
        assert_eq!(escape_field("a \tb"), "a\\040\\011b");
    }

    /// 行交错模型：raw 优先原样输出，注释/空行原样保留，entries() 只挑条目。
    #[test]
    fn document_interleaved_lines_and_raw_precedence() {
        let mut doc = FstabDocument::new();
        doc.push_line(FstabLine::Comment("# 头注释".into()));
        doc.push_line(FstabLine::Blank(String::new()));
        let mut e = FstabEntry::new("UUID=aaaa", "/a", "ext4", "defaults");
        e.raw = Some("UUID=aaaa   /a   ext4   defaults   0   0".into());
        doc.push_line(FstabLine::Entry(e));
        doc.push_line(FstabLine::Blank("   ".into())); // 含空白的空行也要保真
        doc.push_line(FstabLine::Comment("# 尾注释".into()));
        doc.trailing_newline = true;

        assert_eq!(doc.entry_count(), 1);
        let first = doc.entries().next().unwrap();
        assert_eq!(first.source, "UUID=aaaa");
        // raw 存在 → 即使字段被改过也…不，raw 优先是 FstabLine 层的约定：
        assert_eq!(
            doc.lines[2].to_line_string(),
            "UUID=aaaa   /a   ext4   defaults   0   0"
        );
        assert_eq!(doc.lines[3].to_line_string(), "   ");
        assert_eq!(doc.lines[0].to_line_string(), "# 头注释");
    }

    /// entries_mut 就地改条目（清 raw 后 to_line_string 走 canonical）。
    #[test]
    fn document_entries_mut_edits_in_place() {
        let mut doc = FstabDocument::new();
        doc.push_line(FstabLine::Comment("# c".into()));
        doc.push_line(FstabLine::Entry(FstabEntry::new(
            "UUID=aaaa",
            "/a",
            "ext4",
            "defaults",
        )));
        doc.push_line(FstabLine::Blank(String::new()));

        doc.entries_mut().for_each(|e| e.set_options("noauto"));
        assert_eq!(
            doc.lines[1].to_line_string(),
            "UUID=aaaa /a ext4 noauto 0 0"
        );
        // 注释与空行未被 entries_mut 触碰
        assert_eq!(doc.lines[0].to_line_string(), "# c");
        assert_eq!(doc.lines[2].to_line_string(), "");
    }
}
