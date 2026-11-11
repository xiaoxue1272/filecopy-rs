//! 命令行回归冒烟：真的把编译好的二进制拉起来，检查参数、退出码和报错文案。
//!
//! 交互界面本身没法在这里跑（测试环境没有终端），所以 stdin 一律接 /dev/null，
//! 断言的是「非交互环境下必须干净地拒绝，而不是卡住或 panic」。

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_filecopy");

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Run {
    let output = Command::new(BIN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("应该能启动 filecopy");
    Run {
        code: output.status.code().expect("应正常退出（不是被信号杀掉）"),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn write_config(name: &str, body: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("filecopy-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    path
}

#[test]
fn version_prints_name_and_number() {
    let out = run(&["--version"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(out.stdout.trim(), format!("filecopy {version}"));
    // 短选项行为一致
    assert_eq!(run(&["-V"]).stdout, out.stdout);
}

#[test]
fn help_describes_the_tui_and_has_no_stale_shortcut() {
    let out = run(&["--help"]);
    assert_eq!(out.code, 0);
    // 版本号别写死：发版时只改 Cargo.toml
    let version_line = format!("filecopy {}", env!("CARGO_PKG_VERSION"));
    for needle in [
        version_line.as_str(),
        "用法：",
        "filecopy --config",
        "界面快捷键",
        "[ 开始 ]",
        "[ 预览 ]",
        "Esc",
    ] {
        assert!(
            out.stdout.contains(needle),
            "帮助里缺少 {needle:?}；实际输出:\n{}",
            out.stdout
        );
    }
    // F5 已经从界面上撤掉，帮助里不该再出现
    assert!(
        !out.stdout.contains("F5"),
        "帮助里还有已删除的 F5：\n{}",
        out.stdout
    );
    assert_eq!(run(&["-h"]).stdout, out.stdout);
}

#[test]
fn unknown_argument_is_a_usage_error() {
    let out = run(&["--bogus"]);
    assert_eq!(out.code, 2, "用法错误应按 EXIT_USAGE 退出");
    assert!(
        out.stderr.contains("未知参数") && out.stderr.contains("--bogus"),
        "应指出未知参数；实际 stderr:\n{}",
        out.stderr
    );
    // 报错之后应该提示怎么看帮助
    assert!(out.stderr.contains("--help"), "stderr:\n{}", out.stderr);
}

#[test]
fn config_path_needs_a_value() {
    let out = run(&["--config"]);
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("--config"),
        "应说明 --config 的用法；实际 stderr:\n{}",
        out.stderr
    );
}

#[test]
fn unreadable_config_is_reported_per_problem() {
    let path = write_config(
        "bad.json",
        r#"{"source_dir": 123, "target_dir": "", "workers": 0, "strategy": "nope"}"#,
    );
    let out = run(&["--config", path.to_str().unwrap()]);
    assert_eq!(out.code, 2, "配置有问题应按 EXIT_USAGE 退出");
    for needle in ["source_dir", "workers", "strategy"] {
        assert!(
            out.stderr.contains(needle),
            "应逐条指出 {needle} 的问题；实际 stderr:\n{}",
            out.stderr
        );
    }
}

#[test]
fn valid_config_without_a_terminal_is_refused_cleanly() {
    // stdin 接的是 /dev/null，不是终端：必须直接拒绝，而不是进了 TUI 卡死
    let path = write_config(
        "ok.json",
        r#"{"source_dir": "/tmp/a", "target_dir": "/tmp/b"}"#,
    );
    let out = run(&["--config", path.to_str().unwrap()]);
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("交互式终端"),
        "应说明需要终端；实际 stderr:\n{}",
        out.stderr
    );
}

#[test]
fn no_arguments_without_a_terminal_is_refused_cleanly() {
    let out = run(&[]);
    assert_eq!(out.code, 2);
    assert!(
        out.stderr.contains("交互式终端"),
        "应说明需要终端；实际 stderr:\n{}",
        out.stderr
    );
}

#[test]
fn missing_config_file_is_an_error_not_a_panic() {
    let out = run(&["--config", "/nonexistent/filecopy/definitely-absent.json"]);
    assert_eq!(out.code, 2);
    assert!(
        !out.stderr.contains("panicked"),
        "不该 panic；实际 stderr:\n{}",
        out.stderr
    );
}
