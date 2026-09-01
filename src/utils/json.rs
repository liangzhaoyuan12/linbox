//! JSON 解析相关纯逻辑层。
//!
//! 本模块不依赖任何 GTK / libadwaita / glib，输入与输出都是纯数据，
//! 因此可被 `cargo test` 在纯终端环境直接单测（见文件底部 `#[cfg(test)]`）。
//!
//! 网络请求相关逻辑已拆到同级 [`crate::utils::http`]，本模块只负责文本与 JSON 的互转。

use serde_json::Value;

/// 解析 JSON 文本。
///
/// 自动去除首尾空白；解析失败时返回人类可读的错误信息。
pub fn parse(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("输入为空，请粘贴 JSON 文本或填写 URL".into());
    }
    serde_json::from_str(trimmed).map_err(|e| format!("JSON 解析失败：{e}"))
}

/// 将 JSON 值格式化为带缩进的美观文本（2 空格缩进）。
pub fn format_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// 压缩为单行（去空白）文本，便于复制紧凑结果。
pub fn format_compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_formats() {
        let v = parse(r#"{"a":1,"b":[true,null,"x"]}"#).unwrap();
        let pretty = format_pretty(&v);
        assert!(pretty.contains("\"a\": 1"));
        assert!(pretty.contains("\"b\": ["));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(parse("{not json").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse("   ").is_err());
    }

    #[test]
    fn compact_formats() {
        let v = parse(r#"{"a":1}"#).unwrap();
        assert_eq!(format_compact(&v), r#"{"a":1}"#);
    }
}
