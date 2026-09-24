//! 端口扫描与服务识别模块的数据模型（纯数据，无 UI 依赖）。
//!
//! 支持 TCP 端口扫描 + 应用层协议指纹识别。

use serde::{Deserialize, Serialize};

/// 常见服务端口 → 服务名映射（仅用于默认显示提示，不决定实际检测结果）。
pub fn well_known_services() -> &'static [(u16, &'static str)] {
    &[
        (20, "FTP-Data"),
        (21, "FTP"),
        (22, "SSH"),
        (23, "Telnet"),
        (25, "SMTP"),
        (53, "DNS"),
        (80, "HTTP"),
        (110, "POP3"),
        (111, "RPCBind"),
        (135, "MSRPC"),
        (139, "NetBIOS"),
        (143, "IMAP"),
        (443, "HTTPS"),
        (445, "SMB"),
        (993, "IMAPS"),
        (995, "POP3S"),
        (1080, "SOCKS"),
        (1433, "MSSQL"),
        (1521, "Oracle"),
        (3306, "MySQL"),
        (3389, "RDP"),
        (5432, "PostgreSQL"),
        (5900, "VNC"),
        (6379, "Redis"),
        (8080, "HTTP-Alt"),
        (8443, "HTTPS-Alt"),
        (9200, "Elasticsearch"),
        (11211, "Memcached"),
        (27017, "MongoDB"),
        (27018, "MongoDB"),
        (50000, "SAP"),
    ]
}

/// 扫描配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    /// 目标地址（IP 或域名）。
    pub target: String,
    /// 起始端口。
    pub port_start: u16,
    /// 结束端口。
    pub port_end: u16,
    /// 扫描的端口列表（覆盖 start/end，非空时使用）。
    pub custom_ports: Vec<u16>,
    /// 并发数。
    pub concurrency: usize,
    /// 单端口超时（毫秒）。
    pub timeout_ms: u64,
    /// 是否进行服务/协议识别。
    pub service_detection: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        ScanConfig {
            target: String::new(),
            port_start: 1,
            port_end: 65535,
            custom_ports: Vec::new(),
            concurrency: 100,
            timeout_ms: 2000,
            service_detection: true,
        }
    }
}

impl ScanConfig {
    /// 获取要扫描的端口列表。
    pub fn ports(&self) -> Vec<u16> {
        if !self.custom_ports.is_empty() {
            let mut ports = self.custom_ports.clone();
            ports.sort();
            ports.dedup();
            return ports;
        }
        (self.port_start..=self.port_end).collect()
    }

    /// 校验配置。
    pub fn validate(&self) -> Result<(), String> {
        if self.target.trim().is_empty() {
            return Err("目标地址不能为空".into());
        }
        if self.custom_ports.is_empty() && self.port_start > self.port_end {
            return Err("起始端口不能大于结束端口".into());
        }
        if self.concurrency < 1 || self.concurrency > 10000 {
            return Err("并发数需在 1~10000 之间".into());
        }
        Ok(())
    }
}

/// 识别到的协议族。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolFamily {
    Tcp,
    Udp,
}

impl ProtocolFamily {
    pub fn label(&self) -> &'static str {
        match self {
            ProtocolFamily::Tcp => "TCP",
            ProtocolFamily::Udp => "UDP",
        }
    }
}

/// 单个端口的扫描结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortResult {
    /// 端口号。
    pub port: u16,
    /// 是否开放。
    pub open: bool,
    /// 服务名称。
    pub service: String,
    /// 应用层协议。
    pub protocol: String,
    /// 协议族。
    pub family: ProtocolFamily,
    /// 服务 banner（版本信息）。
    pub banner: String,
    /// 往返延迟（毫秒）。
    pub latency_ms: u64,
}

/// 扫描事件。
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// 解析目标地址。
    Resolving { target: String },
    /// 解析完成。
    Resolved { ip: String, port_count: usize },
    /// 单个端口结果。
    PortResult(PortResult),
    /// 进度更新（已完成数）。
    Progress { done: usize },
    /// 扫描完成。
    Finished {
        total: usize,
        open: usize,
        elapsed_secs: u64,
    },
    /// 日志。
    Log(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(port: u16) -> Option<&'static str> {
        well_known_services()
            .iter()
            .find(|(p, _)| *p == port)
            .map(|x| x.1)
    }

    #[test]
    fn default_ports_full_range_sorted() {
        let c = ScanConfig {
            target: "127.0.0.1".into(),
            ..Default::default()
        };
        assert!(c.validate().is_ok(), "{:?}", c.validate().err());
        let ports = c.ports();
        assert_eq!(ports.len(), 65535, "默认 1..=65535");
        assert_eq!(ports.first(), Some(&1));
        assert_eq!(ports.last(), Some(&65535));
        assert!(ports.windows(2).all(|w| w[0] < w[1]), "必须严格升序");
        assert_eq!(ProtocolFamily::Tcp.label(), "TCP");
        assert_eq!(ProtocolFamily::Udp.label(), "UDP");
    }

    #[test]
    fn custom_ports_override_sorted_dedup() {
        let c = ScanConfig {
            custom_ports: vec![443, 80, 443, 22],
            ..Default::default()
        };
        assert_eq!(c.ports(), vec![22, 80, 443], "覆盖范围且排序去重");
        let c = ScanConfig {
            port_start: 1000,
            port_end: 1003,
            ..Default::default()
        };
        assert_eq!(c.ports(), vec![1000, 1001, 1002, 1003]);
    }

    #[test]
    fn validate_matrix() {
        let base = || ScanConfig {
            target: "10.0.0.1".into(),
            ..Default::default()
        };
        // 空目标
        let c = ScanConfig::default();
        assert!(c.validate().is_err());
        let c = ScanConfig {
            target: "   ".into(),
            ..Default::default()
        };
        assert!(c.validate().is_err());
        // 起止倒置（仅在无自定义列表时报错）
        let c = ScanConfig {
            port_start: 100,
            port_end: 10,
            ..base()
        };
        assert!(c.validate().is_err());
        // 有自定义列表时起止不参与校验
        let c = ScanConfig {
            port_start: 100,
            port_end: 10,
            custom_ports: vec![22],
            ..base()
        };
        assert!(c.validate().is_ok());
        // 并发边界：0 和 10000 都非法（10001 超上限），1/10000 合法
        let c = ScanConfig {
            concurrency: 0,
            ..base()
        };
        assert!(c.validate().is_err());
        let c = ScanConfig {
            concurrency: 10001,
            ..base()
        };
        assert!(c.validate().is_err());
        let c = ScanConfig {
            concurrency: 1,
            ..base()
        };
        assert!(c.validate().is_ok());
        let c = ScanConfig {
            concurrency: 10000,
            ..base()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn well_known_services_table() {
        let svcs = well_known_services();
        assert!(svcs.len() > 20);
        let mut seen = std::collections::HashSet::new();
        for (port, name) in svcs {
            assert!((1..=65535).contains(port), "{port}");
            assert!(!name.is_empty());
            assert!(seen.insert(*port), "端口 {} 重复", port);
        }
        assert_eq!(name_of(22), Some("SSH"));
        assert_eq!(name_of(443), Some("HTTPS"));
        assert_eq!(name_of(3306), Some("MySQL"));
        assert_eq!(name_of(5432), Some("PostgreSQL"));
        assert_eq!(name_of(6379), Some("Redis"));
        assert!(name_of(65000).is_none() || !seen.contains(&65000));
    }
}
