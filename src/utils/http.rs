//! HTTP 请求相关纯逻辑层。
//!
//! 本模块与 `json` 一样不依赖任何 GTK / libadwaita / glib，输入输出均为纯数据，
//! 可被 `cargo test` 在纯终端环境直接单测（见文件底部 `#[cfg(test)]`）。
//!
//! 页面层只负责把用户填写的 URL / 方法 / 请求头 / 请求体组装成 [`RequestSpec`]，
//! 再由 [`send`] 在后台线程执行网络 IO。

use std::time::Duration;

/// 请求方法。目前支持 GET 与 POST。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Method {
    #[default]
    Get,
    Post,
}

impl Method {
    /// 方法名（大写）。
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }

    /// 从字符串解析方法名，大小写不敏感；无法识别返回 `None`。
    pub fn parse(text: &str) -> Option<Method> {
        match text.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            _ => None,
        }
    }
}

/// 一次 HTTP 请求的完整描述（纯数据）。
#[derive(Clone, Debug, Default)]
pub struct RequestSpec {
    /// 完整 URL，必须为 http:// 或 https://
    pub url: String,
    pub method: Method,
    /// 请求头，按填写顺序保存的 (键, 值) 列表
    pub headers: Vec<(String, String)>,
    /// 请求体，仅 POST 有意义
    pub body: String,
}

/// 校验 URL：仅允许 http / https，返回去除首尾空白后的 URL。
pub fn validate_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("URL 为空".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("仅支持 http:// 或 https:// 链接".into());
    }
    Ok(url)
}

/// 请求超时时间。
const TIMEOUT: Duration = Duration::from_secs(15);

/// 执行请求并返回响应体文本。
///
/// 使用阻塞式 `ureq` 客户端，因此**必须在后台线程调用**；
/// 页面层负责线程调度与 UI 更新。
///
/// POST 且用户未显式指定 `Content-Type` 时，默认按 `application/json` 发送。
pub fn send(spec: &RequestSpec) -> Result<String, String> {
    let url = validate_url(&spec.url)?;

    let mut request = match spec.method {
        Method::Get => ureq::get(url).timeout(TIMEOUT),
        Method::Post => ureq::post(url).timeout(TIMEOUT),
    };

    for (key, value) in &spec.headers {
        request = request.set(key, value);
    }

    if spec.method == Method::Post
        && !spec
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        request = request.set("Content-Type", "application/json");
    }

    let response = match spec.method {
        Method::Get => request.call(),
        Method::Post => request.send_string(&spec.body),
    }
    .map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("服务器返回错误状态码：{code}"),
        other => format!("网络请求失败：{other}"),
    })?;

    response
        .into_string()
        .map_err(|e| format!("读取响应体失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_round_trip() {
        assert_eq!(Method::parse("get"), Some(Method::Get));
        assert_eq!(Method::parse("POST"), Some(Method::Post));
        assert_eq!(Method::parse("PUT"), None);
        assert_eq!(Method::Post.as_str(), "POST");
    }

    #[test]
    fn validates_url() {
        assert!(validate_url("http://a.com").is_ok());
        assert!(validate_url("  https://a.com/x  ").is_ok());
        assert!(validate_url("").is_err());
        assert!(validate_url("ftp://a.com").is_err());
    }

}
