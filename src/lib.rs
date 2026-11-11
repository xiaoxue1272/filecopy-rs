//! filecopy 的 Rust 版：把相机存储卡上的照片按规则归档到目标目录。
//!
//! 交互全部走 TUI（无命令行参数模式），但配置校验/规划/复制逻辑与引擎
//! 都放在库里，便于测试与非界面场景复用。

pub mod config;
pub mod copy;
pub mod datefmt;
pub mod engine;
pub mod model;
pub mod plan;
pub mod scan;
pub mod tui;

pub use config::Config;
pub use engine::{Engine, EngineEvent, Summary};
pub use model::{format_size, CopyRecord, FileAction, Strategy, VERSION};
