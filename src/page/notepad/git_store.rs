//! Git 仓库作为备忘录存储后端。
//!
//! 仓库内目录结构（与本地 ZIP 内结构一致）：
//!   <repo>/
//!     <id>/
//!       meta.json
//!       content.txt
//!       inline/0.png
//!       files/report.docx
//!
//! 通过 `git` 命令行操作（clone / pull / add / commit / push）。
//! 认证：SSH 密钥优先，无密钥时走 HTTPS 用户名密码。

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Git 仓库配置。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GitConfig {
    /// 仓库地址（SSH: `git@github.com:user/repo.git`，HTTPS: `https://github.com/user/repo.git`）
    pub url: String,
    /// 分支名（默认 "main"）
    #[serde(default = "default_branch")]
    pub branch: String,
    /// git commit 用户名（可选，默认 "linbox"）
    #[serde(default)]
    pub user_name: String,
    /// git commit 邮箱（可选，默认 "linbox@notepad.local"）
    #[serde(default)]
    pub user_email: String,
    /// HTTPS 用户名（可选，SSH 时不需要）
    #[serde(default)]
    pub username: String,
    /// HTTPS 密码 / token（可选，SSH 时不需要）
    #[serde(default)]
    pub password: String,
}

fn default_branch() -> String {
    "main".to_string()
}

impl GitConfig {
    /// 有效的 git 用户名（未填则用默认值）。
    fn effective_user_name(&self) -> &str {
        if self.user_name.is_empty() {
            "linbox"
        } else {
            &self.user_name
        }
    }

    /// 有效的 git 邮箱（未填则用默认值）。
    fn effective_user_email(&self) -> &str {
        if self.user_email.is_empty() {
            "linbox@notepad.local"
        } else {
            &self.user_email
        }
    }
}

impl GitConfig {
    /// 带认证的 URL（HTTPS 时嵌入用户名密码；SSH 时原样返回）。
    // 仅由单测断言格式（askpass 流程当前走 plain URL + 环境变量注入，
    // 见文件头注释）；页面接线自定义凭证时恢复调用。
    #[allow(dead_code)]
    fn auth_url(&self) -> String {
        if self.url.starts_with("https://") && !self.username.is_empty() {
            // https://user:pass@github.com/user/repo.git
            let after = self.url.strip_prefix("https://").unwrap();
            format!("https://{}:{}@{}", self.username, self.password, after)
        } else {
            self.url.clone()
        }
    }

    /// 不含凭证的 URL（用于写入 .git/config，避免泄露密码）。
    fn plain_url(&self) -> &str {
        &self.url
    }

    /// 是否需要通过 GIT_ASKPASS 提供凭证。
    fn needs_askpass(&self) -> bool {
        self.url.starts_with("https://") && !self.username.is_empty()
    }
}

/// Git 操作结果。
pub type GitResult<T> = Result<T, String>;

// ── 本地工作目录 ──────────────────────────────────────────────────────────

/// 所有 Git 工作目录的根：`~/.local/share/linbox/notepad/git-repos/`
fn git_repos_root() -> PathBuf {
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("linbox")
        .join("notepad")
        .join("git-repos");
    let _ = fs::create_dir_all(&base);
    base
}

/// 某个仓库的本地工作目录。
pub fn repo_work_dir(url: &str) -> PathBuf {
    // 用 URL 的 hash 作为目录名，避免特殊字符
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let hash = format!("{:016x}", h.finish());
    git_repos_root().join(hash)
}

// ── Git 命令封装 ──────────────────────────────────────────────────────────

/// 执行 git 命令，返回 (exit_code, stdout, stderr)。
fn git_exec(work_dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new("git")
        .current_dir(work_dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| e.to_string());

    match output {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), format!("执行 git 失败：{e}")),
    }
}

/// 在指定目录执行 git（不依赖已有仓库）。
fn git_exec_raw(args: &[&str]) -> (i32, String, String) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| e.to_string());

    match output {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), format!("执行 git 失败：{e}")),
    }
}

// ── GIT_ASKPASS 凭证安全 ────────────────────────────────────────────────────

