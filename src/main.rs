use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{ColorMode, CompletionType, Config, Context, EditMode, Editor, Helper};

use rig::completion::Prompt;
use rig::providers::openai;

// --- Constants and Type Definitions ---
const BUILTINS: [&str; 5] = ["echo", "exit", "type", "pwd", "history"];

// Tab completion candidates (only echo and exit)
const COMPLETION_COMMANDS: [&str; 2] = ["echo", "exit"];

/// Command completer
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
        // Only complete at command start (no spaces or only leading spaces)
        let trimmed = line[..pos].trim_start();

        // If contains space, already entering arguments, don't complete
        if trimmed.contains(' ') {
            return Ok((pos, vec![]));
        }

        // Get the currently typed word
        let word = &line[..pos];
        let start = word.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
        let prefix = &word[start..];

        // Find all matching completion candidates
        let mut candidates: Vec<Pair> = Vec::new();

        // 1. Add matching builtin commands (echo and exit)
        for cmd in &COMPLETION_COMMANDS {
            if cmd.starts_with(prefix) {
                candidates.push(Pair {
                    display: cmd.to_string(),
                    replacement: format!("{} ", cmd), // Add trailing space
                });
            }
        }

        // 2. Add matching external executable files
        for executable_name in self.executables.keys() {
            if executable_name.starts_with(prefix) {
                candidates.push(Pair {
                    display: executable_name.clone(),
                    replacement: format!("{} ", executable_name), // Add trailing space
                });
            }
        }

        // Sort alphabetically
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

/// Output redirection information
#[derive(Debug, Clone)]
struct Redirection {
    /// Standard output redirect file path
    stdout_file: Option<String>,
    /// Whether standard output is in append mode (true=>>, false=>)
    stdout_append: bool,
    /// Standard error redirect file path
    stderr_file: Option<String>,
    /// Whether standard error is in append mode (true=2>>, false=2>)
    stderr_append: bool,
}

/// Define all actions supported by the Shell
enum CommandAction {
    Exit,
    Echo(Vec<String>),
    Type(Vec<String>),
    Pwd,
    Ai(Vec<String>),
    /// External command: contains executable file path and argument array
    External(String, Vec<String>),
    /// Unknown command
    Unknown(String),
    Cd(Vec<String>),
    /// Pipeline command: contains array of multiple commands and their arguments
    Pipeline(Vec<(String, Vec<String>)>),
    /// History command: optional parameter specifies showing last n records
    History(Option<usize>),
    /// Read history from file
    HistoryRead(String),
    /// Write history to file
    HistoryWrite(String),
    /// Append new history to file
    HistoryAppend(String),
}

fn main() {
    // Preload all executables at startup (Caching)
    let all_executables = get_all_executables();

    // Configure rustyline Editor
    let config = Config::builder()
        .completion_type(CompletionType::List) // List mode: first TAB rings bell, second TAB shows list
        .edit_mode(EditMode::Emacs) // Emacs edit mode
        .color_mode(ColorMode::Enabled) // Enable colors
        .history_ignore_dups(false) // Don't deduplicate history commands
        .expect("Failed to configure history")
        .build();

    // Create rustyline Editor and set completer
    let mut rl = Editor::with_config(config).expect("Failed to create editor");
    let completer = CommandCompleter {
        executables: all_executables.clone(),
    };
    rl.set_helper(Some(completer));

    // Load history from HISTFILE at startup
    if let Ok(histfile_path) = env::var("HISTFILE") {
        if let Ok(content) = fs::read_to_string(&histfile_path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let _ = rl.add_history_entry(trimmed);
                }
            }
        }
    }

    // Track the number of history entries at last file write
    let mut last_written_count: usize = 0;

    loop {
        // Build prompt
        let enable = env::var("ENABLE_CUR_DIR_DISPLAY").unwrap_or(String::from("false"));
        let prompt = if enable == "true" {
            let current = env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
            let dir_name = current.file_name().and_then(|s| s.to_str()).unwrap_or("/");
            format!("[{}] $ ", dir_name)
        } else {
            "$ ".to_string()
        };

        // Read user input
        match rl.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    // Add to history
                    let _ = rl.add_history_entry(trimmed);

                    // Get history (excluding the current command being entered)
                    let history: Vec<String> = rl.history().iter().map(|s| s.to_string()).collect();

                    if let Err(e) = execute_command(
                        trimmed,
                        &all_executables,
                        &history,
                        &mut rl,
                        &mut last_written_count,
                    ) {
                        eprintln!("Execution error: {}", e);
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C: continue loop
                continue;
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D: save history before exit
                let history: Vec<String> = rl.history().iter().map(|s| s.to_string()).collect();
                save_history_to_histfile(&history);
                break;
            }
            Err(err) => {
                eprintln!("Error: {:?}", err);
                break;
            }
        }
    }
}

