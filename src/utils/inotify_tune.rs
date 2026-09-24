//! inotify 限制调优的逻辑层（纯逻辑，不依赖 GTK / libadwaita / glib）。
//!
//! 负责：读取 `/proc/sys/fs/inotify/*` 的实时值、解析 `/etc/sysctl.d/*.conf` 与
//! `/etc/sysctl.conf` 里的持久化配置、需要提权时通过 `pkexec` 执行 `sysctl -w`
//! 与 `tee`（参考 `utils::imfix` 的提权模型）。
//!
//! ⚠️ 持久化写入必须走 `/etc/sysctl.d/99-linbox-inotify.conf`，**绝不能**写
//! `/etc/sysctl.conf`：Debian/Ubuntu 的 sysctl init 脚本对 `/etc/sysctl.conf`
//! 会执行 `sed -e 's/\s#.*$//'` 去掉整行注释后再应用，Netfilter/DSA 之类
//! **被注释掉的调优建议**会被误应用。新写的发行版用 systemd-sysctl，该问题
//! 不存在，但为跨发行版安全，一律不碰这个文件。
//!
//! 约束（见 `docs/项目结构规划书.md` §3.7）：本文件禁止 `use gtk` / `use adw` /
//! `use glib`，输入输出均为数据。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::inotify::{InotifyStatus, RECOMMENDED_INSTANCES, RECOMMENDED_WATCHES};

/// 实时值来源（内核导出的当前生效值，最可信）。
const WATCHES_PROC: &str = "/proc/sys/fs/inotify/max_user_watches";
const INSTANCES_PROC: &str = "/proc/sys/fs/inotify/max_user_instances";

/// 持久化文件：99 前缀保证在绝大多数发行版默认配置**之后**加载（后加载者胜）。
const PERSIST_PATH: &str = "/etc/sysctl.d/99-linbox-inotify.conf";

/// 一个参数名（白名单，防止把任意字符串塞进 sysctl 命令）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Param {
    Watches,
    Instances,
}

impl Param {
    pub fn key(self) -> &'static str {
        match self {
            Param::Watches => "fs.inotify.max_user_watches",
            Param::Instances => "fs.inotify.max_user_instances",
        }
    }

    pub fn proc_path(self) -> &'static str {
        match self {
            Param::Watches => WATCHES_PROC,
            Param::Instances => INSTANCES_PROC,
        }
    }

    /// 中文显示名。
    pub fn label(self) -> &'static str {
        match self {
            Param::Watches => "监听数 (max_user_watches)",
            Param::Instances => "实例数 (max_user_instances)",
        }
    }
}

/// 本模块能调整的全部参数。
pub const ALL_PARAMS: [Param; 2] = [Param::Watches, Param::Instances];

/// 平台支持性：本功能依赖 `/proc/sys/fs/inotify`，只在 Linux 上有意义。
pub fn platform_supported() -> bool {
    Path::new(WATCHES_PROC).exists()
}

// ---------------------------------------------------------------------------
// 读取
// ---------------------------------------------------------------------------

/// 读一个 sysctl 键的实时值（`/proc` 优先；读不到返回 None）。
fn read_live(param: Param) -> Option<u64> {
    let raw = fs::read_to_string(param.proc_path()).ok()?;
    raw.trim().parse::<u64>().ok()
}

pub fn live_watches() -> Option<u64> {
    read_live(Param::Watches)
}

pub fn live_instances() -> Option<u64> {
    read_live(Param::Instances)
}

/// 收集 `/etc/sysctl.d/*.conf` 与 `/etc/sysctl.conf` 里的 fs.inotify 配置。
///
/// **模拟 sysctl 的加载优先级**：同名键"后加载者胜"，加载顺序为
/// `/etc/sysctl.conf` 先、`/etc/sysctl.d/*.conf` 按文件名字典序后。
/// 所以返回的 map 里留下的一定是真正生效的那个文件和值。
///
/// 返回 `(key → (值, 来源文件))`；同时保留全部出现位置供展示"被谁覆盖"。
pub type EffectiveMap = BTreeMap<String, (u64, String)>;
/// 一次出现：(键, 值, 来源文件)。
pub type Occurrence = (String, u64, String);