/// 临时 askpass 脚本守卫：创建临时脚本，Drop 时清理。
///
/// 防止密码被写入 `.git/config`（auth_url 会把 user:pass 嵌入 URL，
/// git 会把它存到 origin URL 里）。改用 GIT_ASKPASS 回调，凭据只在进程内传递。
struct AskpassGuard {
    script_path: PathBuf,
    _temp_dir: PathBuf,
}

impl AskpassGuard {
    /// 创建临时 askpass 脚本，返回守卫。
    fn new(username: &str, password: &str) -> Option<Self> {
        let dir_name = format!("linbox_askpass_{}", std::process::id());
        let temp_dir = std::env::temp_dir().join(&dir_name);
        let _ = fs::create_dir_all(&temp_dir);
        let script_path = temp_dir.join("askpass.sh");
        {
            let mut f = fs::File::create(&script_path).ok()?;
            let _ = writeln!(f, "#!/bin/sh");
            let _ = writeln!(f, "case \"$1\" in");
            let _ = writeln!(f, "  *[Uu]sername*) echo \"{username}\";;");
            let _ = writeln!(f, "  *[Pp]assword*) echo \"{password}\";;");
            let _ = writeln!(f, "  *) echo \"\";;");
            let _ = writeln!(f, "esac");
        }
        // chmod +x
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700));
        }
        Some(AskpassGuard {
            script_path,
            _temp_dir: temp_dir,
        })
    }

    fn path_str(&self) -> &str {
        self.script_path.to_str().unwrap_or("")
    }
}

impl Drop for AskpassGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.script_path);
        let _ = fs::remove_dir(&self._temp_dir);
    }
}

/// 执行需要认证的 git 命令（工作目录内）。
///
/// 使用 GIT_ASKPASS 回调提供凭证，不把密码嵌入 URL 写入 .git/config。
/// AskGuard 在函数作用域内保持存活，确保脚本在 git 进程期间存在。
fn git_exec_auth(work_dir: &Path, args: &[&str], config: &GitConfig) -> (i32, String, String) {
    let mut cmd = Command::new("git");
    cmd.current_dir(work_dir);
    cmd.args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    let _guard: Option<AskpassGuard>;
    if config.needs_askpass() {
        _guard = AskpassGuard::new(&config.username, &config.password);
        if let Some(ref g) = _guard {
            cmd.env("GIT_ASKPASS", g.path_str());
        }
    } else {
        _guard = None;
    }

    match cmd.output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), format!("执行 git 失败：{e}")),
    }
}

/// 执行需要认证的 git 命令（无工作目录，如 ls-remote）。
fn git_exec_auth_raw(args: &[&str], config: &GitConfig) -> (i32, String, String) {
    let mut cmd = Command::new("git");
    cmd.args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    let _guard: Option<AskpassGuard>;
    if config.needs_askpass() {
        _guard = AskpassGuard::new(&config.username, &config.password);
        if let Some(ref g) = _guard {
            cmd.env("GIT_ASKPASS", g.path_str());
        }
    } else {
        _guard = None;
    }

    match cmd.output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), format!("执行 git 失败：{e}")),
    }
}

// ── 公开 API ──────────────────────────────────────────────────────────────

/// 测试 Git 仓库是否可访问。
///
/// 优先尝试 SSH（无密码），失败后尝试 HTTPS。
/// 如果提供了 username/password，则用 HTTPS + 凭证。
/// 空仓库（没有任何 commit）也视为可访问。
pub fn verify_access(config: &GitConfig) -> GitResult<()> {
    if !config.username.is_empty() {
        let plain = config.plain_url();
        let (code, _, _) = git_exec_auth_raw(&["ls-remote", "--exit-code", plain, "HEAD"], config);
        if code == 0 || code == 2 {
            return Ok(());
        }
    }

    // 先试原样地址（可能是 SSH）
    let (code, _, _) = git_exec_raw(&["ls-remote", "--exit-code", &config.url, "HEAD"]);
    if code == 0 || code == 2 {
        // code=0: 正常; code=2: 空仓库（无 HEAD），仍然可访问
        return Ok(());
    }

    // SSH 失败，试 HTTPS 匿名
    if config.url.starts_with("git@") {
        let https_url = ssh_to_https(&config.url);
        return verify_url(&https_url);
    }

    Err("无法访问仓库".to_string())
}