/// Execute command
fn execute_command(
    input: &str,
    all_executables: &HashMap<String, PathBuf>,
    history: &[String],
    rl: &mut Editor<CommandCompleter, DefaultHistory>,
    last_written_count: &mut usize,
) -> io::Result<()> {
    // 1. Parse: convert string input to strongly-typed enum
    let (action, redirection) = parse_command(input, all_executables);

    // 2. Execute: perform corresponding logic based on enum variant
    match action {
        CommandAction::Exit => {
            // Save history to HISTFILE before exit
            save_history_to_histfile(history);
            std::process::exit(0);
        }
        CommandAction::Echo(args) => {
            let output = args.join(" ");

            // Handle stderr redirection (create file but don't write)
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
                // Redirect to file (only handle stdout redirection)
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
                    // No stdout redirection, output to standard output
                    println!("{}", output);
                }
            } else {
                // Output to standard output
                println!("{}", output);
            }
        }
        CommandAction::Ai(args) => {
            generate_command_with_ai(args);
        }
        CommandAction::Type(args) => {
            if let Some(target) = args.first() {
                handle_type_logic(target);
            }
        }
        CommandAction::Pwd => {
            let output = format!("{}", env::current_dir()?.display());

            // Handle stderr redirection (create file but don't write)
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
                // Redirect to file (only handle stdout redirection)
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

            // If there's redirection, configure stdout and/or stderr
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
            /*  Why use set_current_dir?

            At the OS level, cd cannot run as an external program (like /bin/cd) because it must change the state of the current Shell process.
            If you call an external cd script in the Shell, it only changes that subprocess's directory, and when it returns to the Shell, the path remains unchanged.
            Through std::env::set_current_dir, you directly trigger the OS's chdir system call.
            */
            let arg_str = args.first().map(|s| s.as_str()).unwrap_or("");
            let target_path = if arg_str.is_empty() || arg_str == "~" {
                // Handle cd or cd ~, jump to HOME
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
                    io::ErrorKind::NotADirectory => "Not a directory", // Note: some systems support this Kind
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
        CommandAction::History(limit) => {
            // Decide how many history entries to show based on limit parameter
            let (items_to_show, start_index) = if let Some(n) = limit {
                // Show last n entries
                let start = history.len().saturating_sub(n);
                (&history[start..], start)
            } else {
                // Show all entries
                (&history[..], 0)
            };

            // Display history, format: "    <line_number>  <command>"
            for (i, cmd) in items_to_show.iter().enumerate() {
                println!("    {}  {}", start_index + i + 1, cmd);
            }
        }
        CommandAction::HistoryRead(path) => {
            // Read history from file and append to in-memory history list
            match fs::read_to_string(&path) {
                Ok(content) => {
                    for line in content.lines() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let _ = rl.add_history_entry(trimmed);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("history: {}: {}", path, e);
                }
            }
        }
        CommandAction::HistoryWrite(path) => {
            // Write history to file
            match File::create(&path) {
                Ok(mut file) => {
                    // Write all history entries, one command per line
                    for cmd in history {
                        if let Err(e) = writeln!(file, "{}", cmd) {
                            eprintln!("history: {}: {}", path, e);
                            return Ok(());
                        }
                    }
                    // Update the count of written entries
                    *last_written_count = history.len();
                }
                Err(e) => {
                    eprintln!("history: {}: {}", path, e);
                }
            }
        }
        CommandAction::HistoryAppend(path) => {
            // Append new history to file
            match OpenOptions::new()
                .write(true)
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // Only append new commands since last write
                    let new_commands = &history[*last_written_count..];
                    for cmd in new_commands {
                        if let Err(e) = writeln!(file, "{}", cmd) {
                            eprintln!("history: {}: {}", path, e);
                            return Ok(());
                        }
                    }
                    // Update the count of written entries
                    *last_written_count = history.len();
                }
                Err(e) => {
                    eprintln!("history: {}: {}", path, e);
                }
            }
        }
    }

    Ok(())
}

