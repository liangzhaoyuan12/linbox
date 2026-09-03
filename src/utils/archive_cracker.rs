//! 压缩包密码爆破模块的逻辑层（纯逻辑，无 GTK 依赖）。
//!
//! 子模块：
//! - [`mod.rs`]：字典生成 + zip/7z 密码验证 + 并发扫描引擎。
//!
//! 页面层（`page::archive_cracker`）负责把这些能力拼成界面。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::model::archive_cracker::{ArchiveConfig, ArchiveFormat, DictConfig, ScanEvent};

// ---------------------------------------------------------------------------
// 字典生成：on-the-fly，不预生成全量候选
// ---------------------------------------------------------------------------

/// 字典描述（用于 on-the-fly 密码生成，零预分配内存）。
pub struct Dictionary {
    charset: Vec<u8>,
    charset_len: usize,
    min_len: usize,
    max_len: usize,
    /// 各长度的起始索引（prefix sum）。
    offsets: Vec<u128>,
    total: u128,
}

impl Dictionary {
    pub fn new(config: &DictConfig) -> Self {
        let charset = config.dict_type.charset();
        let charset_len = charset.len();
        let mut offsets = Vec::with_capacity(config.max_len - config.min_len + 1);
        let mut cumulative = 0u128;
        for l in config.min_len..=config.max_len {
            offsets.push(cumulative);
            cumulative += charset_len.pow(l as u32) as u128;
        }
        Dictionary {
            charset,
            charset_len,
            min_len: config.min_len,
            max_len: config.max_len,
            offsets,
            total: cumulative,
        }
    }

    /// 字典总候选数。
    pub fn total(&self) -> u128 {
        self.total
    }

    /// 由全局索引计算密码字符串。
    pub fn password_at(&self, index: u128) -> String {
        // 确定该索引落在哪个长度区间
        let mut local = index;
        let mut length = self.min_len;
        for (i, &off) in self.offsets.iter().enumerate() {
            let count = self.charset_len.pow((self.min_len + i) as u32) as u128;
            if local < count {
                length = self.min_len + i;
                break;
            }
            local -= count;
        }
        // local 是该长度内的序号，用 base-N 展开
        let n = self.charset_len as u128;
        let mut buf = vec![0u8; length];
        let mut rem = local;
        for i in (0..length).rev() {
            buf[i] = self.charset[(rem % n) as usize];
            rem /= n;
        }
        // SAFETY: charset 只含 ASCII 字节，String::from_utf8_unchecked 安全
        unsafe { String::from_utf8_unchecked(buf) }
    }
}

// ---------------------------------------------------------------------------
// zip 密码验证
// ---------------------------------------------------------------------------

fn verify_zip(path: &str, password: &str) -> Result<bool, String> {
    use std::fs::File;
    use std::io::Read;
    use zip::read::ZipArchive;

    let file = File::open(path).map_err(|e| format!("打开 zip 失败：{e}"))?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| format!("解析 zip 失败：{e}"))?;

    if archive.is_empty() {
        return Err("zip 压缩包为空".into());
    }

    // 找到第一个非目录的加密条目
    let mut target_index = None;
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            if !entry.is_dir() && entry.encrypted() {
                target_index = Some(i);
                break;
            }
        }
    }

    let idx = match target_index {
        Some(i) => i,
        None => return Err("zip 中无加密文件条目".into()),
    };

    // 用密码解密该条目，读取触发解密 + CRC 校验
    let mut entry = archive
        .by_index_decrypt(idx, password.as_bytes())
        .map_err(|e| format!("解密条目失败：{e}"))?;

    let mut buf = [0u8; 1024];
    match entry.read(&mut buf) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// 7z 密码验证
// ---------------------------------------------------------------------------

