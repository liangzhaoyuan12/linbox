//! 端口扫描与服务/协议识别（纯逻辑，无 GTK 依赖）。
//!
//! TCP connect scan + 应用层协议指纹探测。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::port_scanner::{PortResult, ProtocolFamily, ScanConfig, ScanEvent};

// ---------------------------------------------------------------------------
// 服务指纹
// ---------------------------------------------------------------------------

struct ServiceProbe {
    name: &'static str,
    protocol: &'static str,
    family: ProtocolFamily,
    probe: &'static [u8],
    /// 响应需包含的子串（不区分大小写）。
    contains: &'static [&'static str],
    /// banner 需包含的子串（不区分大小写）。
    banner_contains: &'static &'static str,
}

/// 所有探测规则，按优先级排列（越靠前越优先）。
const PROBES: &[ServiceProbe] = &[
    // HTTP / HTTPS
    ServiceProbe { name: "HTTP", protocol: "HTTP/1.x", family: ProtocolFamily::Tcp, probe: b"GET / HTTP/1.0\r\nHost: target\r\n\r\n", contains: &["HTTP/1", "HTTP/2"], banner_contains: &"" },
    ServiceProbe { name: "HTTPS", protocol: "HTTPS", family: ProtocolFamily::Tcp, probe: b"GET / HTTP/1.0\r\nHost: target\r\n\r\n", contains: &["HTTPS", "SSL"], banner_contains: &"" },
    // SSH
    ServiceProbe { name: "SSH", protocol: "SSH-2.0", family: ProtocolFamily::Tcp, probe: b"", contains: &["SSH-"], banner_contains: &"SSH-" },
    // MySQL
    ServiceProbe { name: "MySQL", protocol: "MySQL", family: ProtocolFamily::Tcp, probe: b"", contains: &["mysql", "MariaDB"], banner_contains: &"mysql" },
    // PostgreSQL
    ServiceProbe { name: "PostgreSQL", protocol: "PostgreSQL", family: ProtocolFamily::Tcp, probe: b"\x00\x00\x00\x08\x04\xd2\x16\x2f", contains: &[], banner_contains: &"PostgreSQL" },
    // Redis
    ServiceProbe { name: "Redis", protocol: "Redis", family: ProtocolFamily::Tcp, probe: b"PING\r\n", contains: &["PONG", "+PONG"], banner_contains: &"redis" },
    // MongoDB
    ServiceProbe { name: "MongoDB", protocol: "MongoDB wire", family: ProtocolFamily::Tcp, probe: b"", contains: &[], banner_contains: &"" },
    // FTP
    ServiceProbe { name: "FTP", protocol: "FTP", family: ProtocolFamily::Tcp, probe: b"", contains: &["220", "FTP"], banner_contains: &"FTP" },
    // SMTP
    ServiceProbe { name: "SMTP", protocol: "SMTP", family: ProtocolFamily::Tcp, probe: b"EHLO linbox\r\n", contains: &["250", "SMTP", "ESMTP"], banner_contains: &"SMTP" },
    // POP3
    ServiceProbe { name: "POP3", protocol: "POP3", family: ProtocolFamily::Tcp, probe: b"CAPA\r\n", contains: &["+OK", "POP3"], banner_contains: &"POP3" },
    // IMAP
    ServiceProbe { name: "IMAP", protocol: "IMAP", family: ProtocolFamily::Tcp, probe: b"a001 CAPABILITY\r\n", contains: &["IMAP", "CAPABILITY"], banner_contains: &"IMAP" },
    // Telnet
    ServiceProbe { name: "Telnet", protocol: "Telnet", family: ProtocolFamily::Tcp, probe: b"", contains: &[], banner_contains: &"" },
    // SOCKS5
    ServiceProbe { name: "SOCKS5", protocol: "SOCKS5", family: ProtocolFamily::Tcp, probe: b"\x05\x01\x00", contains: &["\x05\x00"], banner_contains: &"" },
    // SOCKS4
    ServiceProbe { name: "SOCKS4", protocol: "SOCKS4", family: ProtocolFamily::Tcp, probe: b"\x04\x01\x00\x50\x00\x00\x00\x01", contains: &["\x00\x5A"], banner_contains: &"" },
    // RDP
    ServiceProbe { name: "RDP", protocol: "RDP", family: ProtocolFamily::Tcp, probe: b"\x00\x00\x00\x00\xd3\x00\x02\x1f\xe9\x00\x00\x00", contains: &[], banner_contains: &"" },
    // VNC
    ServiceProbe { name: "VNC", protocol: "VNC/RFB", family: ProtocolFamily::Tcp, probe: b"", contains: &["RFB"], banner_contains: &"RFB" },
    // DNS over TCP
    ServiceProbe { name: "DNS", protocol: "DNS/TCP", family: ProtocolFamily::Tcp, probe: b"\x00\x00\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x06google\x03com\x00\x00\x01\x00\x01", contains: &[], banner_contains: &"" },
    // ElasticSearch
    ServiceProbe { name: "Elasticsearch", protocol: "REST/HTTP", family: ProtocolFamily::Tcp, probe: b"GET / HTTP/1.0\r\nHost: target\r\n\r\n", contains: &["elasticsearch", "cluster_name"], banner_contains: &"" },
    // Memcached
    ServiceProbe { name: "Memcached", protocol: "Memcached", family: ProtocolFamily::Tcp, probe: b"version\r\n", contains: &["VERSION", "Memcached"], banner_contains: &"" },
    // MQTT
    ServiceProbe { name: "MQTT", protocol: "MQTT", family: ProtocolFamily::Tcp, probe: b"\x10\x0d\x00\x04MQTT\x04\x02\x00\x3c\x00\x01", contains: &["\x20\x02"], banner_contains: &"" },
    // SIP
    ServiceProbe { name: "SIP", protocol: "SIP", family: ProtocolFamily::Tcp, probe: b"OPTIONS sip:target SIP/2.0\r\nVia: SIP/2.0/TCP linbox:5060\r\nFrom: <sip:test@linbox>;tag=abc\r\nTo: <sip:target>\r\nCall-ID: 1@linbox\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n", contains: &["SIP/2.0"], banner_contains: &"" },
    // MySQL (handshake probe)
    ServiceProbe { name: "MySQL", protocol: "MySQL wire", family: ProtocolFamily::Tcp, probe: b"\x00\x00\x00\x00\x01\x85\xa6\x03\x00\x00\x00\x00\x01\x21\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00", contains: &[], banner_contains: &"" },
    // MSSQL
    ServiceProbe { name: "MSSQL", protocol: "TDS", family: ProtocolFamily::Tcp, probe: b"\x12\x01\x00\x2b\x00\x00\x00\x00\x00\x00\x1a\x00\x06\x01\x00\x23\x00\x01\x02\x00\x24\x00\x01\x03\x00\x25\x00\x04\x00\x26\x00\x01\x05\x00\x27\x00\x01\x06\x00\x28\x00\x01", contains: &[], banner_contains: &"" },
];

