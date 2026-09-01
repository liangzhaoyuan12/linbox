//! 输入法修复相关的数据模型（纯数据，无 UI 依赖）。
//!
//! 参考 `docs/项目结构规划书.md` §3.8：`model/` 仅定义数据，`utils` 读写 `model`，
//! `page` 展示 `model`。

/// 需要写入 `/etc/environment` 的输入法环境变量（变量名, 推荐值）。
///
/// 顺序即写入顺序；`XMODIFIERS` 的值含 `@`，但写入走字节流不经 shell 解释。
pub const REQUIRED: &[(&str, &str)] = &[
    ("GTK_IM_MODULE", "fcitx"),
    ("QT_IM_MODULE", "fcitx"),
    ("XMODIFIERS", "@im=fcitx"),
    ("INPUT_METHOD", "fcitx"),
    ("SDL_IM_MODULE", "fcitx"),
    ("GLFW_IM_MODULE", "fcitx"),
    ("XIM", "fcitx"),
];

/// 一次输入法环境检测的结论。
#[derive(Clone, Debug, Default)]
pub struct ImfixReport {
    /// 系统是否安装 `fcitx5`。
    pub fcitx_installed: bool,
    /// 已配置的输入法变量数量。
    pub configured: usize,
    /// 需要配置的变量总数。
    pub total: usize,
    /// 缺失的变量（变量名, 推荐值），即需要追加写入的项。
    pub missing: Vec<(String, String)>,
}