fn verify_7z(path: &str, password: &str) -> Result<bool, String> {
    use std::io::Cursor;
    use sevenz_rust2::{ArchiveReader, Password};

    let data = std::fs::read(path).map_err(|e| format!("读取 7z 文件失败：{e}"))?;
    let pwd = Password::from(password);
    let cursor = Cursor::new(&data);

    let mut reader = ArchiveReader::new(cursor, pwd)
        .map_err(|e| format!("打开 7z 失败：{e}"))?;

    let mut found_file = false;
    let mut password_ok = true;

    let result = reader.for_each_entries(|_entry, reader| {
        found_file = true;
        let mut buf = [0u8; 512];
        // 尝试读取解密数据
        match reader.read(&mut buf) {
            Ok(_) => Ok(true),
            Err(_) => {
                password_ok = false;
                Ok(false) // 停止迭代
            }
        }
    });

    match result {
        Ok(_) => {
            if !found_file {
                Err("7z 压缩包为空".into())
            } else {
                Ok(password_ok)
            }
        }
        Err(e) => {
            let msg = format!("{e}");
            // 7z 库在密码错误时可能在 header 阶段就报错
            if msg.contains("password") || msg.contains("decrypt") || msg.contains("bad")
                || msg.contains("wrong") || msg.contains("invalid")
                || msg.contains("Decryption")
            {
                Ok(false)
            } else {
                Err(format!("7z 解析错误：{msg}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 密码验证入口
// ---------------------------------------------------------------------------

/// 对指定压缩包尝试一个密码。返回 Ok(true) = 密码正确，Ok(false) = 密码错误。
pub fn verify_password(archive: &ArchiveConfig, password: &str) -> Result<bool, String> {
    match archive.format {
        ArchiveFormat::Zip => verify_zip(&archive.path, password),
        ArchiveFormat::SevenZip => verify_7z(&archive.path, password),
    }
}

// ---------------------------------------------------------------------------
// 并发扫描引擎
// ---------------------------------------------------------------------------

/// 扫描控制句柄。
pub struct Control {
    stop: Arc<AtomicBool>,
}

impl Control {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 扫描参数。
pub struct ScanParams {
    /// 待爆破的压缩包列表。
    pub archives: Vec<ArchiveConfig>,
    /// 各压缩包对应的字典（已根据 use_global_dict 合并）。
    pub dictionaries: Vec<Dictionary>,
    /// 并发线程数。
    pub concurrency: usize,
}

struct Shared {
    cursor: AtomicUsize,
    completed: AtomicUsize,
    found: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// 启动扫描，立即返回控制句柄。
pub fn start(params: ScanParams, tx: Sender<ScanEvent>) -> Control {
    let total = params
        .dictionaries
        .iter()
        .map(|d| d.total() as usize)
        .sum::<usize>();
    let archive_count = params.archives.len();

    let _ = tx.send(ScanEvent::Started {
        archives: archive_count,
        total_candidates: total as u128,
    });

    let shared = Arc::new(Shared {
        cursor: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        found: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
    });

    let control = Control {
        stop: Arc::clone(&shared.stop),
    };

    let concurrency = params.concurrency.clamp(1, 64);
    let archives = Arc::new(params.archives);
    let dictionaries = Arc::new(params.dictionaries);

    for _ in 0..concurrency {
        let shared = Arc::clone(&shared);
        let archives = Arc::clone(&archives);
        let dictionaries = Arc::clone(&dictionaries);
        let tx = tx.clone();
        std::thread::spawn(move || {
            worker(&shared, &archives, &dictionaries, &tx);
        });
    }

    // 监控线程：定期检查是否所有压缩包都已完成
    let shared2 = Arc::clone(&shared);
    let archives2 = Arc::clone(&archives);
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        monitor_loop(&shared2, &archives2, archive_count, &tx2);
    });

    control
}

/// 单个工作线程：遍历所有压缩包，对每个用共享游标抢候选。
fn worker(
    shared: &Shared,
    archives: &[ArchiveConfig],
    dictionaries: &[Dictionary],
    tx: &Sender<ScanEvent>,
) {
    // 每个线程独立：逐个压缩包处理
    for (archive, dict) in archives.iter().zip(dictionaries.iter()) {
        if shared.stop.load(Ordering::Relaxed) || shared.found.load(Ordering::Relaxed) {
            break;
        }
        crack_archive(shared, archive, dict, tx);
    }
}

/// 对单个压缩包进行爆破。
fn crack_archive(
    shared: &Shared,
    archive: &ArchiveConfig,
    dict: &Dictionary,
    tx: &Sender<ScanEvent>,
) {
    let total = dict.total() as usize;
    let started = Instant::now();
    let mut tested_local = 0usize;

    loop {
        if shared.stop.load(Ordering::Relaxed) || shared.found.load(Ordering::Relaxed) {
            break;
        }

        let index = shared.cursor.fetch_add(1, Ordering::Relaxed);
        if index >= total {
            break;
        }

        let password = dict.password_at(index as u128);
        let result = verify_password(archive, &password);

        tested_local += 1;
        shared.completed.fetch_add(1, Ordering::Relaxed);

        match result {
            Ok(true) => {
                shared.found.store(true, Ordering::Relaxed);
                let _ = tx.send(ScanEvent::Found {
                    path: archive.path.clone(),
                    password,
                    tested: tested_local,
                    elapsed_secs: started.elapsed().as_secs(),
                });
                return;
            }
            Ok(false) => { /* 密码错误，继续 */ }
            Err(e) => {
                let _ = tx.send(ScanEvent::Error {
                    path: archive.path.clone(),
                    message: e,
                });
                // 出错的压缩包跳过后续候选
                return;
            }
        }
    }

    // 跑完所有候选
    if !shared.found.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        let _ = tx.send(ScanEvent::Exhausted {
            path: archive.path.clone(),
            tested: tested_local,
            elapsed_secs: started.elapsed().as_secs(),
        });
    }
}

/// 监控线程：等待所有候选跑完后发送 Finished 事件。
fn monitor_loop(
    shared: &Shared,
    archives: &[ArchiveConfig],
    archive_count: usize,
    tx: &Sender<ScanEvent>,
) {
    let total: usize = archives
        .iter()
        .enumerate()
        .map(|(i, _)| {
            // 需要字典的 total，但这里没有字典引用
            // 改用 shared.completed vs 估算总候选数
            0usize
        })
        .sum();
    // 简单方案：等 shared.completed 停止增长
    let mut last_completed = 0usize;
    let mut stable_count = 0u32;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if shared.stop.load(Ordering::Relaxed) || shared.found.load(Ordering::Relaxed) {
            break;
        }
        let current = shared.completed.load(Ordering::Relaxed);
        if current == last_completed {
            stable_count += 1;
            if stable_count >= 4 {
                // 2 秒无增长，视为完成
                break;
            }
        } else {
            stable_count = 0;
            last_completed = current;
        }
    }

    let _ = tx.send(ScanEvent::Finished {
        found: if shared.found.load(Ordering::Relaxed) { 1 } else { 0 },
        total: shared.completed.load(Ordering::Relaxed),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_generates_correct_passwords() {
        let config = DictConfig {
            dict_type: crate::model::archive_cracker::DictType::Digits,
            min_len: 1,
            max_len: 3,
        };
        let dict = Dictionary::new(&config);
        assert_eq!(dict.total(), 10 + 100 + 1000); // 1110

        assert_eq!(dict.password_at(0), "0");
        assert_eq!(dict.password_at(9), "9");
        assert_eq!(dict.password_at(10), "00");
        assert_eq!(dict.password_at(11), "01");
        assert_eq!(dict.password_at(109), "99");
        assert_eq!(dict.password_at(110), "000");
    }

    #[test]
    fn dictionary_lower_case() {
        let config = DictConfig {
            dict_type: crate::model::archive_cracker::DictType::Lowercase,
            min_len: 1,
            max_len: 2,
        };
        let dict = Dictionary::new(&config);
        assert_eq!(dict.total(), 26 + 26 * 26); // 702
        assert_eq!(dict.password_at(0), "a");
        assert_eq!(dict.password_at(25), "z");
        assert_eq!(dict.password_at(26), "aa");
    }

    #[test]
    fn dict_config_total_candidates() {
        let config = DictConfig {
            dict_type: crate::model::archive_cracker::DictType::Digits,
            min_len: 6,
            max_len: 6,
        };
        assert_eq!(config.total_candidates(), 1_000_000);
    }
}