// ---------------------------------------------------------------------------
// 服务检测
// ---------------------------------------------------------------------------

/// 对一个已确认开放的端口进行服务/协议识别。
fn detect_service(stream: &mut TcpStream, port: u16) -> (String, String, ProtocolFamily, String) {
    let timeout = Duration::from_secs(3);
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    // 先尝试读取 banner（很多服务会主动发送）
    let mut banner_buf = [0u8; 2048];
    let banner_len = stream.read(&mut banner_buf).unwrap_or(0);
    let banner = String::from_utf8_lossy(&banner_buf[..banner_len]).to_string();

    // 如果 banner 已经包含识别信息，直接匹配
    if !banner.is_empty() {
        if let Some(s) = match_banner(port, &banner) {
            return s;
        }
    }

    // 对每个探针进行尝试
    for probe in PROBES {
        if probe.probe.is_empty() && !banner.is_empty() {
            continue; // 纯 banner 识别的已经试过了
        }
        if probe.probe.is_empty() {
            continue;
        }

        // 写入探针
        if stream.write_all(probe.probe).is_err() {
            continue;
        }

        // 读取响应
        let mut resp_buf = [0u8; 4096];
        let resp_len = stream.read(&mut resp_buf).unwrap_or(0);
        if resp_len == 0 {
            continue;
        }
        let resp = String::from_utf8_lossy(&resp_buf[..resp_len]);

        // 检查匹配
        let mut matched = false;
        for pattern in probe.contains {
            if resp.to_lowercase().contains(&pattern.to_lowercase()) {
                matched = true;
                break;
            }
        }
        if !matched && !probe.banner_contains.is_empty() {
            if resp
                .to_lowercase()
                .contains(&probe.banner_contains.to_lowercase())
            {
                matched = true;
            }
        }

        if matched {
            let service = probe.name.to_string();
            let protocol = probe.protocol.to_string();
            let family = probe.family;
            // 从响应中提取版本
            let version = extract_version(&resp, probe.name);
            return (service, protocol, family, version);
        }
    }

    // 没有匹配的探针，尝试通用 banner 匹配
    if !banner.is_empty() {
        let (service, protocol, family) = generic_banner_match(port, &banner);
        let version = extract_version(&banner, &service);
        return (service, protocol, family, version);
    }

    // 通过已知端口推断
    let (service, protocol, family) = guess_by_port(port);
    (service, protocol, family, String::new())
}