fn verify_url(url: &str) -> GitResult<()> {
    let (code, _, _) = git_exec_raw(&["ls-remote", "--exit-code", url, "HEAD"]);
    if code == 0 || code == 2 {
        Ok(())
    } else {
        Err("无法访问仓库".to_string())
    }
}

/// 把 SSH URL 转为 HTTPS URL。
fn ssh_to_https(ssh_url: &str) -> String {
    // git@github.com:user/repo.git → https://github.com/user/repo.git
    // git@gitee.com:user/repo.git → https://gitee.com/user/repo.git
    let rest = ssh_url.strip_prefix("git@").unwrap_or(ssh_url);
    if let Some((host, path)) = rest.split_once(':') {
        format!("https://{host}/{path}")
    } else {
        ssh_url.to_string()
    }
}

/// 获取某个 remote 的 URL。
fn get_remote_url(dir: &Path, remote: &str) -> Option<String> {
    let (code, stdout, _) = git_exec(dir, &["remote", "get-url", remote]);
    if code == 0 {
        Some(stdout.trim().to_string())
    } else {
        None
    }
}

/// 检查远端是否已有指定分支。
fn remote_has_branch(dir: &Path, config: &GitConfig) -> bool {
    let pattern = format!("refs/heads/{}", config.branch);
    let (code, stdout, _) = git_exec(dir, &["ls-remote", "--exit-code", "origin", &pattern]);
    code == 0 && !stdout.trim().is_empty()
}

/// 克隆仓库到本地工作目录（如已存在则 pull）。
///
/// 空仓库（没有任何 commit）会自动初始化并推送首个 commit。
pub fn ensure_repo(config: &GitConfig) -> GitResult<PathBuf> {
    let dir = repo_work_dir(&config.url);
    let has_local = dir.join(".git").is_dir();

    if has_local {
        // 检查远端 URL 是否一致（用户可能删旧建新）
        let current_url = get_remote_url(&dir, "origin");
        let expected_url = config.plain_url();
        if current_url.as_deref() == Some(expected_url) {
            // URL 一致 → pull（可能失败于空仓库，忽略）
            let _ = pull(&dir, config);
            return Ok(dir);
        }
        // URL 不一致 → 删旧目录，重新走 clone/init 流程
        let _ = fs::remove_dir_all(&dir);
    }

    // 尝试 clone（用 plain_url，认证通过 GIT_ASKPASS 传递）
    let plain = config.plain_url();
    let (code, _, stderr) = git_exec_auth_raw(
        &[
            "clone",
            "--branch",
            &config.branch,
            "--single-branch",
            plain,
            dir.to_str().unwrap_or("."),
        ],
        config,
    );
    if code != 0 {
        // clone 失败 → 可能是空仓库，尝试初始化
        // 中文 git: "远程分支 main 在上游 origin 未发现"
        // 英文 git: "Remote branch main not found"
        let is_empty = stderr.contains("does not exist")
            || stderr.contains("not found")
            || stderr.contains("empty")
            || stderr.contains("Remote branch")
            || stderr.contains("未发现")
            || stderr.contains("不存在");
        if is_empty {
            init_empty_repo(&dir, config)?;
        } else {
            return Err(format!("克隆失败：{stderr}"));
        }
    }
    Ok(dir)
}

