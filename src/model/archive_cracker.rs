//! 压缩包密码爆破模块的数据模型（纯数据，无 UI 依赖）。
//!
//! 支持 zip 和 7z 格式，仅爆破密码不解压，纯 Rust 实现无系统依赖。

use serde::{Deserialize, Serialize};

/// 字典类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DictType {
    /// 纯数字 (0-9)
    Digits,
    /// 纯小写 (a-z)
    Lowercase,
    /// 纯大写 (A-Z)
    Uppercase,
    /// 特殊符号 (!@#$%^&*...)
    Special,
    /// 数字+小写
    DigitsLower,
    /// 数字+大写
    DigitsUpper,
    /// 小写+大写
    LowerUpper,
    /// 数字+小写+大写
    DigitsLowerUpper,
    /// 数字+小写+大写+特殊（全部混合）
    Mixed,
}

impl DictType {
    pub fn label(&self) -> &'static str {
        match self {
            DictType::Digits => "纯数字 (0-9)",
            DictType::Lowercase => "纯小写 (a-z)",
            DictType::Uppercase => "纯大写 (A-Z)",
            DictType::Special => "特殊符号",
            DictType::DigitsLower => "数字+小写",
            DictType::DigitsUpper => "数字+大写",
            DictType::LowerUpper => "小写+大写",
            DictType::DigitsLowerUpper => "数字+小写+大写",
            DictType::Mixed => "全部混合",
        }
    }

    /// 各类型下拉顺序。
    pub const ALL: &'static [DictType] = &[
        DictType::Digits,
        DictType::Lowercase,
        DictType::Uppercase,
        DictType::Special,
        DictType::DigitsLower,
        DictType::DigitsUpper,
        DictType::LowerUpper,
        DictType::DigitsLowerUpper,
        DictType::Mixed,
    ];

    /// 返回该类型对应的字符集。
    pub fn charset(&self) -> Vec<u8> {
        match self {
            DictType::Digits => b"0123456789".to_vec(),
            DictType::Lowercase => (b'a'..=b'z').collect(),
            DictType::Uppercase => (b'A'..=b'Z').collect(),
            DictType::Special => b"!@#$%^&*()-_=+[]{}|;:,.<>?/~`".to_vec(),
            DictType::DigitsLower => {
                let mut v = (b'0'..=b'9').collect::<Vec<u8>>();
                v.extend(b'a'..=b'z');
                v
            }
            DictType::DigitsUpper => {
                let mut v = (b'0'..=b'9').collect::<Vec<u8>>();
                v.extend(b'A'..=b'Z');
                v
            }
            DictType::LowerUpper => {
                let mut v = (b'a'..=b'z').collect::<Vec<u8>>();
                v.extend(b'A'..=b'Z');
                v
            }
            DictType::DigitsLowerUpper => {
                let mut v = (b'0'..=b'9').collect::<Vec<u8>>();
                v.extend(b'a'..=b'z');
                v.extend(b'A'..=b'Z');
                v
            }
            DictType::Mixed => {
                let mut v = (b'0'..=b'9').collect::<Vec<u8>>();
                v.extend(b'a'..=b'z');
                v.extend(b'A'..=b'Z');
                v.extend(b"!@#$%^&*()-_=+[]{}|;:,.<>?/~`");
                v
            }
        }
    }
}

/// 字典生成配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictConfig {
    /// 字典类型。
    pub dict_type: DictType,
    /// 最小密码长度。
    pub min_len: usize,
    /// 最大密码长度。
    pub max_len: usize,
}

impl Default for DictConfig {
    fn default() -> Self {
        DictConfig {
            dict_type: DictType::Digits,
            min_len: 1,
            max_len: 6,
        }
    }
}

impl DictConfig {
    /// 计算单个长度的候选数。
    pub fn candidates_for_len(&self, len: usize) -> u128 {
        self.dict_type.charset().len().pow(len as u32) as u128
    }

    /// 计算总候选数。
    pub fn total_candidates(&self) -> u128 {
        let base = self.dict_type.charset().len() as u128;
        (self.min_len..=self.max_len)
            .map(|l| base.pow(l as u32))
            .sum()
    }

    /// 估算内存占用（字节）。
    pub fn estimate_memory(&self) -> u128 {
        let total = self.total_candidates();
        let avg_len = ((self.min_len + self.max_len) / 2) as u128;
        // Vec<String>: pointer + len + cap per String (24 bytes on 64-bit) + 字符内容
        total * (avg_len + 24)
    }
}

/// 压缩包格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveFormat {
    Zip,
    SevenZip,
}

impl ArchiveFormat {
    pub fn label(&self) -> &'static str {
        match self {
            ArchiveFormat::Zip => "ZIP",
            ArchiveFormat::SevenZip => "7Z",
        }
    }

    /// 根据文件扩展名推断格式。
    pub fn from_path(path: &str) -> Option<ArchiveFormat> {
        let lower = path.to_lowercase();
        if lower.ends_with(".zip") {
            Some(ArchiveFormat::Zip)
        } else if lower.ends_with(".7z") {
            Some(ArchiveFormat::SevenZip)
        } else {
            None
        }
    }
}

/// 单个压缩包的爆破配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveConfig {
    /// 文件路径。
    pub path: String,
    /// 文件格式。
    pub format: ArchiveFormat,
    /// 是否使用全局字典设置。
    pub use_global_dict: bool,
    /// 自定义字典设置（use_global_dict = false 时使用）。
    pub dict: DictConfig,
    /// 文件大小（字节，仅展示）。
    pub file_size: u64,
    /// 显示用文件名。
    pub file_name: String,
}