/// 纯 banner 匹配（连接后服务主动发数据的情况）。
fn match_banner(port: u16, banner: &str) -> Option<(String, String, ProtocolFamily, String)> {
    let lower = banner.to_lowercase();

    // SSH
    if lower.starts_with("ssh-") {
        let ver = extract_version(banner, "SSH");
        return Some(("SSH".into(), "SSH-2.0".into(), ProtocolFamily::Tcp, ver));
    }
    // FTP
    if lower.contains("220") && (lower.contains("ftp") || lower.contains("filezilla") || lower.contains("proftpd") || lower.contains("vsftpd") || lower.contains("pure-ftpd")) {
        let ver = extract_version(banner, "FTP");
        return Some(("FTP".into(), "FTP".into(), ProtocolFamily::Tcp, ver));
    }
    // SMTP
    if lower.contains("220") && (lower.contains("smtp") || lower.contains("esmtp") || lower.contains("postfix") || lower.contains("sendmail") || lower.contains("exim")) {
        let ver = extract_version(banner, "SMTP");
        return Some(("SMTP".into(), "SMTP".into(), ProtocolFamily::Tcp, ver));
    }
    // VNC
    if lower.starts_with("rfb") {
        let ver = extract_version(banner, "VNC");
        return Some(("VNC".into(), "VNC/RFB".into(), ProtocolFamily::Tcp, ver));
    }
    // MySQL
    if lower.contains("mysql") || lower.contains("mariadb") {
        let ver = extract_version(banner, "MySQL");
        return Some(("MySQL".into(), "MySQL".into(), ProtocolFamily::Tcp, ver));
    }
    // Redis
    if lower.contains("-err") && lower.contains("wrongpass") {
        return Some(("Redis".into(), "Redis".into(), ProtocolFamily::Tcp, String::new()));
    }
    // MongoDB
    if lower.contains("mongodb") {
        let ver = extract_version(banner, "MongoDB");
        return Some(("MongoDB".into(), "MongoDB wire".into(), ProtocolFamily::Tcp, ver));
    }
    // Elasticsearch
    if lower.contains("elasticsearch") || lower.contains("cluster_name") {
        let ver = extract_version(banner, "Elasticsearch");
        return Some(("Elasticsearch".into(), "REST/HTTP".into(), ProtocolFamily::Tcp, ver));
    }
    // Memcached
    if lower.contains("version") && lower.contains("memcached") {
        let ver = extract_version(banner, "Memcached");
        return Some(("Memcached".into(), "Memcached".into(), ProtocolFamily::Tcp, ver));
    }
    // RDP
    if banner.starts_with("\x03\x00") || banner.starts_with("\x00\x00") {
        // RDP X.224 connection response
        return Some(("RDP".into(), "RDP".into(), ProtocolFamily::Tcp, String::new()));
    }
    None
}

