//! 导航排序与首页视图模式的持久化（Phase 13.2）：`$XDG_CONFIG_HOME/linbox/nav.json`。
//!
//! 形态：`{"order":["json","media",…],"home_view":"card"|"list"}`
//!
//! - `order` 是**中部 navigation list 19 个功能模块**的用户自定义顺序
//!   （首页与设置固定在侧边栏上下两端，不参与排序，见 GOAL D13.3）；
//! - `home_view` 是首页的卡片/列表视图模式（D13.5）；
//! - **容错口径（D13.6）**：文件缺失 / 空文件 / 坏 JSON / 字段类型错 /
//!   未知 `home_view` 一律回退默认序与卡片视图，绝不 panic。
//!
//! 与 `utils/download/settings.rs`（`download.json`）同款路径与写盘姿势：
//! 本地 <1KB JSON 同步写，不涉及外部命令，因此不属 GOAL 3.2 的后台线程范围。
//!
//! 约束：纯逻辑，**禁止 `use gtk` / `use adw` / `use glib`**。

use crate::model::nav_catalog;
use std::path::{Path, PathBuf};

/// 首页显示模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeView {
    /// 卡片网格（默认）。
    Card,
    /// 列表。
    List,
}

impl HomeView {
    /// 落盘用的字符串形态。
    pub fn as_str(self) -> &'static str {
        match self {
            HomeView::Card => "card",
            HomeView::List => "list",
        }
    }

    /// 从落盘字符串解析；**未知值一律回退 `Card`**（D13.6，不编造、不报错）。
    pub fn parse(s: &str) -> HomeView {
        match s.trim() {
            "list" => HomeView::List,
            _ => HomeView::Card,
        }
    }
}

/// 一份完整配置：模块顺序 + 首页视图模式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavConfig {
    /// 中部功能区的 page id 顺序（已归一化：只含默认序里存在的 id、无重复）。
    pub order: Vec<String>,
    /// 首页视图模式。
    pub home_view: HomeView,
}

impl Default for NavConfig {
    fn default() -> Self {
        NavConfig {
            order: nav_catalog::default_order(),
            home_view: HomeView::Card,
        }
    }
}

/// 配置文件路径：`$XDG_CONFIG_HOME/linbox/nav.json`
/// （未设 `XDG_CONFIG_HOME` 时回落 `~/.config/linbox/nav.json`）。
pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from("."))
        });
    base.join("linbox/nav.json")
}

/// 把「保存过的顺序」与「默认顺序」合成最终顺序（D13.3 / D13.6）：
///
/// 1. `saved` 里**默认序中不存在的 id 一律丢弃**（已下线页面的残留、
///    拼错的、含 `..` 或分隔符的注入串都走这条——精确匹配，大小写敏感）；
/// 2. 重复 id **取首见**；
/// 3. 默认序里没被保存过的 id（新增模块 / 用户从未排过的）**按默认相对序追加到末尾**。
pub fn normalize_order(saved: &[String], default: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(default.len());
    for s in saved {
        if default.contains(s) && !out.contains(s) {
            out.push(s.clone());
        }
    }
    for d in default {
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

/// 按一份配置算出最终顺序（= `normalize_order`，供页面直接拿配置用）。
pub fn apply_order(default: &[String], cfg: &NavConfig) -> Vec<String> {
    normalize_order(&cfg.order, default)
}

/// 宽容解析配置文本（D13.6）：任何解析失败都回退「默认序 + 卡片视图」。
///
/// `default` 通常是 `model::nav_catalog::default_order()`。
pub fn parse_config(text: &str, default: &[String]) -> NavConfig {
    // 剥掉 UTF-8 BOM：手工编辑过的配置文件常见，不能因此判成坏文件。
    let text = text.trim_start_matches('\u{feff}');
    #[derive(serde::Deserialize)]
    struct Wire {
        #[serde(default)]
        order: Vec<String>,
        #[serde(default)]
        home_view: String,
    }
    let wire: Wire = match serde_json::from_str(text) {
        Ok(w) => w,
        Err(_) => {
            return NavConfig {
                order: default.to_vec(),
                home_view: HomeView::Card,
            };
        }
    };
    NavConfig {
        order: normalize_order(&wire.order, default),
        home_view: HomeView::parse(&wire.home_view),
    }
}

/// 从指定路径读配置；文件不存在或读失败 → 默认配置。
pub fn load_from(path: &Path, default: &[String]) -> NavConfig {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_config(&text, default),
        Err(_) => NavConfig {
            order: default.to_vec(),
            home_view: HomeView::Card,
        },
    }
}

/// 写到指定路径（自动建父目录）。
pub fn save_to(path: &Path, cfg: &NavConfig) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Wire<'a> {
        order: &'a [String],
        home_view: &'a str,
    }
    let wire = Wire {
        order: &cfg.order,
        home_view: cfg.home_view.as_str(),
    };
    let text = serde_json::to_string_pretty(&wire).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    std::fs::write(path, text).map_err(|e| format!("写入配置失败：{e}"))
}

