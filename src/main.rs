use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{ColorMode, CompletionType, Config, Context, EditMode, Editor, Helper};

// --- 常量与类型定义 ---
const BUILTINS: [&str; 5] = ["echo", "exit", "type", "pwd", "history"];

// Tab 补全候选词（只包含 echo 和 exit）
const COMPLETION_COMMANDS: [&str; 2] = ["echo", "exit"];

/// 命令补全器
struct CommandCompleter {
    executables: HashMap<String, PathBuf>,
}

impl Completer for CommandCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        // 只在命令开始时补全（没有空格或只有前导空格）
        let trimmed = line[..pos].trim_start();

        // 如果包含空格，说明已经在输入参数了，不补全
        if trimmed.contains(' ') {
            return Ok((pos, vec![]));
        }

        // 获取当前正在输入的词
        let word = &line[..pos];
        let start = word.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
        let prefix = &word[start..];

        // 找到所有匹配的补全候选
        let mut candidates: Vec<Pair> = Vec::new();

        // 1. 添加匹配的内置命令（echo 和 exit）
        for cmd in &COMPLETION_COMMANDS {
            if cmd.starts_with(prefix) {
                candidates.push(Pair {
                    display: cmd.to_string(),
                    replacement: format!("{} ", cmd), // 添加尾随空格
                });
            }
        }

        // 2. 添加匹配的外部可执行文件
        for executable_name in self.executables.keys() {
            if executable_name.starts_with(prefix) {
                candidates.push(Pair {
                    display: executable_name.clone(),
                    replacement: format!("{} ", executable_name), // 添加尾随空格
                });
            }
        }

        // 按字母顺序排序
        candidates.sort_by(|a, b| a.display.cmp(&b.display));

        Ok((start, candidates))
    }
}

impl Hinter for CommandCompleter {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<Self::Hint> {
        None
    }
}

impl Highlighter for CommandCompleter {}

impl Validator for CommandCompleter {}

impl Helper for CommandCompleter {}

/// 输出重定向信息
#[derive(Debug, Clone)]
struct Redirection {
    /// 标准输出重定向文件路径
    stdout_file: Option<String>,
    /// 标准输出是否为追加模式（true=>>，false=>）
    stdout_append: bool,
    /// 标准错误重定向文件路径
    stderr_file: Option<String>,
    /// 标准错误是否为追加模式（true=2>>，false=2>）
    stderr_append: bool,
}

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
    /// 管道命令：包含多个命令及其参数的数组
    Pipeline(Vec<(String, Vec<String>)>),
    /// 历史记录命令
    History,
}

fn main() {
    // 启动时预加载所有可执行文件 (Caching)
    let all_executables = get_all_executables();

    // 配置 rustyline Editor
    let config = Config::builder()
        .completion_type(CompletionType::List) // 列表模式：第一次TAB响铃，第二次显示列表
        .edit_mode(EditMode::Emacs) // Emacs 编辑模式
        .color_mode(ColorMode::Enabled) // 启用颜色
        .build();

    // 创建 rustyline Editor 并设置补全器
    let mut rl = Editor::with_config(config).expect("Failed to create editor");
    let completer = CommandCompleter {
        executables: all_executables.clone(),
    };
    rl.set_helper(Some(completer));

    loop {
        // 构建提示符
        let enable = env::var("ENABLE_CUR_DIR_DISPLAY").unwrap_or(String::from("false"));
        let prompt = if enable == "true" {
            let current = env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
            let dir_name = current.file_name().and_then(|s| s.to_str()).unwrap_or("/");
            format!("[{}] $ ", dir_name)
        } else {
            "$ ".to_string()
        };

        // 读取用户输入
        match rl.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    // 添加到历史记录
                    let _ = rl.add_history_entry(trimmed);

                    // 获取历史记录（不包括当前正在输入的命令）
                    let history: Vec<String> = rl.history().iter().map(|s| s.to_string()).collect();

                    if let Err(e) = execute_command(trimmed, &all_executables, &history) {
                        eprintln!("Execution error: {}", e);
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C: 继续循环
                continue;
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D: 退出
                break;
            }
            Err(err) => {
                eprintln!("Error: {:?}", err);
                break;
            }
        }
    }
}