/// Parser: responsible for command dispatch logic
fn parse_command(
    input: &str,
    all_executables: &HashMap<String, PathBuf>,
) -> (CommandAction, Option<Redirection>) {
    // First check if it's an AI command (starts with !)
    let trimmed = input.trim();
    if trimmed.starts_with('!') {
        // Extract all content after ! as AI prompt
        let prompt = trimmed[1..].trim();
        let prompt_tokens: Vec<String> = prompt.split_whitespace().map(|s| s.to_string()).collect();
        return (CommandAction::Ai(prompt_tokens), None);
    }

    // First check if there's a pipeline
    let pipeline_parts = parse_pipeline(input);

    if pipeline_parts.len() > 1 {
        // Has pipeline, parse each part
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

    // First check if there are redirection operators
    let (command_part, redirection) = parse_redirection(input);

    // Parse the entire command line, get command and arguments
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
        "history" => {
            // Check if it's -r option (read history from file)
            if args.first().map(|s| s.as_str()) == Some("-r") {
                if let Some(path) = args.get(1) {
                    CommandAction::HistoryRead(path.clone())
                } else {
                    // -r option missing file path parameter
                    CommandAction::Unknown("history".to_string())
                }
            } else if args.first().map(|s| s.as_str()) == Some("-w") {
                // Check if it's -w option (write history to file)
                if let Some(path) = args.get(1) {
                    CommandAction::HistoryWrite(path.clone())
                } else {
                    // -w option missing file path parameter
                    CommandAction::Unknown("history".to_string())
                }
            } else if args.first().map(|s| s.as_str()) == Some("-a") {
                // Check if it's -a option (append new history to file)
                if let Some(path) = args.get(1) {
                    CommandAction::HistoryAppend(path.clone())
                } else {
                    // -a option missing file path parameter
                    CommandAction::Unknown("history".to_string())
                }
            } else {
                // Parse optional numeric parameter
                let limit = args.first().and_then(|s| s.parse::<usize>().ok());
                CommandAction::History(limit)
            }
        }
        _ => {
            // Check if in preloaded external command cache
            if all_executables.contains_key(command) {
                CommandAction::External(command.to_string(), args)
            } else {
                CommandAction::Unknown(command.to_string())
            }
        }
    };

    (action, redirection)
}

/// Parse pipeline: split commands by | but ignore | inside quotes
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
                // Found pipe, save current command
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

    // Add the last command
    if !current.trim().is_empty() {
        commands.push(current.trim().to_string());
    }

    // If no pipeline, return single command
    if commands.is_empty() {
        vec![input.to_string()]
    } else {
        commands
    }
}

