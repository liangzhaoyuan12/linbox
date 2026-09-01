//! 硬件加速能力探测（逻辑层 · 无 GTK 依赖）。
//!
//! 探测流程（呼应规划书 §3.2）：
//! 1. 检查 `/dev/dri/render*`（VAAPI）、`/dev/nvidia*`（NVENC）等设备节点；
//! 2. 运行 `ffmpeg -hide_banner -encoders` 并检索 `nvenc` / `vaapi` / `qsv` / `amf`；
//! 3. 解析 `ffmpeg -hwaccels` 得到解码加速列表。
//!
//! 综合生成能力矩阵。纯探测逻辑可在无显示器环境下运行（依赖系统 ffmpeg）。
//!
//! ## 结果持久化
//! 探测需要启动两次 ffmpeg 进程，而用户机器的硬件不会频繁变动，因此探测结果
//! 会被**永久缓存**到 `$XDG_CONFIG_HOME/linbox/hwaccel.json`：
//! - 启动时直接读取缓存，不再重复探测（下拉选项本身是固定全量列表，与缓存无关）；
//! - 只有用户手动点击「刷新」时才重新探测并覆盖缓存。

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// 单个加速器是否可用。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HwCapabilities {
    pub nvenc: bool,
    pub vaapi: bool,
    pub qsv: bool,
    pub amf: bool,
    /// macOS VideoToolbox 可用。
    pub videotoolbox: bool,
    /// `ffmpeg -hwaccels` 列出的全部解码加速方法。
    pub decode_methods: Vec<String>,
    /// 设备节点探测结果（用于诊断提示）。
    pub has_dri_render: bool,
    pub has_nvidia: bool,
}

/// 带时间戳的探测结果缓存（永久保存到磁盘）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HwCache {
    /// 探测时刻（UNIX 秒）。
    pub detected_at: u64,
    /// 探测结果。
    pub caps: HwCapabilities,
}

impl HwCapabilities {
    /// 根据探测结果给出一个默认的硬件加速偏好（自动选择规则：
    /// NVENC > VAAPI > 软件编码）。无可用硬件时返回 `Software`。
    pub fn auto_preference(&self) -> crate::model::media::HwAccelPreference {
        if self.nvenc {
            crate::model::media::HwAccelPreference::Nvenc
        } else if self.vaapi {
            crate::model::media::HwAccelPreference::Vaapi
        } else if self.qsv {
            crate::model::media::HwAccelPreference::Qsv
        } else         if self.amf {
            crate::model::media::HwAccelPreference::Amf
        } else if self.videotoolbox {
            crate::model::media::HwAccelPreference::Videotoolbox
        } else {
            crate::model::media::HwAccelPreference::Software
        }
    }

    /// 探测到的可用后端汇总（用于界面状态展示）。
    ///
    /// 先列编码后端（NVENC / VAAPI / QSV / AMF / VideoToolbox），
    /// 再列仅解码可用的方法（CUDA / DXVA2 / D3D11VA / Vulkan / OpenCL）。
    pub fn summary(&self) -> String {
        let mut names: Vec<String> = Vec::new();
        if self.nvenc {
            names.push("NVENC".to_string());
        }
        if self.vaapi {
            names.push("VAAPI".to_string());
        }
        if self.qsv {
            names.push("QSV".to_string());
        }
        if self.amf {
            names.push("AMF".to_string());
        }
        if self.videotoolbox {
            names.push("VideoToolbox".to_string());
        }
        for m in &self.decode_methods {
            let label = match m.as_str() {
                "cuda" => Some("CUDA 解码"),
                "dxva2" => Some("DXVA2 解码"),
                "d3d11va" => Some("D3D11VA 解码"),
                "vulkan" => Some("Vulkan 解码"),
                "opencl" => Some("OpenCL 解码"),
                _ => None,
            };
            if let Some(l) = label {
                if !names.iter().any(|n| n.eq_ignore_ascii_case(l)) {
                    names.push(l.to_string());
                }
            }
        }
        if names.is_empty() {
            "未检测到可用硬件加速（将使用软件编码）".to_string()
        } else {
            format!("可用：{}", names.join("、"))
        }
    }
}