/// 执行命令
fn execute_command(
    input: &str,
    all_executables: &HashMap<String, PathBuf>,
    history: &[String],
) -> io::Result<()> {
    // 1. 解析：将字符串输入转换为强类型的枚举
    let (action, redirection) = parse_command(input, all_executables);

    // 2. 执行：根据枚举成员执行相应逻辑
    match action {
        CommandAction::Exit => std::process::exit(0),
        CommandAction::Echo(args) => {
            let output = args.join(" ");

            // 处理 stderr 重定向（创建文件但不写入）
            if let Some(ref redir) = redirection {
                if let Some(stderr_file) = &redir.stderr_file {
                    let _ = if redir.stderr_append {
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(stderr_file)
                    } else {
                        File::create(stderr_file)
                    };
                }
            }

            if let Some(redir) = redirection {
                // 重定向到文件（只处理stdout重定向）
                if let Some(stdout_file) = &redir.stdout_file {
                    let file_result = if redir.stdout_append {
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(stdout_file)
                    } else {
                        File::create(stdout_file)
                    };
                    if let Ok(mut file) = file_result {
                        let _ = writeln!(file, "{}", output);
                    }
                } else {
                    // 没有 stdout 重定向，输出到标准输出
                    println!("{}", output);
                }
            } else {
                // 输出到标准输出
                println!("{}", output);
            }
        }
        CommandAction::Type(args) => {
            if let Some(target) = args.first() {
                handle_type_logic(target);
            }
        }
        CommandAction::Pwd => {
            let output = format!("{}", env::current_dir()?.display());

            // 处理 stderr 重定向（创建文件但不写入）
            if let Some(ref redir) = redirection {
                if let Some(stderr_file) = &redir.stderr_file {
                    let _ = if redir.stderr_append {
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(stderr_file)
                    } else {
                        File::create(stderr_file)
                    };
                }
            }

            if let Some(redir) = redirection {
                // 重定向到文件（只处理stdout重定向）
                if let Some(stdout_file) = &redir.stdout_file {
                    let file_result = if redir.stdout_append {
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(stdout_file)
                    } else {
                        File::create(stdout_file)
                    };
                    if let Ok(mut file) = file_result {
                        let _ = writeln!(file, "{}", output);
                    }
                } else {
                    println!("{}", output);
                }
            } else {
                println!("{}", output);
            }
        }
        CommandAction::External(command, args) => {
            let mut cmd = Command::new(command);
            cmd.args(args);

            // 如果有重定向，配置 stdout 和/或 stderr
            if let Some(redir) = redirection {
                if let Some(stdout_file) = &redir.stdout_file {
                    let file_result = if redir.stdout_append {
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(stdout_file)
                    } else {
                        File::create(stdout_file)
                    };
                    if let Ok(file) = file_result {
                        cmd.stdout(Stdio::from(file));
                    }
                }
                if let Some(stderr_file) = &redir.stderr_file {
                    let file_result = if redir.stderr_append {
                        OpenOptions::new()
                            .write(true)
                            .create(true)
                            .append(true)
                            .open(stderr_file)
                    } else {
                        File::create(stderr_file)
                    };
                    if let Ok(file) = file_result {
                        cmd.stderr(Stdio::from(file));
                    }
                }
            }

            let _ = cmd.status();
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
        CommandAction::Pipeline(commands) => {
            execute_pipeline(commands)?;
        }
        CommandAction::History => {
            // 显示历史记录，格式："    <行号>  <命令>"
            for (i, cmd) in history.iter().enumerate() {
                println!("    {}  {}", i + 1, cmd);
            }
        }
    }

    Ok(())
}

/// 解析器：负责命令分发逻辑
fn parse_command(
    input: &str,
    all_executables: &HashMap<String, PathBuf>,
) -> (CommandAction, Option<Redirection>) {
    // 首先检查是否有管道
    let pipeline_parts = parse_pipeline(input);

    if pipeline_parts.len() > 1 {
        // 有管道，解析每个部分
        let mut commands = Vec::new();

        for part in pipeline_parts {
            let (command_part, _) = parse_redirection(&part);
            let tokens = parse_args(&command_part);

            if !tokens.is_empty() {
                let command = tokens[0].clone();
                let args = tokens[1..].to_vec();
                commands.push((command, args));
            }
        }

        return (CommandAction::Pipeline(commands), None);
    }

    // 首先检查是否有重定向操作符
    let (command_part, redirection) = parse_redirection(input);

    // 解析整个命令行，获取命令和参数
    let tokens = parse_args(&command_part);

    if tokens.is_empty() {
        return (CommandAction::Unknown(String::new()), redirection);
    }

    let command = &tokens[0];
    let args: Vec<String> = tokens[1..].to_vec();

    let action = match command.as_str() {
        "exit" => CommandAction::Exit,
        "echo" => CommandAction::Echo(args),
        "pwd" => CommandAction::Pwd,
        "type" => CommandAction::Type(args),
        "cd" => CommandAction::Cd(args),
        "history" => CommandAction::History,
        _ => {
            // 检查是否在预加载的外部命令缓存中
            if all_executables.contains_key(command) {
                CommandAction::External(command.to_string(), args)
            } else {
                CommandAction::Unknown(command.to_string())
            }
        }
    };

    (action, redirection)
}

/// 解析管道：按 | 分割命令，但忽略引号内的 |
fn parse_pipeline(input: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if !in_single_quote => {
                escaped = true;
                current.push(ch);
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
                current.push(ch);
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
                current.push(ch);
            }
            '|' if !in_single_quote && !in_double_quote => {
                // 找到管道符，保存当前命令
                if !current.trim().is_empty() {
                    commands.push(current.trim().to_string());
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    // 添加最后一个命令
    if !current.trim().is_empty() {
        commands.push(current.trim().to_string());
    }

    // 如果没有管道，返回单个命令
    if commands.is_empty() {
        vec![input.to_string()]
    } else {
        commands
    }
}

/// 解析重定向操作符，返回命令部分和重定向信息
fn parse_redirection(input: &str) -> (String, Option<Redirection>) {
    let mut chars = input.chars().peekable();
    let mut command_part = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut stdout_file: Option<String> = None;
    let mut stdout_append = false;
    let mut stderr_file: Option<String> = None;
    let mut stderr_append = false;

    while let Some(ch) = chars.peek() {
        // 处理引号状态
        if !escaped {
            match ch {
                '\\' if !in_single_quote => {
                    escaped = true;
                    command_part.push(chars.next().unwrap());
                    continue;
                }
                '\'' if !in_double_quote => {
                    in_single_quote = !in_single_quote;
                    command_part.push(chars.next().unwrap());
                    continue;
                }
                '"' if !in_single_quote => {
                    in_double_quote = !in_double_quote;
                    command_part.push(chars.next().unwrap());
                    continue;
                }
                '>' if !in_single_quote && !in_double_quote => {
                    // 找到重定向操作符
                    chars.next(); // 消费 '>'

                    // 检查是否是追加模式 '>>'
                    let is_append = if chars.peek() == Some(&'>') {
                        chars.next(); // 消费第二个 '>'
                        true
                    } else {
                        false
                    };

                    // 跳过空格
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }

                    // 获取输出文件名（读取到下一个重定向符或结束）
                    let file = parse_filename(&mut chars);

                    if !file.is_empty() {
                        stdout_file = Some(file);
                        stdout_append = is_append;
                    }
                    continue;
                }
                '1' if !in_single_quote && !in_double_quote => {
                    // 检查是否是 "1>" 或 "1>>" 形式
                    let mut temp_chars = chars.clone();
                    temp_chars.next(); // 跳过 '1'
                    if temp_chars.peek() == Some(&'>') {
                        chars.next(); // 消费 '1'
                        chars.next(); // 消费 '>'

                        // 检查是否是追加模式 '1>>'
                        let is_append = if chars.peek() == Some(&'>') {
                            chars.next(); // 消费第二个 '>'
                            true
                        } else {
                            false
                        };

                        // 跳过空格
                        while chars.peek() == Some(&' ') {
                            chars.next();
                        }

                        // 获取输出文件名
                        let file = parse_filename(&mut chars);

                        if !file.is_empty() {
                            stdout_file = Some(file);
                            stdout_append = is_append;
                        }
                    } else {
                        command_part.push(chars.next().unwrap());
                    }
                    continue;
                }
                '2' if !in_single_quote && !in_double_quote => {
                    // 检查是否是 "2>" 或 "2>>" 形式（stderr重定向）
                    let mut temp_chars = chars.clone();
                    temp_chars.next(); // 跳过 '2'
                    if temp_chars.peek() == Some(&'>') {
                        chars.next(); // 消费 '2'
                        chars.next(); // 消费 '>'

                        // 检查是否是追加模式 '2>>'
                        let is_append = if chars.peek() == Some(&'>') {
                            chars.next(); // 消费第二个 '>'
                            true
                        } else {
                            false
                        };

                        // 跳过空格
                        while chars.peek() == Some(&' ') {
                            chars.next();
                        }

                        // 获取输出文件名
                        let file = parse_filename(&mut chars);

                        if !file.is_empty() {
                            stderr_file = Some(file);
                            stderr_append = is_append;
                        }
                    } else {
                        command_part.push(chars.next().unwrap());
                    }
                    continue;
                }
                _ => {}
            }
        }

        escaped = false;
        command_part.push(chars.next().unwrap());
    }

    // 构建重定向信息
    let redirection = if stdout_file.is_some() || stderr_file.is_some() {
        Some(Redirection {
            stdout_file,
            stdout_append,
            stderr_file,
            stderr_append,
        })
    } else {
        None
    };

    (command_part.trim_end().to_string(), redirection)
}

/// 从字符迭代器中解析文件名（直到空格、重定向符或结束）
fn parse_filename(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut filename = String::new();

    while let Some(&ch) = chars.peek() {
        // 停止条件：遇到空格、重定向操作符或特殊字符
        if ch == ' ' || ch == '>' || ch == '1' || ch == '2' {
            // 检查是否是重定向操作符的开始
            if ch == '1' || ch == '2' {
                let mut temp = chars.clone();
                temp.next();
                if temp.peek() == Some(&'>') {
                    // 这是下一个重定向操作符，停止解析
                    break;
                }
            } else if ch == '>' || ch == ' ' {
                break;
            }
        }

        filename.push(chars.next().unwrap());
    }

    filename.trim().to_string()
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

/// 解析命令行参数，正确处理引号、空格和转义
///
/// 规则：
/// - 单引号内：所有字符都是字面量，包括双引号和反斜杠
/// - 双引号内：保留空格，单引号被视为普通字符，\ 只转义 " 和 \ 本身
/// - 引号外的反斜杠：转义下一个字符，使其成为字面字符，反斜杠本身被移除
/// - 引号外的连续空格被视为分隔符
/// - 相邻的引号字符串会被连接（无空格分隔时）
/// - 空引号被忽略
///
/// 返回：包含命令和所有参数的 token 数组
fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false; // 跟踪是否处于转义状态（仅用于引号外）

    while let Some(ch) = chars.next() {
        if escaped {
            // 如果前一个字符是反斜杠（在引号外），当前字符作为字面字符
            current_arg.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if in_double_quote => {
                // 在双引号内的反斜杠：只转义特定字符 (", \)
                if let Some(&next_ch) = chars.peek() {
                    if next_ch == '"' || next_ch == '\\' {
                        // 转义：跳过反斜杠，添加被转义的字符
                        chars.next(); // 消费下一个字符
                        current_arg.push(next_ch);
                    } else {
                        // 其他字符：反斜杠作为字面字符
                        current_arg.push('\\');
                    }
                } else {
                    // 反斜杠是最后一个字符，作为字面字符
                    current_arg.push('\\');
                }
            }
            '\\' if !in_single_quote && !in_double_quote => {
                // 在引号外的反斜杠：设置转义标志，反斜杠本身不添加
                escaped = true;
            }
            '\'' if !in_double_quote => {
                // 不在双引号内时，切换单引号状态
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                // 不在单引号内时，切换双引号状态
                in_double_quote = !in_double_quote;
            }
            ' ' if !in_single_quote && !in_double_quote => {
                // 在引号外的空格：如果当前参数非空，则完成当前参数
                if !current_arg.is_empty() {
                    args.push(current_arg.clone());
                    current_arg.clear();
                }
                // 跳过连续空格
            }
            _ => {
                // 其他字符直接添加到当前参数（包括引号内的所有字符）
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

/// 检查命令是否是内置命令
fn is_builtin(command: &str) -> bool {
    matches!(command, "echo" | "type" | "pwd" | "cd" | "exit" | "history")
}

/// 在子进程中执行内置命令
fn execute_builtin_in_child(command: &str, args: &[String]) {
    // 对于不使用 stdin 的命令（type, pwd），需要消耗掉所有 stdin 输入
    // 这样可以避免前面的命令因为管道关闭而产生 "Broken pipe" 错误
    // 注意：echo 不应该消耗 stdin，因为它只输出参数
    let should_consume_stdin = matches!(command, "type" | "pwd");

    if should_consume_stdin {
        // 读取并丢弃所有 stdin 数据
        let mut buffer = [0u8; 8192];
        loop {
            match unsafe { libc::read(0, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) } {
                n if n > 0 => continue, // 继续读取
                _ => break,             // EOF 或错误，停止读取
            }
        }
    }

    match command {
        "echo" => {
            println!("{}", args.join(" "));
        }
        "type" => {
            if let Some(target) = args.first() {
                if BUILTINS.contains(&target.as_str()) {
                    println!("{} is a shell builtin", target);
                } else if let Some(path) = find_command_in_path(target) {
                    println!("{} is {}", target, path.display());
                } else {
                    eprintln!("{}: not found", target);
                }
            }
        }
        "pwd" => {
            if let Ok(dir) = env::current_dir() {
                println!("{}", dir.display());
            }
        }
        _ => {}
    }
}

/// 执行管道命令
fn execute_pipeline(commands: Vec<(String, Vec<String>)>) -> io::Result<()> {
    if commands.is_empty() {
        return Ok(());
    }

    if commands.len() == 1 {
        // 只有一个命令，直接执行
        let (command, args) = &commands[0];
        if is_builtin(command) {
            execute_builtin_in_child(command, args);
        } else {
            let _ = Command::new(command).args(args).status();
        }
        return Ok(());
    }

    // 创建管道并执行多个命令
    let mut pipes: Vec<(i32, i32)> = Vec::new();

    // 创建 n-1 个管道（n 是命令数量）
    for _ in 0..commands.len() - 1 {
        let mut pipe_fds = [0i32; 2];
        unsafe {
            if libc::pipe(pipe_fds.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        pipes.push((pipe_fds[0], pipe_fds[1]));
    }

    let mut pids = Vec::new();

    for (i, (command, args)) in commands.iter().enumerate() {
        let is_cmd_builtin = is_builtin(command);

        unsafe {
            let pid = libc::fork();

            if pid < 0 {
                return Err(io::Error::last_os_error());
            } else if pid == 0 {
                // 子进程

                // 设置 stdin：如果不是第一个命令，从前一个管道读取
                if i > 0 {
                    let (read_fd, _) = pipes[i - 1];
                    libc::dup2(read_fd, 0);
                }

                // 设置 stdout：如果不是最后一个命令，写入下一个管道
                if i < commands.len() - 1 {
                    let (_, write_fd) = pipes[i];
                    libc::dup2(write_fd, 1);
                }

                // 关闭所有管道文件描述符
                for (read_fd, write_fd) in &pipes {
                    libc::close(*read_fd);
                    libc::close(*write_fd);
                }

                if is_cmd_builtin {
                    // 执行内置命令
                    execute_builtin_in_child(command, args);
                    std::process::exit(0);
                } else {
                    // 执行外部命令
                    let cmd_cstring = std::ffi::CString::new(command.as_str()).unwrap();
                    let mut args_cstring: Vec<std::ffi::CString> = vec![cmd_cstring.clone()];
                    args_cstring.extend(
                        args.iter()
                            .map(|a| std::ffi::CString::new(a.as_str()).unwrap()),
                    );
                    let mut args_ptr: Vec<*const libc::c_char> =
                        args_cstring.iter().map(|s| s.as_ptr()).collect();
                    args_ptr.push(std::ptr::null());

                    libc::execvp(cmd_cstring.as_ptr(), args_ptr.as_ptr());
                    // 如果 execvp 返回，说明出错了
                    eprintln!("{}: command not found", command);
                    std::process::exit(127);
                }
            } else {
                // 父进程，记录子进程 PID
                pids.push(pid);
            }
        }
    }

    // 父进程关闭所有管道
    unsafe {
        for (read_fd, write_fd) in &pipes {
            libc::close(*read_fd);
            libc::close(*write_fd);
        }
    }

    // 等待所有子进程完成
    for pid in pids {
        unsafe {
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }

    Ok(())
}
