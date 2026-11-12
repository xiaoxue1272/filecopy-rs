//! 程序入口：只做参数解析与 TUI 启动，业务逻辑都在库里。
//!
//! 刻意不提供「命令行直接跑一次复制」的模式：用户要求所有交互都在 TUI 里，
//! 参数只保留 --version / --help / --config（预填表单）。

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use filecopy_rs::config::Config;
use filecopy_rs::model::{ASCII_LOGO, EXIT_USAGE, VERSION};
use filecopy_rs::tui;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut config_path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-V" | "--version" => {
                println!("filecopy {}", VERSION);
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            _ if args[i] == "--config" || args[i].starts_with("--config=") => {
                let value = if let Some(rest) = args[i].strip_prefix("--config=") {
                    rest.to_string()
                } else {
                    i += 1;
                    match args.get(i) {
                        Some(v) => v.clone(),
                        None => {
                            eprintln!("错误：--config 需要一个配置文件路径");
                            return ExitCode::from(EXIT_USAGE as u8);
                        }
                    }
                };
                config_path = Some(PathBuf::from(value));
            }
            other => {
                eprintln!("错误：未知参数 {other}");
                eprintln!("试试 `filecopy --help`。");
                return ExitCode::from(EXIT_USAGE as u8);
            }
        }
        i += 1;
    }

    // 加载初始配置（可选）。加载失败直接报错退出，别让用户进了界面才发现配置坏了。
    let initial: Option<Config> = match &config_path {
        Some(path) => match filecopy_rs::config::load_config(path) {
            Ok(cfg) => Some(cfg),
            Err(errors) => {
                eprintln!("配置加载失败：{}", path.display());
                for e in errors {
                    eprintln!("  - {e}");
                }
                return ExitCode::from(EXIT_USAGE as u8);
            }
        },
        None => None,
    };

    // 没有终端就没法画界面：明确报错而不是静默失败。
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        eprintln!("错误：filecopy 需要交互式终端（TUI）。请在终端中运行。");
        return ExitCode::from(EXIT_USAGE as u8);
    }

    match tui::run(initial) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("界面异常：{e}");
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    print!("{ASCII_LOGO}");
    println!(
        "filecopy {} — 文件归档工具（按日期 / 扩展名 / 目录结构）",
        VERSION
    );
    println!();
    println!("用法：");
    println!("  filecopy                  打开配置界面");
    println!("  filecopy --config a.json  用已有配置预填界面");
    println!("  filecopy -V | --version   显示版本");
    println!("  filecopy -h | --help      显示本帮助");
    println!();
    println!("界面快捷键：");
    println!("  Tab / ↑↓        移动光标          ←→   在两列之间移动");
    println!("  回车            选目录 / 改值 / 启动");
    println!("  空格            改值 / 展开高级选项 / 启动（缓冲区上换单位）");
    println!("  底部的 [ 开始 ]  [ 预览 ] 两个按钮分别启动复制与预览。");
    println!("  Ctrl-S          保存配置          Ctrl-O 加载配置");
    println!("  Esc             退出 / 运行中中断  Ctrl-C、Ctrl-Q 随时退出");
    println!();
    println!("说明：没有提供「命令行一次性复制」模式，复制、预览、保存配置都在界面里完成。");
}