/// Parse redirection operators, return command part and redirection info
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
        // Handle quote state
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
                    // Found redirection operator
                    chars.next(); // Consume '>'

                    // Check if it's append mode '>>'
                    let is_append = if chars.peek() == Some(&'>') {
                        chars.next(); // Consume second '>'
                        true
                    } else {
                        false
                    };

                    // Skip spaces
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }

                    // Get output filename (read until next redirect or end)
                    let file = parse_filename(&mut chars);

                    if !file.is_empty() {
                        stdout_file = Some(file);
                        stdout_append = is_append;
                    }
                    continue;
                }
                '1' if !in_single_quote && !in_double_quote => {
                    // Check if it's "1>" or "1>>" form
                    let mut temp_chars = chars.clone();
                    temp_chars.next(); // Skip '1'
                    if temp_chars.peek() == Some(&'>') {
                        chars.next(); // Consume '1'
                        chars.next(); // Consume '>'

                        // Check if it's append mode '1>>'
                        let is_append = if chars.peek() == Some(&'>') {
                            chars.next(); // Consume second '>'
                            true
                        } else {
                            false
                        };

                        // Skip spaces
                        while chars.peek() == Some(&' ') {
                            chars.next();
                        }

                        // Get output filename
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
                    // Check if it's "2>" or "2>>" form (stderr redirection)
                    let mut temp_chars = chars.clone();
                    temp_chars.next(); // Skip '2'
                    if temp_chars.peek() == Some(&'>') {
                        chars.next(); // Consume '2'
                        chars.next(); // Consume '>'

                        // Check if it's append mode '2>>'
                        let is_append = if chars.peek() == Some(&'>') {
                            chars.next(); // Consume second '>'
                            true
                        } else {
                            false
                        };

                        // Skip spaces
                        while chars.peek() == Some(&' ') {
                            chars.next();
                        }

                        // Get output filename
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

    // Build redirection info
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

/// Parse filename from character iterator (until space, redirect or end)
fn parse_filename(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut filename = String::new();

    while let Some(&ch) = chars.peek() {
        // Stop condition: space, redirection operator or special character
        if ch == ' ' || ch == '>' || ch == '1' || ch == '2' {
            // Check if it's the start of a redirection operator
            if ch == '1' || ch == '2' {
                let mut temp = chars.clone();
                temp.next();
                if temp.peek() == Some(&'>') {
                    // This is the next redirection operator, stop parsing
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

/// Handle specific logic for type command
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

/// Parse command line arguments, correctly handle quotes, spaces and escapes
///
/// Rules:
/// - Inside single quotes: all characters are literals, including double quotes and backslashes
/// - Inside double quotes: preserve spaces, single quotes treated as normal chars, \ only escapes " and \ itself
/// - Backslash outside quotes: escape next character, make it literal, backslash itself removed
/// - Consecutive spaces outside quotes treated as separators
/// - Adjacent quoted strings are concatenated (when no space separates them)
/// - Empty quotes are ignored
///
/// Returns: token array containing command and all arguments
fn parse_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false; // Track if in escape state (only outside quotes)

    while let Some(ch) = chars.next() {
        if escaped {
            // If previous character was backslash (outside quotes), current char is literal
            current_arg.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if in_double_quote => {
                // Backslash inside double quotes: only escape specific chars (", \)
                if let Some(&next_ch) = chars.peek() {
                    if next_ch == '"' || next_ch == '\\' {
                        // Escape: skip backslash, add escaped character
                        chars.next(); // Consume next character
                        current_arg.push(next_ch);
                    } else {
                        // Other characters: backslash as literal
                        current_arg.push('\\');
                    }
                } else {
                    // Backslash is last character, treat as literal
                    current_arg.push('\\');
                }
            }
            '\\' if !in_single_quote && !in_double_quote => {
                // Backslash outside quotes: set escape flag, backslash itself not added
                escaped = true;
            }
            '\'' if !in_double_quote => {
                // When not inside double quotes, toggle single quote state
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                // When not inside single quotes, toggle double quote state
                in_double_quote = !in_double_quote;
            }
            ' ' if !in_single_quote && !in_double_quote => {
                // Space outside quotes: if current arg is not empty, complete current arg
                if !current_arg.is_empty() {
                    args.push(current_arg.clone());
                    current_arg.clear();
                }
                // Skip consecutive spaces
            }
            _ => {
                // Other characters added directly to current arg (including all chars inside quotes)
                current_arg.push(ch);
            }
        }
    }

    // Handle last argument
    if !current_arg.is_empty() {
        args.push(current_arg);
    }

    args
}

/// Dynamic search logic (for type command)
fn find_command_in_path(command: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(command))
            .find(|full_path| is_executable(full_path))
    })
}

/// Preload all external commands (for execution validation)
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

/// Common check: whether path exists and has execute permission
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Save history to HISTFILE (if the environment variable is set)
fn save_history_to_histfile(history: &[String]) {
    if let Ok(histfile_path) = env::var("HISTFILE") {
        if let Ok(mut file) = File::create(&histfile_path) {
            for cmd in history {
                let _ = writeln!(file, "{}", cmd);
            }
        }
    }
}

/// Check if command is a builtin command
fn is_builtin(command: &str) -> bool {
    matches!(command, "echo" | "type" | "pwd" | "cd" | "exit" | "history")
}

/// Execute builtin command in child process
fn execute_builtin_in_child(command: &str, args: &[String]) {
    // For commands that don't use stdin (type, pwd), need to consume all stdin input
    // This avoids "Broken pipe" error from previous command when pipe is closed
    // Note: echo should not consume stdin as it only outputs arguments
    let should_consume_stdin = matches!(command, "type" | "pwd");

    if should_consume_stdin {
        // Read and discard all stdin data
        let mut buffer = [0u8; 8192];
        loop {
            match unsafe { libc::read(0, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) } {
                n if n > 0 => continue, // Continue reading
                _ => break,             // EOF or error, stop reading
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

/// Execute pipeline command
fn execute_pipeline(commands: Vec<(String, Vec<String>)>) -> io::Result<()> {
    if commands.is_empty() {
        return Ok(());
    }

    if commands.len() == 1 {
        // Only one command, execute directly
        let (command, args) = &commands[0];
        if is_builtin(command) {
            execute_builtin_in_child(command, args);
        } else {
            let _ = Command::new(command).args(args).status();
        }
        return Ok(());
    }

    // Create pipes and execute multiple commands
    let mut pipes: Vec<(i32, i32)> = Vec::new();

    // Create n-1 pipes (n is the number of commands)
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
                // Child process

                // Setup stdin: if not first command, read from previous pipe
                if i > 0 {
                    let (read_fd, _) = pipes[i - 1];
                    libc::dup2(read_fd, 0);
                }

                // Setup stdout: if not last command, write to next pipe
                if i < commands.len() - 1 {
                    let (_, write_fd) = pipes[i];
                    libc::dup2(write_fd, 1);
                }

                // Close all pipe file descriptors
                for (read_fd, write_fd) in &pipes {
                    libc::close(*read_fd);
                    libc::close(*write_fd);
                }

                if is_cmd_builtin {
                    // Execute builtin command
                    execute_builtin_in_child(command, args);
                    std::process::exit(0);
                } else {
                    // Execute external command
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
                    // If execvp returns, an error occurred
                    eprintln!("{}: command not found", command);
                    std::process::exit(127);
                }
            } else {
                // Parent process, record child process PID
                pids.push(pid);
            }
        }
    }

    // Parent process closes all pipes
    unsafe {
        for (read_fd, write_fd) in &pipes {
            libc::close(*read_fd);
            libc::close(*write_fd);
        }
    }

    // Wait for all child processes to complete
    for pid in pids {
        unsafe {
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        }
    }

    Ok(())
}

fn generate_command_with_ai(prompts: Vec<String>) {
    let prompt_text = prompts.join(" ");
    
    if prompt_text.trim().is_empty() {
        eprintln!("AI: Please provide a description of what you want to do");
        return;
    }

    // Create tokio runtime to run async code
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("AI: Failed to create async runtime: {}", e);
            return;
        }
    };

    // Call AI in async environment
    match runtime.block_on(async {
        // Check environment variable
        if env::var("OPENAI_API_KEY").is_err() {
            return Err("OPENAI_API_KEY environment variable not set".to_string());
        }

        // Create OpenAI client
        let client = openai::Client::from_env();

        // Create agent specifically for generating shell commands
        let agent = client
            .agent(openai::GPT_4O)
            .preamble(
                "You are a helpful shell command assistant. \
                 Given a natural language description, generate the appropriate shell command. \
                 Return ONLY the command itself without any explanation, markdown formatting, or code blocks. \
                 The command should be ready to execute directly in a bash/zsh shell."
            )
            .build();

        // Get current working directory as context
        let cwd = env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        // Build complete prompt
        let full_prompt = format!(
            "Current directory: {}\nTask: {}\nGenerate the shell command:",
            cwd, prompt_text
        );

        // Send request to AI
        let response = agent.prompt(&full_prompt).await
            .map_err(|e| format!("AI request failed: {}", e))?;

        Ok(response)
    }) {
        Ok(command) => {
            let command = command.trim();
            
            // Display AI generated command
            println!("AI suggested command:");
            println!("$ {}", command);
            println!();
            print!("Execute this command? (y/n): ");
            io::stdout().flush().unwrap();

            // Read user confirmation
            let stdin = io::stdin();
            let mut response = String::new();
            if stdin.lock().read_line(&mut response).is_ok() {
                let response = response.trim().to_lowercase();
                if response == "y" || response == "yes" {
                    println!("Executing...");
                    // Use sh -c to execute command, supporting pipes, redirects and other complex commands
                    let status = Command::new("sh")
                        .arg("-c")
                        .arg(command)
                        .status();
                    
                    match status {
                        Ok(exit_status) => {
                            if !exit_status.success() {
                                eprintln!("Command exited with status: {}", exit_status);
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to execute command: {}", e);
                        }
                    }
                } else {
                    println!("Command cancelled.");
                }
            }
        }
        Err(e) => {
            eprintln!("AI: {}", e);
        }
    }
}