/// 读真实配置（默认序 = 模块目录）。
pub fn load() -> NavConfig {
    let default = nav_catalog::default_order();
    load_from(&config_path(), &default)
}

/// 写真实配置。
pub fn save(cfg: &NavConfig) -> Result<(), String> {
    save_to(&config_path(), cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小型合成默认序：够表达规则、又不依赖真实模块目录。
    fn def() -> Vec<String> {
        ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect()
    }

    fn sv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ---------- 路径 ----------

    /// 不 set 环境变量（并行测试线程共享会串扰，download/settings.rs 同款告诫），
    /// 只断言路径结构。
    #[test]
    fn config_path_file_name_is_nav_json() {
        assert_eq!(
            config_path().file_name().unwrap().to_string_lossy(),
            "nav.json"
        );
    }

    #[test]
    fn config_path_lives_under_linbox_dir() {
        let p = config_path();
        assert_eq!(
            p.parent().unwrap().file_name().unwrap().to_string_lossy(),
            "linbox"
        );
        assert!(p.to_string_lossy().ends_with("linbox/nav.json"));
    }

    // ---------- normalize_order ----------

    /// 完全没排过 = 默认序。
    #[test]
    fn normalize_empty_saved_is_default() {
        assert_eq!(normalize_order(&[], &def()), def());
    }

    /// 全量重排逐条生效。
    #[test]
    fn normalize_full_reorder_applied() {
        let saved = sv(&["d", "c", "b", "a"]);
        assert_eq!(normalize_order(&saved, &def()), saved);
    }

    /// 部分排序：前 3 个换位，其余按默认序补尾。
    #[test]
    fn normalize_partial_reorder_appends_rest_in_default_order() {
        let saved = sv(&["c", "a"]);
        assert_eq!(normalize_order(&saved, &def()), sv(&["c", "a", "b", "d"]));
    }

    /// 未知 id 丢弃（已下线页面的残留）。
    #[test]
    fn normalize_unknown_ids_dropped() {
        let saved = sv(&["zzz", "a", "gone"]);
        assert_eq!(normalize_order(&saved, &def()), sv(&["a", "b", "c", "d"]));
    }

    /// 注入形态的 id（含 `..`、分隔符、换行、超长）都不是合法模块 id → 判未知丢弃，
    /// 不 panic、不越界。
    #[test]
    fn normalize_injection_like_ids_dropped_without_panic() {
        let saved = sv(&[
            "../../etc/passwd",
            "a/b",
            "..",
            "json\nrm -rf /",
            "  a  ",
            "A",
        ]);
        // 大小写敏感 + 空白不修剪：全部判未知丢弃，只剩默认序
        assert_eq!(normalize_order(&saved, &def()), def());
    }

    /// 大小写敏感：`A` 不等于 `a`。
    #[test]
    fn normalize_is_case_sensitive() {
        let saved = sv(&["A", "B"]);
        assert_eq!(normalize_order(&saved, &def()), def());
    }

    /// 重复 id 取首见。
    #[test]
    fn normalize_dedupe_keeps_first_occurrence() {
        let saved = sv(&["b", "a", "b", "b"]);
        assert_eq!(normalize_order(&saved, &def()), sv(&["b", "a", "c", "d"]));
    }

    /// 新增模块 id 追尾并保持默认相对序。
    #[test]
    fn normalize_new_modules_appended_keeping_default_relative_order() {
        let new_def = sv(&["a", "b", "x", "y", "c"]);
        let saved = sv(&["c", "a"]);
        assert_eq!(
            normalize_order(&saved, &new_def),
            sv(&["c", "a", "b", "x", "y"])
        );
    }

    /// 默认序为空（极端：没有任何模块）也安全。
    #[test]
    fn normalize_empty_default_is_empty() {
        assert!(normalize_order(&sv(&["a"]), &[]).is_empty());
    }

    /// apply_order == normalize_order。
    #[test]
    fn apply_order_matches_normalize() {
        let cfg = NavConfig {
            order: sv(&["d", "b"]),
            home_view: HomeView::Card,
        };
        assert_eq!(
            apply_order(&def(), &cfg),
            normalize_order(&cfg.order, &def())
        );
    }

    // ---------- HomeView ----------

    /// 四态真值表：card / list / 未知 / 空串 / 带首尾空白。
    #[test]
    fn home_view_truth_table() {
        assert_eq!(HomeView::parse("card"), HomeView::Card);
        assert_eq!(HomeView::parse("list"), HomeView::List);
        assert_eq!(HomeView::parse("grid"), HomeView::Card);
        assert_eq!(HomeView::parse(""), HomeView::Card);
        assert_eq!(
            HomeView::parse("  list  "),
            HomeView::List,
            "首尾空白应修剪"
        );
    }

    #[test]
    fn home_view_roundtrip_strings() {
        assert_eq!(HomeView::Card.as_str(), "card");
        assert_eq!(HomeView::List.as_str(), "list");
        assert_eq!(HomeView::parse(HomeView::List.as_str()), HomeView::List);
        assert_eq!(HomeView::parse(HomeView::Card.as_str()), HomeView::Card);
    }

    // ---------- parse_config 容错 ----------

    /// 空文本（等价于空文件）→ 默认。
    #[test]
    fn parse_empty_text_returns_default() {
        let cfg = parse_config("", &def());
        assert_eq!(cfg.order, def());
        assert_eq!(cfg.home_view, HomeView::Card);
    }

    /// 坏 JSON → 默认，不 panic。
    #[test]
    fn parse_bad_json_returns_default() {
        assert_eq!(
            parse_config("{ not json", &def()).order,
            def(),
            "坏 JSON 应回退默认序"
        );
    }

    /// `order` 类型错（是字符串不是数组）→ 整份配置回退默认（D13.6：
    /// 字段类型错一律回退默认序与卡片视图），不 panic。
    #[test]
    fn parse_wrong_type_order_returns_default() {
        let text = r#"{"order":"json","home_view":"list"}"#;
        let cfg = parse_config(text, &def());
        assert_eq!(cfg.order, def());
        assert_eq!(cfg.home_view, HomeView::Card);
    }

    /// 字段缺失（只有 order / 只有 home_view / 空对象）→ 各自回退。
    #[test]
    fn parse_missing_fields_fall_back_per_field() {
        assert_eq!(parse_config(r#"{"home_view":"list"}"#, &def()).order, def());
        assert_eq!(
            parse_config(r#"{"order":["b","a"]}"#, &def()).home_view,
            HomeView::Card
        );
        let empty = parse_config("{}", &def());
        assert_eq!(empty.order, def());
        assert_eq!(empty.home_view, HomeView::Card);
    }

    /// 未知 home_view 值 → Card。
    #[test]
    fn parse_unknown_home_view_falls_back_to_card() {
        let cfg = parse_config(r#"{"order":[],"home_view":"tile"}"#, &def());
        assert_eq!(cfg.home_view, HomeView::Card);
    }

    /// BOM 开头的合法配置仍可解析。
    #[test]
    fn parse_tolerates_bom() {
        let text = "\u{feff}{\"order\":[\"d\",\"c\",\"b\",\"a\"],\"home_view\":\"list\"}";
        let cfg = parse_config(text, &def());
        assert_eq!(cfg.order, sv(&["d", "c", "b", "a"]));
        assert_eq!(cfg.home_view, HomeView::List);
    }

    /// 合法配置走完整链路：解析出的顺序已归一化（未知 id 不会留在结果里）。
    #[test]
    fn parse_normalizes_saved_order() {
        let text = r#"{"order":["d","zzz","b"],"home_view":"list"}"#;
        assert_eq!(parse_config(text, &def()).order, sv(&["d", "b", "a", "c"]));
    }

    // ---------- 读写往返（临时目录，不碰真实配置） ----------

    /// save_to → load_from 往返一致；父目录自动创建。
    #[test]
    fn save_load_roundtrip_in_tempdir() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("deep/inner/nav.json");
        let cfg = NavConfig {
            order: sv(&["b", "d", "a", "c"]),
            home_view: HomeView::List,
        };
        save_to(&path, &cfg).expect("写入");
        assert!(path.exists(), "父目录应被自动创建且文件落盘");
        assert_eq!(load_from(&path, &def()), cfg);
    }

    /// 文件缺失 → 默认配置。
    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("no-such-nav.json");
        let cfg = load_from(&path, &def());
        assert_eq!(cfg.order, def());
        assert_eq!(cfg.home_view, HomeView::Card);
    }

    /// 落盘内容的字节形态：`order` 数组 + `home_view` 字符串（防格式漂移）。
    #[test]
    fn saved_text_has_expected_shape() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("nav.json");
        let cfg = NavConfig {
            order: sv(&["a", "b"]),
            home_view: HomeView::List,
        };
        save_to(&path, &cfg).expect("写入");
        let text = std::fs::read_to_string(&path).expect("读回");
        let v: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");
        assert_eq!(v["order"], serde_json::json!(["a", "b"]));
        assert_eq!(v["home_view"], "list");
    }

    /// NavConfig::default 的序 = 模块目录默认序、视图 = 卡片（D13.5 / D13.6）。
    #[test]
    fn default_config_follows_catalog_and_card_view() {
        let cfg = NavConfig::default();
        assert_eq!(cfg.order, nav_catalog::default_order());
        assert_eq!(cfg.order.len(), 19);
        assert_eq!(cfg.home_view, HomeView::Card);
    }
}