pub fn collect_persisted() -> (EffectiveMap, Vec<Occurrence>) {
    let mut files: Vec<PathBuf> = Vec::new();

    // sysctl 处理 --system 时的顺序：先 /etc/sysctl.conf，再 /etc/sysctl.d/*.conf。
    // 我们只读不写 sysctl.conf，但仍然要读它 —— 它可能覆盖或被覆盖，影响生效判定。
    if Path::new("/etc/sysctl.conf").exists() {
        files.push(PathBuf::from("/etc/sysctl.conf"));
    }
    if let Ok(rd) = fs::read_dir("/etc/sysctl.d") {
        let mut confs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("conf") && p.is_file())
            .collect();
        // 字典序：与 sysctl 的 glob 排序一致（99-* 天然排在 50-* 之后）。
        confs.sort();
        files.extend(confs);
    }

    let mut texts: Vec<(String, String)> = Vec::new();
    for path in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue; // 权限不足 / 二进制 / 软链断裂：跳过，不影响主流程
        };
        texts.push((path.to_string_lossy().into_owned(), text));
    }
    effective_from_texts(&texts)
}

/// 从「(文件名, 内容)」列表按加载顺序算出生效值（纯函数，可单测）。
///
/// 约定与 `collect_persisted` 一致：输入必须按 sysctl 的实际加载顺序排列，
/// 同名键后出现者胜。
pub fn effective_from_texts(files: &[(String, String)]) -> (EffectiveMap, Vec<Occurrence>) {
    let mut effective: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut all: Vec<(String, u64, String)> = Vec::new();

    for (src, text) in files {
        for line in text.lines() {
            let Some((k, v)) = parse_sysctl_line(line) else {
                continue;
            };
            if !k.starts_with("fs.inotify.") {
                continue;
            }
            all.push((k.clone(), v, src.clone()));
            // 后面的文件/行覆盖前面的（"最后出现者胜"）。
            effective.insert(k, (v, src.clone()));
        }
    }
    (effective, all)
}

/// 解析一行 sysctl 配置：`key = value` / `key=value`。
///
/// 注释行（# 或 ; 开头）返回 None；行尾注释只剥掉「空白 + #」，
/// 避免 `max_user_watches=524288 # 说明` 里把值也吃掉。
fn parse_sysctl_line(line: &str) -> Option<(String, u64)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
        return None;
    }
    // 剥掉行尾注释：只在「空白 + #」时剥，正常值里不含空白。
    let body = match t.find(" #") {
        Some(i) => t[..i].trim_end(),
        None => match t.find("\t#") {
            Some(i) => t[..i].trim_end(),
            None => t,
        },
    };
    let (k, v) = body.split_once('=')?;
    let key = k.trim().to_string();
    let val = v.trim().parse::<u64>().ok()?;
    Some((key, val))
}

/// 汇总当前状态（实时值 + 持久化值 + 生效来源）。
pub fn status() -> InotifyStatus {
    let (persisted, _all) = collect_persisted();
    let w = persisted.get("fs.inotify.max_user_watches");
    let i = persisted.get("fs.inotify.max_user_instances");
    InotifyStatus {
        watches: live_watches().unwrap_or(0),
        instances: live_instances().unwrap_or(0),
        persisted_watches: w.map(|(v, _)| *v),
        persisted_instances: i.map(|(v, _)| *v),
        // 两个键的来源文件可能不同；展示时优先报 watches 的来源。
        persisted_source: w.or(i).map(|(_, s)| s.clone()),
    }
}

// ---------------------------------------------------------------------------
// 校验
// ---------------------------------------------------------------------------

/// watches 合理性上限：防止误输入一个天文数字（每个 watch 占内核少量 slab 内存）。
/// 参考值：社区最高常见建议 2097152；给 4× 余量。超过即拒绝。
const SANE_MAX_WATCHES: u64 = 8_388_608;

/// instances 合理性上限：每个实例是一组内核数据结构，远低于 watches 的合理上限。
const SANE_MAX_INSTANCES: u64 = 65_536;