/// 通用 banner 匹配（基于关键词）。
fn generic_banner_match(port: u16, banner: &str) -> (String, String, ProtocolFamily) {
    let lower = banner.to_lowercase();
    if lower.starts_with("http/") {
        ("HTTP".into(), "HTTP/1.x".into(), ProtocolFamily::Tcp)
    } else if lower.starts_with("ssh-") {
        ("SSH".into(), "SSH-2.0".into(), ProtocolFamily::Tcp)
    } else if lower.contains("ftp") {
        ("FTP".into(), "FTP".into(), ProtocolFamily::Tcp)
    } else if lower.contains("smtp") || lower.contains("esmtp") {
        ("SMTP".into(), "SMTP".into(), ProtocolFamily::Tcp)
    } else if lower.contains("pop3") {
        ("POP3".into(), "POP3".into(), ProtocolFamily::Tcp)
    } else if lower.contains("imap") {
        ("IMAP".into(), "IMAP".into(), ProtocolFamily::Tcp)
    } else if lower.contains("ssh") || lower.contains("openssh") {
        ("SSH".into(), "SSH-2.0".into(), ProtocolFamily::Tcp)
    } else if lower.starts_with("+ok") {
        ("POP3".into(), "POP3".into(), ProtocolFamily::Tcp)
    } else {
        guess_by_port(port)
    }
}

/// 通过已知端口推断。
fn guess_by_port(port: u16) -> (String, String, ProtocolFamily) {
    match port {
        21 => ("FTP".into(), "FTP".into(), ProtocolFamily::Tcp),
        22 => ("SSH".into(), "SSH-2.0".into(), ProtocolFamily::Tcp),
        23 => ("Telnet".into(), "Telnet".into(), ProtocolFamily::Tcp),
        25 => ("SMTP".into(), "SMTP".into(), ProtocolFamily::Tcp),
        53 => ("DNS".into(), "DNS".into(), ProtocolFamily::Udp),
        80 => ("HTTP".into(), "HTTP/1.x".into(), ProtocolFamily::Tcp),
        110 => ("POP3".into(), "POP3".into(), ProtocolFamily::Tcp),
        143 => ("IMAP".into(), "IMAP".into(), ProtocolFamily::Tcp),
        443 => ("HTTPS".into(), "HTTPS".into(), ProtocolFamily::Tcp),
        445 => ("SMB".into(), "SMB/CIFS".into(), ProtocolFamily::Tcp),
        993 => ("IMAPS".into(), "IMAPS".into(), ProtocolFamily::Tcp),
        995 => ("POP3S".into(), "POP3S".into(), ProtocolFamily::Tcp),
        1080 => ("SOCKS".into(), "SOCKS".into(), ProtocolFamily::Tcp),
        1433 => ("MSSQL".into(), "TDS".into(), ProtocolFamily::Tcp),
        1521 => ("Oracle".into(), "Oracle TNS".into(), ProtocolFamily::Tcp),
        3306 => ("MySQL".into(), "MySQL".into(), ProtocolFamily::Tcp),
        3389 => ("RDP".into(), "RDP".into(), ProtocolFamily::Tcp),
        5432 => ("PostgreSQL".into(), "PostgreSQL".into(), ProtocolFamily::Tcp),
        5900 => ("VNC".into(), "VNC/RFB".into(), ProtocolFamily::Tcp),
        6379 => ("Redis".into(), "Redis".into(), ProtocolFamily::Tcp),
        8080 => ("HTTP".into(), "HTTP/1.x".into(), ProtocolFamily::Tcp),
        8443 => ("HTTPS".into(), "HTTPS".into(), ProtocolFamily::Tcp),
        9200 => ("Elasticsearch".into(), "REST/HTTP".into(), ProtocolFamily::Tcp),
        11211 => ("Memcached".into(), "Memcached".into(), ProtocolFamily::Tcp),
        27017 => ("MongoDB".into(), "MongoDB wire".into(), ProtocolFamily::Tcp),
        _ => ("unknown".into(), "unknown".into(), ProtocolFamily::Tcp),
    }
}

