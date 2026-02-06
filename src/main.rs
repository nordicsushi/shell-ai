use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// --- 常量与类型定义 ---
const BUILTINS: [&str; 4] = ["echo", "exit", "type", "pwd"];

/// 定义 Shell 支持的所有动作
enum CommandAction {
    Exit,
    Echo(Vec<String>),
    Type(Vec<String>),
    Pwd,
    /// 外部命令：包含可执行文件的路径和参数数组
    External(String, Vec<String>),
    /// 未知命令
    Unknown(String),
    Cd(Vec<String>),
}

fn main() {
    // 启动时预加载所有可执行文件 (Caching)
    let all_executables = get_all_executables();
    // init current_dir
    loop {
        if let Err(e) = run_shell_cycle(&all_executables) {
            // 处理 EOF (如 Ctrl+D) 退出
            if e.kind() == io::ErrorKind::UnexpectedEof {
                break;
            }
            eprintln!("Execution error: {}", e);
        }
    }
}

/// 执行单次“输入-解析-运行”循环
fn run_shell_cycle(all_executables: &HashMap<String, PathBuf>) -> io::Result<()> {
    let enable = env::var("ENABLE_CUR_DIR_DISPLAY").unwrap_or(String::from("false"));

    if enable == "true" {
        let current = env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
        let dir_name = current.file_name().and_then(|s| s.to_str()).unwrap_or("/");
        print!("[{}] $ ", dir_name);
    } else {
        print!("$ ");
    }

    io::stdout().flush()?;

    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "End of input"));
    }

    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // 1. 解析：将字符串输入转换为强类型的枚举
    let action = parse_command(trimmed, all_executables);

    // 2. 执行：根据枚举成员执行相应逻辑
    match action {
        CommandAction::Exit => std::process::exit(0),
        CommandAction::Echo(args) => {
            println!("{}", args.join(" "));
        }
        CommandAction::Type(args) => {
            if let Some(target) = args.first() {
                handle_type_logic(target);
            }
        }
        CommandAction::Pwd => {
            println!("{}", env::current_dir()?.display());
        }
        CommandAction::External(command, args) => {
            let _ = Command::new(command).args(args).status();
        }
        CommandAction::Cd(args) => {
            /*  为什么要使用set_current_dir?

            在操作系统层面，cd 不能作为一个外部程序（如 /bin/cd）运行，因为它必须改变 当前 Shell 进程 的状态。
            如果你在 Shell 里调用一个外部的 cd 脚本，它只会改变那个子进程的目录，执行完后回到 Shell，路径依然没变。
            通过 std::env::set_current_dir，你直接触发了操作系统的 chdir 系统调用。
            */
            let arg_str = args.first().map(|s| s.as_str()).unwrap_or("");
            let target_path = if arg_str.is_empty() || arg_str == "~" {
                // 处理 cd 或 cd ~，跳转到 HOME
                env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("/"))
            } else {
                PathBuf::from(arg_str)
            };

            if let Err(e) = env::set_current_dir(&target_path) {
                let error_msg = match e.kind() {
                    io::ErrorKind::NotFound => "No such file or directory",
                    io::ErrorKind::PermissionDenied => "Permission denied",
                    io::ErrorKind::NotADirectory => "Not a directory", // 注意：部分系统支持此 Kind
                    _ => "Unknown error",
                };
                eprintln!("cd: {}: {}", target_path.display(), error_msg);
            }
        }
        CommandAction::Unknown(cmd) => {
            eprintln!("{}: command not found", cmd);
        }
    }

    Ok(())
}

/// 解析器：负责命令分发逻辑
fn parse_command(input: &str, all_executables: &HashMap<String, PathBuf>) -> CommandAction {
    // 解析整个命令行，获取命令和参数
    let tokens = parse_args(input);

    if tokens.is_empty() {
        return CommandAction::Unknown(String::new());
    }

    let command = &tokens[0];
    let args: Vec<String> = tokens[1..].to_vec();

    match command.as_str() {
        "exit" => CommandAction::Exit,
        "echo" => CommandAction::Echo(args),
        "pwd" => CommandAction::Pwd,
        "type" => CommandAction::Type(args),
        "cd" => CommandAction::Cd(args),
        _ => {
            // 检查是否在预加载的外部命令缓存中
            if all_executables.contains_key(command) {
                CommandAction::External(command.to_string(), args)
            } else {
                CommandAction::Unknown(command.to_string())
            }
        }
    }
}

/// 处理 type 命令的特定逻辑
fn handle_type_logic(target: &str) {
    if target.is_empty() {
        return;
    }

    if BUILTINS.contains(&target) {
        println!("{} is a shell builtin", target);
    } else if let Some(path) = find_command_in_path(target) {
        println!("{} is {}", target, path.display());
    } else {
        eprintln!("{}: not found", target);
    }
}

/// 解析命令行参数，正确处理引号和空格
///
/// 规则：
/// - 引号内的空格保持原样
/// - 引号外的连续空格被视为分隔符
/// - 相邻的引号字符串会被连接（无空格分隔时）
/// - 空引号被忽略
///
/// 返回：包含命令和所有参数的 token 数组
fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut chars = input.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                // 切换引号状态
                in_quotes = !in_quotes;
            }
            ' ' if !in_quotes => {
                // 在引号外的空格：如果当前参数非空，则完成当前参数
                if !current_arg.is_empty() {
                    args.push(current_arg.clone());
                    current_arg.clear();
                }
                // 跳过连续空格
            }
            _ => {
                // 其他字符直接添加到当前参数
                current_arg.push(ch);
            }
        }
    }

    // 处理最后一个参数
    if !current_arg.is_empty() {
        args.push(current_arg);
    }

    args
}

/// 动态搜索逻辑 (用于 type 命令)
fn find_command_in_path(command: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(command))
            .find(|full_path| is_executable(full_path))
    })
}

/// 预加载所有外部命令 (用于执行校验)
fn get_all_executables() -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();

    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if is_executable(&path) {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            map.entry(name.to_string()).or_insert(path);
                        }
                    }
                }
            }
        }
    }
    map
}

/// 通用判断：路径是否存在且具有执行权限
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