/// 初始化空仓库或同步远端已有内容。
///
/// clone 失败后调用此函数。可能的情况：
/// - 空仓库：git init → push
/// - 远端有 commit 但本地没有：git pull → push
fn init_empty_repo(dir: &Path, config: &GitConfig) -> GitResult<()> {
    let _ = fs::create_dir_all(dir);
    let plain = config.plain_url().to_string();

    // 先尝试 pull 远端（可能远端已有 commit）
    let (code, _, _) = git_exec(dir, &["init"]);
    if code == 0 {
        let _ = git_exec_auth(dir, &["remote", "add", "origin", &plain], config);
        let (pull_code, _, _) =
            git_exec_auth(dir, &["pull", "--rebase", "origin", &config.branch], config);
        if pull_code == 0 {
            // pull 成功 → 远端有内容，已经同步到本地
            return Ok(());
        }
        // pull 失败 → 空仓库，继续初始化
    }

    // 确保本地分支名与 config.branch 一致（无 commit 的 repo 默认分支可能是 master）
    ensure_branch(dir, &config.branch)?;
    // 设置 remote（可能已存在）
    let _ = git_exec(dir, &["remote", "remove", "origin"]);
    let _ = git_exec_auth(dir, &["remote", "add", "origin", &plain], config);
    // 设置本地 git 用户信息（commit 需要）
    let _ = git_exec(dir, &["config", "user.name", config.effective_user_name()]);
    let _ = git_exec(
        dir,
        &["config", "user.email", config.effective_user_email()],
    );
    // 创建 .gitignore 避免空仓库无法 commit
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = fs::write(&gitignore, "# linbox notepad sync\n");
    }
    git_exec(dir, &["add", "-A"]);
    let (code, _, stderr) = git_exec(dir, &["commit", "-m", "linbox: initialize notepad sync"]);
    if code != 0 {
        return Err(format!("初始 commit 失败：{stderr}"));
    }
    // push 创建远程分支（带 -u 设置上游）
    let (code, stdout, stderr) =
        git_exec_auth(dir, &["push", "-u", "origin", &config.branch], config);
    if code != 0 {
        return Err(format!("初始 push 失败：{stderr}\n{stdout}"));
    }
    Ok(())
}

/// 获取当前分支名。
///
/// `git init` 后还没有 commit，`rev-parse --abbrev-ref HEAD` 会直接报错，
/// 此时用 `symbolic-ref --short HEAD` 读 HEAD 指向的分支名。
fn get_current_branch(dir: &Path) -> (String, bool) {
    let (code, stdout, _) = git_exec(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let name = stdout.trim();
    if code == 0 && !name.is_empty() && name != "HEAD" {
        return (name.to_string(), true);
    }
    // 尚无 commit（unborn 分支）→ 直接读 HEAD 符号引用
    let (code, stdout, _) = git_exec(dir, &["symbolic-ref", "--short", "HEAD"]);
    let name = stdout.trim();
    if code == 0 && !name.is_empty() {
        return (name.to_string(), true);
    }
    ("".to_string(), false)
}

/// 确保本地 HEAD 落在 `branch` 分支上（不受 git 默认分支名影响）。
///
/// 三级兜底，覆盖「刚 init、还没有任何 commit」的情况：
/// 1. 重命名当前分支（unborn 分支也能重命名）
/// 2. 分支已存在 → 直接切过去
/// 3. 兜底：把 HEAD 符号引用指到目标分支
fn ensure_branch(dir: &Path, branch: &str) -> GitResult<()> {
    let (cur, known) = get_current_branch(dir);
    if known && cur == branch {
        return Ok(());
    }
    // 1) 重命名
    let (code, _, _) = git_exec(dir, &["branch", "-m", branch]);
    if code == 0 {
        return Ok(());
    }
    // 2) 切换
    let (code, _, _) = git_exec(dir, &["checkout", branch]);
    if code == 0 {
        return Ok(());
    }
    // 3) 直接改 HEAD 指向
    let target = format!("refs/heads/{branch}");
    let (code, _, stderr) = git_exec(dir, &["symbolic-ref", "HEAD", &target]);
    if code == 0 {
        return Ok(());
    }
    Err(format!("无法切换到分支 {branch}：{stderr}"))
}

/// Pull 远端变更。
fn pull(dir: &Path, config: &GitConfig) -> GitResult<()> {
    let plain = config.plain_url().to_string();
    // 先设置 remote URL（可能凭证变了，用 plain_url 避免泄露）
    let _ = git_exec_auth(dir, &["remote", "set-url", "origin", &plain], config);
    let (code, _, stderr) =
        git_exec_auth(dir, &["pull", "--rebase", "origin", &config.branch], config);
    if code != 0 {
        return Err(format!("pull 失败：{stderr}"));
    }
    Ok(())
}

/// Commit 并 push 本地变更。
fn commit_and_push(dir: &Path, config: &GitConfig, message: &str) -> GitResult<()> {
    // 保证本地就在 config.branch 上（否则 push 会报「源引用规格 xxx 没有匹配」）
    ensure_branch(dir, &config.branch)?;
    git_exec(dir, &["add", "-A"]);
    // 检查是否有变更
    let (code, stdout, _) = git_exec(dir, &["status", "--porcelain"]);
    let has_changes = code == 0 && !stdout.trim().is_empty();

    if has_changes {
        // 确保本地有 git 用户信息（commit 需要）
        let _ = git_exec(dir, &["config", "user.name", config.effective_user_name()]);
        let _ = git_exec(
            dir,
            &["config", "user.email", config.effective_user_email()],
        );
        let (code, _, stderr) = git_exec(dir, &["commit", "-m", message]);
        if code != 0 && !stderr.contains("nothing to commit") {
            return Err(format!("commit 失败：{stderr}"));
        }
    }

    // 即使没有新 commit，如果远端还没有这个分支，也必须 push
    let need_push = !remote_has_branch(dir, config);
    if !has_changes && !need_push {
        return Ok(()); // 远端已有分支且无新变更，跳过
    }

    let plain = config.plain_url().to_string();
    let _ = git_exec_auth(dir, &["remote", "set-url", "origin", &plain], config);
    // 直接用 -u push，一步到位设置上游
    let (code, stdout, stderr) =
        git_exec_auth(dir, &["push", "-u", "origin", &config.branch], config);
    if code != 0 {
        return Err(format!("push 失败：{stderr}\n{stdout}"));
    }
    // 验证远端确实收到了
    if !remote_has_branch(dir, config) {
        return Err("push 报告成功但远端未找到分支，可能被服务端拒绝".to_string());
    }
    Ok(())
}

// ── 仓库内条目读写 ────────────────────────────────────────────────────────

/// 从 Git 仓库读取所有条目的元信息。
pub fn read_all_entries(dir: &Path) -> Vec<super::storage::MemoMeta> {
    let mut list = Vec::new();
    if !dir.is_dir() {
        return list;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = e.file_name();
            let id = name.to_string_lossy();
            // 跳过非 hex ID 的目录（如 .git）
            if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let meta_path = e.path().join("meta.json");
            if let Ok(text) = fs::read_to_string(&meta_path)
                && let Ok(m) = serde_json::from_str::<super::storage::MemoMeta>(&text)
            {
                list.push(m);
            }
        }
    }
    list.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    list
}

