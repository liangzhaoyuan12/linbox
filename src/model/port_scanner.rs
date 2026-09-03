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
