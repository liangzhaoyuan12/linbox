//! ffprobe 媒体信息探测（逻辑层 · 无 GTK 依赖）。
//!
//! 解析逻辑（[`parse`]/[`MediaInfo`]）是纯函数，输入 ffprobe 的 JSON 文本、
//! 输出结构化数据，可被 `cargo test` 单测；真正的子进程调用放在 [`probe_file`]，
//! 由页面在后台线程里调用，避免阻塞 UI。

use serde_json::Value;

/// 单条流（视频 / 音频 / 字幕）。
#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    pub kind: String, // video / audio / subtitle
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_rate: u64,
    pub language: String,
}

/// 一次探测得到的完整媒体信息。
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub format_name: String,
    pub duration_sec: f64,
    pub size_bytes: u64,
    pub overall_bitrate: u64,
    pub streams: Vec<StreamInfo>,
}

impl MediaInfo {
    /// 人类可读的时长（HH:MM:SS）。
    pub fn duration_text(&self) -> String {
        let s = self.duration_sec;
        let h = (s / 3600.0).floor() as u64;
        let m = ((s % 3600.0) / 60.0).floor() as u64;
        let sec = (s % 60.0).floor() as u64;
        format!("{h:02}:{m:02}:{sec:02}")
    }

    /// 分辨率文本（取第一个视频流）。
    pub fn resolution_text(&self) -> String {
        self.streams
            .iter()
            .find(|s| s.kind == "video")
            .map(|v| {
                if v.width > 0 && v.height > 0 {
                    format!("{}×{}", v.width, v.height)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default()
    }

    /// 主要视频编码。
    pub fn video_codec(&self) -> String {
        self.streams
            .iter()
            .find(|s| s.kind == "video")
            .map(|v| v.codec.clone())
            .unwrap_or_default()
    }
}

/// 解析 ffprobe 的 JSON 输出（`-v quiet -print_format json -show_format -show_streams`）。
pub fn parse(json: &str) -> Result<MediaInfo, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let mut info = MediaInfo::default();

    if let Some(fmt) = v.get("format") {
        info.format_name = fmt.get("format_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        info.duration_sec = fmt
            .get("duration")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        info.size_bytes = fmt
            .get("size")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        info.overall_bitrate = fmt
            .get("bit_rate")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
    }

    if let Some(streams) = v.get("streams").and_then(|x| x.as_array()) {
        for s in streams {
            let mut st = StreamInfo::default();
            st.kind = s.get("codec_type").and_then(|x| x.as_str()).unwrap_or("").to_string();
            st.codec = s.get("codec_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            st.width = s.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            st.height = s.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            st.frame_rate = s
                .get("avg_frame_rate")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            st.sample_rate = s
                .get("sample_rate")
                .and_then(|x| x.as_str())
                .and_then(|r| r.parse::<u32>().ok())
                .unwrap_or(0);
            st.channels = s.get("channels").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            st.bit_rate = s
                .get("bit_rate")
                .and_then(|x| x.as_str())
                .and_then(|r| r.parse::<u64>().ok())
                .unwrap_or(0);
            st.language = s
                .get("tags")
                .and_then(|t| t.get("language"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            info.streams.push(st);
        }
    }

    Ok(info)
}

/// 调用外部 `ffprobe` 探测一个文件。应在后台线程调用。
pub fn probe_file(path: &str) -> Result<MediaInfo, String> {
    let output = std::process::Command::new("ffprobe")
        .arg("-v")
        .arg("quiet")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(path)
        .output()
        .map_err(|e| format!("无法执行 ffprobe（请确认已安装）：{e}"))?;
    if !output.status.success() {
        return Err("ffprobe 返回非零退出码".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "streams": [
        {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"avg_frame_rate":"30000/1001","bit_rate":"4000000"},
        {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2,"bit_rate":"192000","tags":{"language":"eng"}}
      ],
      "format": {"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"12.500","size":"6553600","bit_rate":"4224000"}
    }"#;

    #[test]
    fn parses_basic() {
        let info = parse(SAMPLE).unwrap();
        assert_eq!(info.streams.len(), 2);
        assert_eq!(info.video_codec(), "h264");
        assert_eq!(info.resolution_text(), "1920×1080");
        assert_eq!(info.duration_sec, 12.5);
        assert!(info.duration_text().starts_with("00:00:12"));
        assert_eq!(info.streams[1].language, "eng");
    }
}