/// 从 Git 仓库读取单个条目。
pub fn read_entry(dir: &Path, id: &str) -> Option<super::storage::MemoMeta> {
    let meta_path = dir.join(id).join("meta.json");
    let text = fs::read_to_string(meta_path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 读取条目正文。
pub fn read_content(dir: &Path, id: &str) -> String {
    fs::read_to_string(dir.join(id).join("content.txt")).unwrap_or_default()
}

/// 读取内嵌图片。
pub fn read_inline(dir: &Path, id: &str, index: usize) -> Option<Vec<u8>> {
    fs::read(dir.join(id).join("inline").join(format!("{index}.png"))).ok()
}

/// 列出内嵌图片文件名。
pub fn list_inline(dir: &Path, id: &str) -> Vec<String> {
    let inline_dir = dir.join(id).join("inline");
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&inline_dir) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
            {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

/// 列出附件文件名。
pub fn list_files(dir: &Path, id: &str) -> Vec<String> {
    let files_dir = dir.join(id).join("files");
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&files_dir) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && let Some(name) = e.file_name().to_str()
            {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

/// 读取附件内容。
pub fn read_file(dir: &Path, id: &str, filename: &str) -> Option<Vec<u8>> {
    fs::read(dir.join(id).join("files").join(filename)).ok()
}

/// 把一个条目写入 Git 仓库目录。
pub fn write_entry(
    dir: &Path,
    id: &str,
    meta: &super::storage::MemoMeta,
    content: &str,
    inline: &[Vec<u8>],
    attachments: &[(String, Vec<u8>)],
) {
    let entry_dir = dir.join(id);
    let _ = fs::create_dir_all(&entry_dir);

    // meta.json
    let _ = fs::write(
        entry_dir.join("meta.json"),
        serde_json::to_string_pretty(meta).unwrap(),
    );

    // content.txt
    let _ = fs::write(entry_dir.join("content.txt"), content);

    // inline/
    let inline_dir = entry_dir.join("inline");
    let _ = fs::remove_dir_all(&inline_dir);
    if !inline.is_empty() {
        let _ = fs::create_dir_all(&inline_dir);
        for (i, data) in inline.iter().enumerate() {
            let _ = fs::write(inline_dir.join(format!("{i}.png")), data);
        }
    }

    // files/
    let files_dir = entry_dir.join("files");
    let _ = fs::remove_dir_all(&files_dir);
    if !attachments.is_empty() {
        let _ = fs::create_dir_all(&files_dir);
        for (name, data) in attachments {
            let _ = fs::write(files_dir.join(name), data);
        }
    }
}

/// 删除条目目录。
pub fn delete_entry(dir: &Path, id: &str) {
    let _ = fs::remove_dir_all(dir.join(id));
}

/// 从 work_dir 读取远端条目（content + inline + attachments），保存到本地存储。
///
/// 返回 true 表示成功应用。用于 sync_git_backend 和 pull_only 中
/// "远端有但本地没有" 和 "远端更新" 两种场景。
fn apply_remote_entry(work_dir: &Path, id: &str, remote_meta: &super::storage::MemoMeta) -> bool {
    let content = read_content(work_dir, id);
    super::storage::save(id, remote_meta, &content);
    // inline images
    let inlines = list_inline(work_dir, id);
    let inline_data: Vec<Vec<u8>> = inlines
        .iter()
        .filter_map(|n| read_inline(work_dir, id, n.parse().unwrap_or(0)))
        .collect();
    super::storage::save_inline(id, &inline_data);
    // attachments
    let files = list_files(work_dir, id);
    for fname in &files {
        if let Some(data) = read_file(work_dir, id, fname) {
            let _ = super::storage::write_file(id, fname, &data);
        }
    }
    true
}

/// 同步一个 Git 后端：pull → 合并 → commit → push。
///
/// 返回 (上传数, 下载数, 错误信息)。
pub fn sync_git_backend(
    config: &GitConfig,
    local_metas: &[super::storage::MemoMeta],
    deleted_ids: &[String],
) -> Result<(usize, usize), String> {
    // 1. 确保仓库在本地
    let work_dir = ensure_repo(config)?;

    // 2. 读取远端所有条目
    let remote_entries = read_all_entries(&work_dir);
    let remote_map: std::collections::HashMap<String, super::storage::MemoMeta> = remote_entries
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();

    // 3. 处理远端删除
    for id in deleted_ids {
        delete_entry(&work_dir, id);
    }

    // 4. 合并：本地 vs 远端，按 modified_at 取最新
    let local_map: std::collections::HashMap<String, &super::storage::MemoMeta> =
        local_metas.iter().map(|m| (m.id.clone(), m)).collect();

    let mut downloaded = 0usize;
    let mut uploaded = 0usize;

    // 远端有但本地没有 → 从 work_dir 读取并保存到本地存储
    for (id, remote_meta) in &remote_map {
        if !local_map.contains_key(id) {
            apply_remote_entry(&work_dir, id, remote_meta);
            downloaded += 1;
        }
    }

    // 远端和本地都有 → 比较 modified_at，取最新的
    for (id, local_meta) in &local_map {
        let local_ts: u64 = local_meta.modified_at.parse().unwrap_or(0);
        if let Some(remote_meta) = remote_map.get(id) {
            let remote_ts: u64 = remote_meta.modified_at.parse().unwrap_or(0);
            if local_ts >= remote_ts {
                // 本地更新 → 写到 Git 仓库
                let content = super::storage::read_content(id);
                let inline: Vec<Vec<u8>> = (0..super::storage::list_inline(id).len())
                    .filter_map(|i| super::storage::read_inline(id, i))
                    .collect();
                let attachments: Vec<(String, Vec<u8>)> = super::storage::list_files(id)
                    .iter()
                    .filter_map(|n| {
                        let data = super::storage::read_file(id, n)?;
                        Some((n.clone(), data))
                    })
                    .collect();
                write_entry(&work_dir, id, local_meta, &content, &inline, &attachments);
                uploaded += 1;
            } else {
                // 远端更新 → 从 work_dir 读取并保存到本地存储
                apply_remote_entry(&work_dir, id, remote_meta);
                downloaded += 1;
            }
        } else {
            // 本地有但远端没有 → 写到 Git 仓库
            let content = super::storage::read_content(id);
            let inline: Vec<Vec<u8>> = (0..super::storage::list_inline(id).len())
                .filter_map(|i| super::storage::read_inline(id, i))
                .collect();
            let attachments: Vec<(String, Vec<u8>)> = super::storage::list_files(id)
                .iter()
                .filter_map(|n| {
                    let data = super::storage::read_file(id, n)?;
                    Some((n.clone(), data))
                })
                .collect();
            write_entry(&work_dir, id, local_meta, &content, &inline, &attachments);
            uploaded += 1;
        }
    }

    // 5. commit + push
    let msg = format!(
        "linbox sync: {} entries, {} deleted",
        local_metas.len(),
        deleted_ids.len()
    );
    commit_and_push(&work_dir, config, &msg)?;

    Ok((uploaded, downloaded))
}

/// 只从远端拉取（刷新）：clone/pull → 合并远端条目到本地存储，不推送。
pub fn pull_only(config: &GitConfig) -> Result<usize, String> {
    let work_dir = ensure_repo(config)?;

    // pull 最新
    pull(&work_dir, config).map_err(|e| format!("pull 失败：{e}"))?;

    // 读取远端所有条目
    let remote_entries = read_all_entries(&work_dir);
    let remote_map: std::collections::HashMap<String, super::storage::MemoMeta> = remote_entries
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();

    // 读取本地条目
    let local_metas = super::storage::load_all();
    let local_map: std::collections::HashMap<String, &super::storage::MemoMeta> =
        local_metas.iter().map(|m| (m.id.clone(), m)).collect();

    let mut downloaded = 0usize;

    // 远端有但本地没有 → 从 work_dir 下载到本地存储
    for (id, remote_meta) in &remote_map {
        if !local_map.contains_key(id) {
            apply_remote_entry(&work_dir, id, remote_meta);
            downloaded += 1;
        }
    }

    // 远端和本地都有 → 比较 modified_at，远端更新则覆盖本地
    for (id, remote_meta) in &remote_map {
        if let Some(local_meta) = local_map.get(id) {
            let remote_ts: u64 = remote_meta.modified_at.parse().unwrap_or(0);
            let local_ts: u64 = local_meta.modified_at.parse().unwrap_or(0);
            if remote_ts > local_ts {
                apply_remote_entry(&work_dir, id, remote_meta);
                downloaded += 1;
            }
        }
    }

    Ok(downloaded)
}

/// 只推送本地到远端：clone/pull → 写入本地条目 → commit+push，不下载。
pub fn push_only(
    config: &GitConfig,
    local_metas: &[super::storage::MemoMeta],
    deleted_ids: &[String],
) -> Result<usize, String> {
    let work_dir = ensure_repo(config)?;

    // 先 pull 避免冲突
    let _ = pull(&work_dir, config);

    // 处理远端删除
    for id in deleted_ids {
        delete_entry(&work_dir, id);
    }

    // 写入所有本地条目到 git 仓库
    for local_meta in local_metas {
        let content = super::storage::read_content(&local_meta.id);
        let inline: Vec<Vec<u8>> = (0..super::storage::list_inline(&local_meta.id).len())
            .filter_map(|i| super::storage::read_inline(&local_meta.id, i))
            .collect();
        let attachments: Vec<(String, Vec<u8>)> = super::storage::list_files(&local_meta.id)
            .iter()
            .filter_map(|n| {
                let data = super::storage::read_file(&local_meta.id, n)?;
                Some((n.clone(), data))
            })
            .collect();
        write_entry(
            &work_dir,
            &local_meta.id,
            local_meta,
            &content,
            &inline,
            &attachments,
        );
    }

    // commit + push
    let msg = format!(
        "linbox push: {} entries, {} deleted",
        local_metas.len(),
        deleted_ids.len()
    );
    commit_and_push(&work_dir, config, &msg)?;

    Ok(local_metas.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> GitConfig {
        GitConfig {
            url: url.to_string(),
            branch: "main".to_string(),
            user_name: String::new(),
            user_email: String::new(),
            username: String::new(),
            password: String::new(),
        }
    }

    #[test]
    fn default_branch_is_main() {
        assert_eq!(default_branch(), "main");
    }

    #[test]
    fn effective_user_name_defaults_and_custom() {
        let c = cfg("https://github.com/u/r.git");
        assert_eq!(c.effective_user_name(), "linbox");
        let mut c2 = c.clone();
        c2.user_name = "张三".into();
        assert_eq!(c2.effective_user_name(), "张三");
    }

    #[test]
    fn effective_user_email_defaults_and_custom() {
        let c = cfg("https://github.com/u/r.git");
        assert_eq!(c.effective_user_email(), "linbox@notepad.local");
        let mut c2 = c.clone();
        c2.user_email = "me@example.com".into();
        assert_eq!(c2.effective_user_email(), "me@example.com");
    }

    #[test]
    fn auth_url_embeds_credentials_for_https() {
        let mut c = cfg("https://github.com/u/r.git");
        c.username = "alice".into();
        c.password = "s3cr3t".into();
        assert_eq!(c.auth_url(), "https://alice:s3cr3t@github.com/u/r.git");
    }

    #[test]
    fn auth_url_plain_without_username() {
        let c = cfg("https://github.com/u/r.git");
        assert_eq!(c.auth_url(), "https://github.com/u/r.git");
    }

    #[test]
    fn auth_url_plain_for_non_https() {
        // 非 https（http:// 或 ssh）都不嵌凭证
        let mut c = cfg("http://git.local/u/r.git");
        c.username = "bob".into();
        assert_eq!(c.auth_url(), "http://git.local/u/r.git");
        let mut s = cfg("git@github.com:u/r.git");
        s.username = "bob".into();
        assert_eq!(s.auth_url(), "git@github.com:u/r.git");
    }

    #[test]
    fn needs_askpass_only_https_with_user() {
        let mut c = cfg("https://github.com/u/r.git");
        assert!(!c.needs_askpass());
        c.username = "u".into();
        assert!(c.needs_askpass());
        let mut h = cfg("http://x/y.git");
        h.username = "u".into();
        assert!(!h.needs_askpass(), "http 不走 askpass");
        let mut ssh = cfg("git@github.com:u/r.git");
        ssh.username = "u".into();
        assert!(!ssh.needs_askpass(), "ssh 不走 askpass");
    }

    #[test]
    fn plain_url_never_contains_credentials() {
        let mut c = cfg("https://github.com/u/r.git");
        c.username = "alice".into();
        c.password = "s3cr3t".into();
        assert_eq!(c.plain_url(), "https://github.com/u/r.git");
    }

    #[test]
    fn serde_defaults_for_missing_fields() {
        let c: GitConfig = serde_json::from_str(r#"{"url":"https://github.com/u/r.git"}"#).unwrap();
        assert_eq!(c.branch, "main", "branch 缺省应为 main");
        assert_eq!(c.user_name, "");
        assert_eq!(c.user_email, "");
        assert_eq!(c.username, "");
        assert_eq!(c.password, "");
    }

    #[test]
    fn serde_roundtrip_preserves_all_fields() {
        let mut c = cfg("https://github.com/u/r.git");
        c.branch = "dev".into();
        c.user_name = "n".into();
        c.user_email = "e@x".into();
        c.username = "u".into();
        c.password = "p".into();
        let s = serde_json::to_string(&c).unwrap();
        let back: GitConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn repo_work_dir_is_stable_hex_named() {
        let url = "https://github.com/u/stable-test.git";
        let a = repo_work_dir(url);
        let b = repo_work_dir(url);
        assert_eq!(a, b, "同 URL 必须映射到同一目录");
        let other = repo_work_dir("https://github.com/u/other.git");
        assert_ne!(a, other, "不同 URL 不能撞目录");
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 16, "目录名应为16 位十六进制 hash");
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(a.to_string_lossy().contains("git-repos"));
    }
}
