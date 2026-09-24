//! linbox 可执行入口（薄包装）。
//!
//! 应用本体在 `src/lib.rs`：拆出 lib 目标是为了让 `benches/` 基准测试
//! 能以 `linbox::...` 路径链接纯逻辑层。

fn main() -> glib::ExitCode {
    linbox::run()
}