/// 校验用户输入：必须是纯数字、不为 0、不超过该参数的合理上限。
///
/// **这是"点了没反应"的防线**：`sysctl` 对越界值可能静默拒绝（或回 EINVAL），
/// 若不校验就会出现「提示成功但值没变」。执行前后都会比对实时值。
pub fn validate(param: Param, value: u64) -> Result<(), String> {
    if value == 0 {
        return Err("不能为 0（会把该能力直接关闭）".to_string());
    }
    let max = match param {
        Param::Watches => SANE_MAX_WATCHES,
        Param::Instances => SANE_MAX_INSTANCES,
    };
    if value > max {
        return Err(format!(
            "数值过大（> {max}）。inotify 资源占用内核内存，\
             过大的值会侵蚀物理内存。推荐值 {RECOMMENDED_WATCHES} 已覆盖绝大多数场景。"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 写入：立即生效
// ---------------------------------------------------------------------------

/// 用 `pkexec sysctl -w` 让参数立即生效（无需重启）。
///
/// 值先做数字校验再过 `validate`，拼进命令串是安全的；但仍把 key/value 作为
/// **独立 argv** 传递，避免 shell 解释（不用 `sh -c`）。
pub fn apply_live(params: &[(Param, u64)]) -> Result<(), String> {
    if params.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec!["-w".to_string()];
    for (p, v) in params {
        validate(*p, *v)?;
        args.push(format!("{}={}", p.key(), v));
    }

    let output = Command::new("pkexec")
        .arg("sysctl")
        .args(&args)
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("无法启动 pkexec（请确认已安装 polkit）：{e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "sysctl 应用失败（退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            err.trim()
        ));
    }
    Ok(())
}

/// 应用后**回读校验**：确认实时值真的变成了目标值。
///
/// 返回不一致的 `(参数, 期望, 实际)` 列表；空列表表示全部生效。
pub fn verify_live(params: &[(Param, u64)]) -> Vec<(Param, u64, u64)> {
    params
        .iter()
        .filter_map(|(p, want)| {
            let got = read_live(*p)?;
            (got != *want).then_some((*p, *want, got))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 写入：持久化
// ---------------------------------------------------------------------------

/// 把目标值写进 `/etc/sysctl.d/99-linbox-inotify.conf`（整体重写该文件）。
///
/// 通过 `pkexec tee <path>` 从 **stdin** 喂内容，不在命令行里嵌入内容，
/// 也不会因为重定向而需要额外的 root shell（参考 `imfix::write_env_as_root`）。
///
/// 只重写自己管理的这一个文件，**不碰其它 sysctl 配置**：
/// 其它文件若在同名键上设了更小的值、且字典序排在 99-* 之后，会覆盖我们；
/// 该情况由页面读 `collect_persisted()` 的 effective 值检测后提示用户，不自动改。
pub fn persist(params: &[(Param, u64)]) -> Result<(), String> {
    if params.is_empty() {
        return Ok(());
    }
    for (p, v) in params {
        validate(*p, *v)?;
    }
    let content = build_persist_content(params);

    let mut child = Command::new("pkexec")
        .arg("tee")
        .arg(PERSIST_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 pkexec（请确认已安装 polkit）：{e}"))?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "无法获取 tee 标准输入".to_string())?;
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| format!("写入失败：{e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("等待 pkexec 失败：{e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "持久化失败（pkexec 退出码 {}）：{}",
            output.status.code().unwrap_or(-1),
            err.trim()
        ));
    }
    Ok(())
}

/// 生成持久化文件内容（带说明注释，方便用户日后手改）。
fn build_persist_content(params: &[(Param, u64)]) -> String {
    let mut s = String::new();
    s.push_str("# 由 linbox「inotify 调优」页面写入。\n");
    s.push_str(
        "# 提高文件监听上限（VS Code / WebStorm / webpack / nodemon / watchman 等常用）。\n",
    );
    s.push_str("# 删除本文件并重启即可恢复默认值；也可手动改小数值。\n");
    s.push_str("# 注：故意不写 /etc/sysctl.conf —— Debian 的 sysctl 脚本会剥掉该文件的\n");
    s.push_str("#     整行注释并应用，可能误启用其中被注释掉的建议项。\n");
    for (p, v) in params {
        s.push_str(&format!("{}={}\n", p.key(), v));
    }
    s
}

/// 一次性生成推荐预设参数：watches → 2097152、instances → 1024。
///
/// 只包含**当前低于推荐值**的项；已达标的不出现在结果里（不会被调小）。
/// 返回空列表表示全部已达标。
pub fn preset_params(current_watches: u64, current_instances: u64) -> Vec<(Param, u64)> {
    let mut v = Vec::new();
    if current_watches < RECOMMENDED_WATCHES {
        v.push((Param::Watches, RECOMMENDED_WATCHES));
    }
    if current_instances < RECOMMENDED_INSTANCES {
        v.push((Param::Instances, RECOMMENDED_INSTANCES));
    }
    v
}

/// 是否已有 telemetry 的 udev 规则在管这个值（存在即说明系统已自动调优）。
pub fn telemetry_rule_present() -> bool {
    let rd = match fs::read_dir("/etc/udev/rules.d") {
        Ok(rd) => rd,
        Err(_) => return false,
    };
    rd.filter_map(|e| e.ok())
        .any(|e| match e.file_name().to_str() {
            Some(n) => n.contains("inotify") && n.ends_with(".rules"),
            None => false,
        })
}

// ── 测试 ──────────────────────────────────────────────────────────────────
//
// 只测纯逻辑（解析 / 校验 / 预设），不碰 pkexec 和 /proc —— 那两条在
// CI/无提权环境里根本跑不通。

#[cfg(test)]
mod tests {
    use super::*;

    /// 行解析：等号两侧空白、行尾注释、注释行、非法值。
    #[test]
    fn parse_sysctl_line_variants() {
        assert_eq!(
            parse_sysctl_line("fs.inotify.max_user_watches=524288"),
            Some(("fs.inotify.max_user_watches".into(), 524288))
        );
        assert_eq!(
            parse_sysctl_line("  fs.inotify.max_user_watches \t = \t 524288  "),
            Some(("fs.inotify.max_user_watches".into(), 524288))
        );
        // 行尾注释（带前导空白）要被剥掉
        assert_eq!(
            parse_sysctl_line("fs.inotify.max_user_watches=524288 # 提高上限"),
            Some(("fs.inotify.max_user_watches".into(), 524288))
        );
        // 整行注释 / 分号注释 / 空行 / 非数字 → None
        assert_eq!(parse_sysctl_line("# fs.inotify.max_user_watches=1"), None);
        assert_eq!(parse_sysctl_line("; foo=1"), None);
        assert_eq!(parse_sysctl_line(""), None);
        assert_eq!(parse_sysctl_line("fs.inotify.max_user_watches=abc"), None);
        assert_eq!(parse_sysctl_line("no_equals_here"), None);
    }

    /// 加载优先级：同名键后加载者胜 —— 99-* 必须赢过 50-*，
    /// 这正是本模块把持久化文件命名为 99-linbox-inotify.conf 的全部理由。
    #[test]
    fn effective_value_is_last_loaded() {
        let files = vec![
            // 模拟加载顺序：sysctl.conf 最先，然后 /etc/sysctl.d/ 按字典序
            (
                "/etc/sysctl.conf".to_string(),
                "fs.inotify.max_user_watches=8192\n".to_string(),
            ),
            (
                "/etc/sysctl.d/50-kde.conf".to_string(),
                "fs.inotify.max_user_watches=106496\n".to_string(),
            ),
            (
                "/etc/sysctl.d/99-linbox-inotify.conf".to_string(),
                "fs.inotify.max_user_watches=524288\n".to_string(),
            ),
        ];
        let (eff, all) = effective_from_texts(&files);
        assert_eq!(
            eff.get("fs.inotify.max_user_watches"),
            Some(&(
                524288u64,
                "/etc/sysctl.d/99-linbox-inotify.conf".to_string()
            )),
            "99-* 必须覆盖 50-* 和 sysctl.conf"
        );
        // all 保留全部出现位置（供页面提示"被谁覆盖"）
        assert_eq!(all.len(), 3);
    }

    /// 非 fs.inotify.* 的键不收，避免把无关配置当成可覆盖项展示。
    #[test]
    fn ignores_other_keys() {
        let files = vec![(
            "/etc/sysctl.d/10-net.conf".to_string(),
            "net.core.somaxconn=4096\nfs.inotify.max_user_instances=512\n".to_string(),
        )];
        let (eff, all) = effective_from_texts(&files);
        assert!(!eff.contains_key("net.core.somaxconn"));
        assert_eq!(eff.get("fs.inotify.max_user_instances").unwrap().0, 512);
        assert_eq!(all.len(), 1);
    }

    /// 校验：0 和天文数字必须被拒 —— 否则会出现「提示成功但 sysctl 静默拒绝」。
    #[test]
    fn validate_bounds() {
        assert!(validate(Param::Watches, 0).is_err());
        assert!(validate(Param::Watches, 524288).is_ok());
        assert!(validate(Param::Watches, 2097152).is_ok());
        assert!(validate(Param::Watches, SANE_MAX_WATCHES + 1).is_err());
        // instances 的上限独立且更低：800 万对 instances 不合理
        assert!(validate(Param::Instances, 8192).is_ok());
        assert!(validate(Param::Instances, SANE_MAX_INSTANCES + 1).is_err());
    }

    /// 预设：已达标项不出现在结果里（绝不会被调小）。
    #[test]
    fn preset_never_lowers() {
        // 两项都高于推荐值 → 空结果（页面据此提示"已达标"）
        assert!(preset_params(4194304, 8192).is_empty());
        // watches 达标、instances 未达标 → 只返回 instances
        assert_eq!(
            preset_params(2097152, 128),
            vec![(Param::Instances, RECOMMENDED_INSTANCES)]
        );
    }

    /// 预设：默认 8192 / 128 桌面 → 两项分别提到 2097152 / 1024。
    #[test]
    fn preset_raises_defaults() {
        let params = preset_params(8192, 128);
        assert_eq!(params[0], (Param::Watches, RECOMMENDED_WATCHES));
        assert_eq!(params[1], (Param::Instances, RECOMMENDED_INSTANCES));
    }

    /// 持久化文件内容：每个键恰好一行、可被我们的解析器原样读回。
    #[test]
    fn persist_content_roundtrips() {
        let content = build_persist_content(&[(Param::Watches, 524288), (Param::Instances, 512)]);
        let (eff, all) = effective_from_texts(&[("x.conf".to_string(), content)]);
        assert_eq!(all.len(), 2, "两个键都应出现");
        assert_eq!(eff.get("fs.inotify.max_user_watches").unwrap().0, 524288);
        assert_eq!(eff.get("fs.inotify.max_user_instances").unwrap().0, 512);
    }

    /// 参数白名单元数据（key 同时是 sysctl 命令参数，拼错会注入）。
    #[test]
    fn param_whitelist_metadata() {
        assert_eq!(Param::Watches.key(), "fs.inotify.max_user_watches");
        assert_eq!(Param::Instances.key(), "fs.inotify.max_user_instances");
        assert!(Param::Watches.proc_path().contains("/proc/sys/fs/inotify/"));
        assert!(Param::Instances.proc_path().ends_with("max_user_instances"));
        assert!(Param::Watches.label().contains("max_user_watches"));
        assert_eq!(ALL_PARAMS, [Param::Watches, Param::Instances]);
    }

    /// 合法区间的边界值：恰好等于上限必须通过（+1 已被 validate_bounds 覆盖为拒绝）。
    #[test]
    fn validate_upper_boundary_exact() {
        assert!(validate(Param::Watches, SANE_MAX_WATCHES).is_ok());
        assert!(validate(Param::Instances, SANE_MAX_INSTANCES).is_ok());
        assert!(validate(Param::Watches, 1).is_ok(), "最小合法值 1");
    }

    /// Linux 真机 smoke：/proc 必然存在且有可读默认值。
    #[test]
    fn platform_and_live_readable_on_linux() {
        assert!(
            platform_supported(),
            "Linux 桌面应支持 /proc/sys/fs/inotify"
        );
        let w = live_watches().expect("max_user_watches 必须可读");
        assert!(w >= 8192, "内核默认至少 8192，实际 {w}");
        assert!(live_instances().is_some());
    }
}