// ---------- 探测结果持久化 ----------

/// 当前 UNIX 时间戳（秒）。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 缓存文件路径：`$XDG_CONFIG_HOME/linbox/hwaccel.json`
/// （未设置 `XDG_CONFIG_HOME` 时回落到 `~/.config/linbox/hwaccel.json`）。
fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| {
                let mut p = PathBuf::from(h);
                p.push(".config");
                p
            })
        });
    match base {
        Some(mut p) => {
            p.push("linbox");
            p.push("hwaccel.json");
            p
        }
        None => PathBuf::from("hwaccel.json"),
    }
}

/// 读取上次探测结果（文件不存在或内容损坏时返回 `None`）。
pub fn load_cached() -> Option<HwCache> {
    let s = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str::<HwCache>(&s).ok()
}

/// 把探测结果永久写入缓存，返回带时间戳的缓存（写入失败也返回缓存，供本次会话使用）。
pub fn save_cache(caps: &HwCapabilities) -> HwCache {
    let cache = HwCache {
        detected_at: now_secs(),
        caps: caps.clone(),
    };
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(path, s);
    }
    cache
}

/// 距 `detected_at`（UNIX 秒）的相对时间描述；不依赖时区。
pub fn age_text(detected_at: u64) -> String {
    let now = now_secs();
    if detected_at == 0 || detected_at > now {
        return "未知时间".to_string();
    }
    let d = now - detected_at;
    if d < 60 {
        "刚刚".to_string()
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86400)
    }
}

/// 探测系统硬件加速能力。应在后台线程调用。
pub fn detect() -> HwCapabilities {
    let has_dri_render = std::fs::read_dir("/dev")
        .map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("render")))
        .unwrap_or(false)
        || std::path::Path::new("/dev/dri/renderD128").exists();

    let has_nvidia = std::path::Path::new("/dev/nvidia0").exists()
        || std::fs::read_dir("/dev")
            .map(|d| d.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().starts_with("nvidia")))
            .unwrap_or(false);

    let encoders = run_ffmpeg_encoders();
    let enc_nvenc = encoders.iter().any(|e| e.contains("nvenc"));
    let enc_vaapi = encoders.iter().any(|e| e.contains("vaapi"));
    let enc_qsv = encoders.iter().any(|e| e.contains("_qsv"));
    let enc_amf = encoders.iter().any(|e| e.contains("_amf"));

    let decode_methods = run_ffmpeg_hwaccels();
    let videotoolbox = decode_methods.iter().any(|m| m.as_str() == "videotoolbox");

    HwCapabilities {
        // 真正可用 = 编码器已编译「且」对应硬件设备节点存在。
        // 否则 `ffmpeg -encoders` 里几乎都会列出 nvenc/vaapi，
        // 会让「自动选择」误判为 NVENC（cuda），在无 N 卡机器上生成跑不起来的命令。
        nvenc: has_nvidia && enc_nvenc,
        vaapi: has_dri_render && enc_vaapi,
        qsv: has_dri_render && enc_qsv,
        amf: enc_amf,
        videotoolbox,
        decode_methods,
        has_dri_render,
        has_nvidia,
    }
}

fn run_ffmpeg_encoders() -> Vec<String> {
    Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-encoders")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

fn run_ffmpeg_hwaccels() -> Vec<String> {
    Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-hwaccels")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .skip_while(|l| !l.contains("Hardware acceleration methods"))
                .skip(1)
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_preference_falls_back_to_software() {
        let caps = HwCapabilities::default();
        assert_eq!(caps.auto_preference(), crate::model::media::HwAccelPreference::Software);
    }
}