impl ArchiveConfig {
    pub fn new(path: &str) -> Option<Self> {
        let format = ArchiveFormat::from_path(path)?;
        let file_name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Some(ArchiveConfig {
            path: path.to_string(),
            format,
            use_global_dict: true,
            dict: DictConfig::default(),
            file_size,
            file_name,
        })
    }
}

/// 单个压缩包的爆破结果。
#[derive(Debug, Clone)]
pub struct CrackResult {
    pub path: String,
    pub file_name: String,
    pub password: Option<String>,
    pub tested: usize,
    pub elapsed_secs: u64,
    pub error: Option<String>,
}

/// 扫描事件。
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// 全局开始。
    Started {
        archives: usize,
        total_candidates: u128,
    },
    /// 某个压缩包找到密码。
    Found {
        path: String,
        password: String,
        tested: usize,
        elapsed_secs: u64,
    },
    /// 某个压缩包爆破完毕（未找到）。
    Exhausted {
        path: String,
        tested: usize,
        elapsed_secs: u64,
    },
    /// 某个压缩包出错。
    Error { path: String, message: String },
    /// 日志。
    Log(String),
    /// 全部完成。
    Finished { found: usize, total: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dict_type_labels_and_all() {
        assert_eq!(DictType::Digits.label(), "纯数字 (0-9)");
        assert_eq!(DictType::Mixed.label(), "全部混合");
        assert_eq!(DictType::ALL.len(), 9);
        for t in DictType::ALL {
            assert!(!t.label().is_empty());
        }
    }

    #[test]
    fn charset_sizes_and_order() {
        assert_eq!(DictType::Digits.charset().len(), 10);
        assert_eq!(DictType::Lowercase.charset().len(), 26);
        assert_eq!(DictType::Uppercase.charset().len(), 26);
        assert_eq!(DictType::DigitsLower.charset().len(), 36);
        assert_eq!(DictType::DigitsUpper.charset().len(), 36);
        assert_eq!(DictType::LowerUpper.charset().len(), 52);
        assert_eq!(DictType::DigitsLowerUpper.charset().len(), 62);
        // 组合类型顺序：数字 → 小写 → 大写 → 特殊
        let dl = DictType::DigitsLower.charset();
        assert_eq!(&dl[..10], b"0123456789");
        assert_eq!(&dl[10..], b"abcdefghijklmnopqrstuvwxyz");
        let m = DictType::Mixed.charset();
        let sp = DictType::Special.charset();
        assert_eq!(m.len(), 10 + 26 + 26 + sp.len(), "混合=数字+小写+大写+特殊");
        assert_eq!(&m[..10], b"0123456789");
        assert_eq!(&m[10..36], b"abcdefghijklmnopqrstuvwxyz");
        assert_eq!(&m[36..62], b"ABCDEFGHIJKLMNOPQRSTUVWXYZ");
        assert_eq!(&m[62..], &sp[..], "特殊符号必须完整在末尾");
    }

    #[test]
    fn candidates_math() {
        let c = DictConfig {
            dict_type: DictType::Digits,
            min_len: 3,
            max_len: 3,
        };
        assert_eq!(c.candidates_for_len(3), 1000);
        let c = DictConfig {
            dict_type: DictType::Lowercase,
            min_len: 2,
            max_len: 2,
        };
        assert_eq!(c.candidates_for_len(2), 676);
        // 默认：数字 1..6 位 = 10+100+…+1,000,000
        let d = DictConfig::default();
        assert_eq!(d.total_candidates(), 1_111_110);
        // 内存估算 = total * (avg_len + 24)
        assert_eq!(d.estimate_memory(), 1_111_110 * (3 + 24));
    }

    #[test]
    fn archive_format_from_path() {
        assert_eq!(
            ArchiveFormat::from_path("/a/b.zip"),
            Some(ArchiveFormat::Zip)
        );
        assert_eq!(
            ArchiveFormat::from_path("/a/b.ZIP"),
            Some(ArchiveFormat::Zip),
            "大小写不敏感"
        );
        assert_eq!(
            ArchiveFormat::from_path("x.7z"),
            Some(ArchiveFormat::SevenZip)
        );
        assert_eq!(
            ArchiveFormat::from_path("x.7Z"),
            Some(ArchiveFormat::SevenZip)
        );
        assert_eq!(ArchiveFormat::from_path("x.rar"), None);
        assert_eq!(ArchiveFormat::from_path("noext"), None);
        assert_eq!(ArchiveFormat::Zip.label(), "ZIP");
        assert_eq!(ArchiveFormat::SevenZip.label(), "7Z");
    }

    #[test]
    fn archive_config_rejects_unsupported() {
        assert!(
            ArchiveConfig::new("/tmp/no-such-thing.rar").is_none(),
            "rar 不在支持范围，必须返回 None"
        );
        assert!(ArchiveConfig::new("/tmp/no-ext").is_none());
    }

    #[test]
    fn archive_config_new_fields() {
        // 文件不存在时也能构造：file_size=0、file_name=路径末段
        let c = ArchiveConfig::new("/nonexistent/dir/secret.7z").expect("7z 应识别");
        assert_eq!(c.format, ArchiveFormat::SevenZip);
        assert_eq!(c.file_name, "secret.7z");
        assert_eq!(c.file_size, 0);
        assert!(c.use_global_dict, "默认走全局字典");
        assert_eq!(c.path, "/nonexistent/dir/secret.7z");
    }
}