/// 从 banner/响应中提取版本字符串。
fn extract_version(text: &str, service: &str) -> String {
    match service {
        "SSH" => {
            // SSH-2.0-OpenSSH_8.9p1 Ubuntu-3ubuntu0.1
            if let Some(idx) = text.find("SSH-") {
                let rest = &text[idx..];
                if let Some(end) = rest.find(|c: char| c == '\r' || c == '\n') {
                    return rest[..end].trim().to_string();
                }
                return rest.trim().to_string();
            }
            String::new()
        }
        "FTP" => {
            // 220 ProFTPD 1.3.6 Server ready.
            if let Some(idx) = text.find("220") {
                let rest = text[idx..].trim();
                if let Some(end) = rest.find(|c: char| c == '\r' || c == '\n') {
                    return rest[..end].trim().to_string();
                }
                return rest.trim().to_string();
            }
            String::new()
        }
        "SMTP" => {
            // 220 mail.example.com ESMTP Postfix
            if let Some(idx) = text.find("220") {
                let rest = text[idx..].trim();
                if let Some(end) = rest.find(|c: char| c == '\r' || c == '\n') {
                    return rest[..end].trim().to_string();
                }
                return rest.trim().to_string();
            }
            String::new()
        }
        "VNC" => {
            // RFB 003.008\n
            text.trim().lines().next().unwrap_or("").trim().to_string()
        }
        "HTTP" | "HTTPS" | "Elasticsearch" => {
            // Server: nginx/1.24.0  or  Server: Apache/2.4.52
            if let Some(idx) = text.to_lowercase().find("server:") {
                let rest = &text[idx + 7..];
                if let Some(end) = rest.find(|c: char| c == '\r' || c == '\n') {
                    return rest[..end].trim().to_string();
                }
            }
            String::new()
        }
        "MySQL" => {
            // 检查版本号 5.7.x / 8.0.x
            if let Some(idx) = text.find(|c: char| c.is_ascii_digit()) {
                let rest = &text[idx..];
                if let Some(end) = rest.find(|c: char| !c.is_ascii_digit() && c != '.') {
                    let ver = &rest[..end];
                    if ver.contains('.') {
                        return format!("MySQL {ver}");
                    }
                }
            }
            String::new()
        }
        "MongoDB" => {
            if let Some(idx) = text.find("MongoDB") {
                let rest = &text[idx..];
                if let Some(end) = rest.find(|c: char| c == '\r' || c == '\n') {
                    return rest[..end].trim().to_string();
                }
            }
            String::new()
        }
        "Memcached" => {
            // VERSION 1.6.22
            if let Some(idx) = text.to_uppercase().find("VERSION") {
                let rest = &text[idx + 7..];
                if let Some(end) = rest.find(|c: char| c == '\r' || c == '\n') {
                    return format!("Memcached {}", rest[..end].trim());
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 扫描引擎
// ---------------------------------------------------------------------------

/// 启动端口扫描，通过 mpsc 发送事件。
pub fn start_scan(config: ScanConfig, tx: Sender<ScanEvent>) {
    let ports = config.ports();
    let port_count = ports.len();
    let timeout = Duration::from_millis(config.timeout_ms);

    // 解析目标
    let _ = tx.send(ScanEvent::Resolving {
        target: config.target.clone(),
    });

    let addr = match format!("{}:0", config.target.trim()).to_socket_addrs() {
        Ok(mut addrs) => addrs.next().unwrap(),
        Err(e) => {
            let _ = tx.send(ScanEvent::Log(format!("DNS 解析失败：{e}")));
            let _ = tx.send(ScanEvent::Finished {
                total: 0,
                open: 0,
                elapsed_secs: 0,
            });
            return;
        }
    };

    let ip = addr.ip().to_string();
    let _ = tx.send(ScanEvent::Resolved {
        ip: ip.clone(),
        port_count,
    });

    let started = Instant::now();

    // 使用 Mutex 包装 sender 以便多线程共享（Sender 是 Clone+Send 的）
    let tx = Arc::new(Mutex::new(tx));
    let concurrency = config.concurrency.min(port_count).max(1);
    let service_detection = config.service_detection;

    // 使用简单的并发模型：N 个线程，共享端口队列
    let ports = Arc::new(Mutex::new(ports.into_iter()));
    let done_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total = port_count;

    let handles: Vec<_> = (0..concurrency)
        .map(|_| {
            let ports = Arc::clone(&ports);
            let tx = Arc::clone(&tx);
            let done = Arc::clone(&done_count);
            let ip = ip.clone();
            let timeout = timeout;
            let sd = service_detection;

            std::thread::spawn(move || {
                loop {
                    let port = {
                        let mut guard = ports.lock().unwrap();
                        guard.next()
                    };
                    let port = match port {
                        Some(p) => p,
                        None => break,
                    };

                    let result = scan_port(&ip, port, timeout, sd);
                    let _ = tx.lock().unwrap().send(ScanEvent::PortResult(result));

                    let d = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if d % 100 == 0 || d == total {
                        let _ = tx.lock().unwrap().send(ScanEvent::Progress { done: d });
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().ok();
    }

    let elapsed = started.elapsed().as_secs();
    let open = done_count.load(std::sync::atomic::Ordering::Relaxed);
    let _ = tx.lock().unwrap().send(ScanEvent::Finished {
        total,
        open, // 这里用 done 代替，实际 open 数由 PortResult 统计
        elapsed_secs: elapsed,
    });
}

/// 扫描单个端口。
fn scan_port(ip: &str, port: u16, timeout: Duration, service_detection: bool) -> PortResult {
    let addr = format!("{ip}:{port}");
    let started = Instant::now();

    match TcpStream::connect_timeout(&addr.parse().unwrap(), timeout) {
        Ok(mut stream) => {
            stream.set_read_timeout(Some(timeout)).ok();
            stream.set_write_timeout(Some(timeout)).ok();
            let latency = started.elapsed().as_millis() as u64;

            if service_detection {
                let (service, protocol, family, banner) = detect_service(&mut stream, port);
                PortResult {
                    port,
                    open: true,
                    service,
                    protocol,
                    family,
                    banner,
                    latency_ms: latency,
                }
            } else {
                let (service, protocol, family) = guess_by_port(port);
                PortResult {
                    port,
                    open: true,
                    service,
                    protocol,
                    family,
                    banner: String::new(),
                    latency_ms: latency,
                }
            }
        }
        Err(_) => PortResult {
            port,
            open: false,
            service: String::new(),
            protocol: String::new(),
            family: ProtocolFamily::Tcp,
            banner: String::new(),
            latency_ms: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ssh_version() {
        let banner = "SSH-2.0-OpenSSH_8.9p1 Ubuntu-3ubuntu0.1\r\n";
        assert_eq!(
            extract_version(banner, "SSH"),
            "SSH-2.0-OpenSSH_8.9p1 Ubuntu-3ubuntu0.1"
        );
    }

    #[test]
    fn extract_ftp_banner() {
        let banner = "220 ProFTPD 1.3.6 Server ready.\r\n";
        assert_eq!(extract_version(banner, "FTP"), "220 ProFTPD 1.3.6 Server ready.");
    }

    #[test]
    fn extract_http_server() {
        let banner = "HTTP/1.1 200 OK\r\nServer: nginx/1.24.0\r\n\r\n";
        assert_eq!(extract_version(banner, "HTTP"), "nginx/1.24.0");
    }

    #[test]
    fn guess_by_known_port() {
        assert_eq!(guess_by_port(22).0, "SSH");
        assert_eq!(guess_by_port(80).0, "HTTP");
        assert_eq!(guess_by_port(3306).0, "MySQL");
        assert_eq!(guess_by_port(6379).0, "Redis");
    }

    #[test]
    fn config_ports_range() {
        let config = ScanConfig {
            port_start: 80,
            port_end: 83,
            ..Default::default()
        };
        assert_eq!(config.ports(), vec![80, 81, 82, 83]);
    }

    #[test]
    fn config_custom_ports() {
        let config = ScanConfig {
            custom_ports: vec![80, 443, 8080],
            ..Default::default()
        };
        assert_eq!(config.ports(), vec![80, 443, 8080]);
    }
}
