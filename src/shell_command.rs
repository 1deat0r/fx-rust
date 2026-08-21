//! Shell-command parsing and classification, modeled after fx's
//! `core/shell_command` module (command_lex / command_classification /
//! command_effect). The lexer is a pragmatic POSIX-ish tokenizer; the
//! classifier maps a command line onto read/write/network/destructive
//! effects that feed the permission runtime.

use std::path::Path;

// ------------------------------------------------------------------ lexing

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// Plain word (already unquoted/unescaped).
    Word(String),
    /// Shell operator: `|`, `||`, `&&`, `;`, `&`, `>`, `>>`, `<`, `2>`, `2>>`, `<<`.
    Operator(String),
    /// `VAR=value` appearing before the command (not passed to the command).
    Assignment(String, String),
}

/// Split a shell command line into tokens. Not a full POSIX parser: heredocs
/// are consumed up to their delimiter, `$()` and backticks are kept as whole
/// "command substitution" words, and comments are stripped when unquoted.
pub fn lex(line: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let s = line.trim_start();
    let mut rest = s;
    let mut pending_assignment: Option<(String, String)> = None;
    let mut first_word = true;

    while !rest.is_empty() {
        // Skip whitespace.
        let trimmed = rest.trim_start();
        let leading = rest.len() - trimmed.len();
        if leading > 0 {
            // A token boundary: flush a pending assignment as its own token.
            if let Some((k, v)) = pending_assignment.take() {
                out.push(Token::Assignment(k, v));
            }
            rest = trimmed;
            if rest.is_empty() {
                break;
            }
        }

        // Comment.
        if rest.starts_with('#') {
            break;
        }

        // Operators.
        let op = match rest {
            _ if rest.starts_with("||") => Some("||"),
            _ if rest.starts_with("&&") => Some("&&"),
            _ if rest.starts_with("2>>") => Some("2>>"),
            _ if rest.starts_with("2>") => Some("2>"),
            _ if rest.starts_with(">>") => Some(">>"),
            _ if rest.starts_with("<<") => Some("<<"),
            _ if rest.starts_with(">") => Some(">"),
            _ if rest.starts_with("<") => Some("<"),
            _ if rest.starts_with("|") => Some("|"),
            _ if rest.starts_with(";") => Some(";"),
            _ if rest.starts_with("&") => Some("&"),
            _ => None,
        };
        if let Some(op) = op {
            if let Some((k, v)) = pending_assignment.take() {
                out.push(Token::Assignment(k, v));
            }
            // A heredoc operator consumes until its delimiter on the next lines.
            if op == "<<" {
                let after = &rest[2..];
                if let Some(delim) = heredoc_delimiter(after) {
                    let (before, remaining) = consume_heredoc(rest, &delim);
                    out.push(Token::Operator(op.to_string()));
                    // Words preceding `<<` were already pushed; skip everything
                    // up to the heredoc body end.
                    let _ = before;
                    if let Some(rem) = remaining {
                        rest = rem.trim_start();
                        first_word = false;
                        continue;
                    }
                    out.clear(); // unterminated heredoc: treat as opaque
                    break;
                }
            }
            out.push(Token::Operator(op.to_string()));
            rest = &rest[op.len()..];
            first_word = true;
            continue;
        }

        // A word (possibly quoted). Gather until whitespace/operator.
        let (word, next) = lex_word(rest);
        if word.is_empty() {
            // Unrecognized char: skip one to guarantee progress.
            rest = &rest[1..];
            continue;
        }
        // VAR=value (left side is a valid identifier, no quoting on the key).
        if first_word {
            if let Some((k, v)) = word.split_once('=') {
                if is_identifier(k) && (!v.is_empty() || rest_needs_no_rhs(next, rest)) {
                    pending_assignment = Some((k.to_string(), v.to_string()));
                    rest = next;
                    continue;
                }
            }
        }
        first_word = false;
        out.push(Token::Word(word));
        rest = next;
    }
    if let Some((k, v)) = pending_assignment {
        out.push(Token::Assignment(k, v));
    }
    out
}

fn rest_needs_no_rhs(next: &str, _original: &str) -> bool {
    // `VAR=` with nothing after is still an assignment.
    next.is_empty()
        || next
            .trim_start()
            .starts_with([' ', '\t', '\n', '&', ';', '|', '#'])
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn heredoc_delimiter(after: &str) -> Option<String> {
    let after = after.trim_start();
    // Skip `-` (strip tabs), then read the delimiter word.
    let after = after.strip_prefix('-').unwrap_or(after).trim_start();
    let end = after
        .find([' ', '\t', '\n', '|', '&', ';', '>', '<'])
        .unwrap_or(after.len());
    let d = &after[..end];
    if d.is_empty() || d.contains('"') || d.contains('\'') {
        return None;
    }
    Some(d.to_string())
}

fn consume_heredoc<'a>(rest: &'a str, delim: &str) -> (&'a str, Option<&'a str>) {
    // Find the heredoc body start: after the `<<` line.
    let after_op = &rest[rest.find("<<").unwrap() + 2..];
    let line_end = after_op.find('\n').map(|i| i + 1).unwrap_or(after_op.len());
    let body_start = line_end;
    let body = &after_op[body_start..];
    let search_from = 0;
    if let Some(idx) = find_line(body, delim, search_from) {
        let consumed = &after_op[..body_start + idx + delim.len()];
        let remaining = &after_op[body_start + idx + delim.len()..];
        return (consumed, Some(remaining));
    }
    (rest, None)
}

fn find_line(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let mut start = from;
    while start <= haystack.len() {
        let line_end = haystack[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(haystack.len());
        let line = &haystack[start..line_end].trim();
        if *line == needle {
            return Some(start);
        }
        if line_end == haystack.len() {
            return None;
        }
        start = line_end + 1;
    }
    None
}

/// Read one word starting at `s`. Handles single/double quotes and backslash
/// escapes; returns (unquoted_word, rest_of_input).
fn lex_word(s: &str) -> (String, &str) {
    let mut out = String::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        let c_len = c.len_utf8();
        if !in_single && !in_double {
            if c == '\'' {
                in_single = true;
                i += c_len;
                continue;
            }
            if c == '"' {
                in_double = true;
                i += c_len;
                continue;
            }
            if c == '\\' && i + 1 < s.len() {
                let next = s[i + c_len..].chars().next().unwrap();
                out.push(next);
                i += c_len + next.len_utf8();
                continue;
            }
            if c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>' | '#') {
                break;
            }
            out.push(c);
            i += c_len;
        } else if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                out.push(c);
            }
            i += c_len;
        } else {
            // in_double
            if c == '"' {
                in_double = false;
                i += c_len;
            } else if c == '\\' && i + 1 < s.len() {
                let next = s[i + c_len..].chars().next().unwrap();
                if matches!(next, '"' | '\\' | '$' | '`') {
                    out.push(next);
                } else {
                    out.push('\\');
                    out.push(next);
                }
                i += c_len + next.len_utf8();
            } else {
                // Keep shell expansions verbatim inside double quotes.
                out.push(c);
                i += c_len;
            }
        }
    }
    // Unterminated quote: keep what we had and let callers treat it as a word.
    (out, &s[i..])
}

/// The first command of a pipeline and its arguments (before any `|`, `&&`,
/// `||`, `;`, `&`). Assignments are excluded from the argument list.
pub fn head_command(line: &str) -> (String, Vec<String>) {
    let tokens = lex(line);
    let mut words: Vec<String> = Vec::new();
    for t in &tokens {
        match t {
            Token::Word(w) => words.push(w.clone()),
            Token::Operator(op)
                if op == "|" || op == "&&" || op == "||" || op == ";" || op == "&" =>
            {
                break
            }
            Token::Operator(_) => {}
            Token::Assignment(..) => {}
        }
    }
    if words.is_empty() {
        (String::new(), Vec::new())
    } else {
        (words[0].clone(), words[1..].to_vec())
    }
}

// ------------------------------------------------------------ classification

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandClass {
    /// Anything the classifier does not know.
    #[default]
    Unknown,
    /// Read-only inspection (`ls`, `cat`, `grep`, `git status`, ...).
    ReadOnly,
    /// Mutates the filesystem (`cp`, `mv`, `rm`, `mkdir`, ...).
    Write,
    /// Talks to a network endpoint (`curl`, `wget`, `ssh`, `git fetch`, ...).
    Network,
    /// Package/tool installer (`npm`, `pip`, `brew`, `cargo`, ...).
    Package,
    /// Interactive or long-running process that would wedge an agent.
    Interactive,
    /// Destructive / privilege-escalating (`sudo`, `dd`, `mkfs`, `reboot`, ...).
    Dangerous,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandEffect {
    pub class: CommandClass,
    /// Whether the command writes to the filesystem.
    pub writes: bool,
    /// Whether the command opens a network connection.
    pub network: bool,
    /// Whether the command is inherently destructive/privilege-escalating.
    pub destructive: bool,
    /// Path-like arguments (best-effort, includes `-o`/`--output` values).
    pub paths: Vec<String>,
    /// The head binary before any prefixes (`sudo`, `command`, `env`).
    pub binary: String,
    /// Full head command path (first word of the command).
    pub raw: String,
}

fn classify_binary(bin: &str) -> CommandClass {
    let b = bin.rsplit('/').next().unwrap_or(bin);
    match b {
        "ls" | "cat" | "head" | "tail" | "grep" | "egrep" | "fgrep" | "find" | "pwd" | "echo"
        | "printf" | "date" | "which" | "file" | "wc" | "du" | "df" | "env" | "printenv"
        | "tree" | "rg" | "awk" | "sort" | "uniq" | "cut" | "tr" | "xargs" | "basename"
        | "dirname" | "realpath" | "readlink" | "stat" | "sha256sum" | "md5sum" | "cksum"
        | "diff" | "cmp" | "comm" | "paste" | "join" | "nl" | "fmt" | "fold" | "split" | "yes"
        | "false" | "true" | "id" | "whoami" | "hostname" | "uname" | "lscpu" | "free" | "ps"
        | "uptime" | "jobs" | "history" | "alias" | "type" | "time" | "nproc" | "getconf" => {
            CommandClass::ReadOnly
        }
        "mkdir" | "touch" | "cp" | "mv" | "rm" | "rmdir" | "ln" | "chmod" | "chown" | "install"
        | "tee" | "truncate" | "mkfifo" | "mktemp" | "unlink" | "shred" | "sync" => {
            CommandClass::Write
        }
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "rsync" | "nc" | "netcat" | "telnet"
        | "ping" | "traceroute" | "dig" | "host" | "nslookup" | "ftp" | "git" => {
            CommandClass::Network
        }
        "npm" | "yarn" | "pnpm" | "bun" | "pip" | "pip3" | "uv" | "poetry" | "cargo" | "brew"
        | "apt" | "apt-get" | "dpkg" | "dnf" | "yum" | "pacman" | "gem" | "composer" | "go"
        | "rustup" | "conda" | "mamba" => CommandClass::Package,
        "bash" | "sh" | "zsh" | "fish" | "python" | "python3" | "node" | "nodejs" | "ruby"
        | "perl" | "php" | "less" | "more" | "top" | "htop" | "vim" | "vi" | "nano" | "emacs"
        | "irb" | "rails" | "mysql" | "psql" | "sqlite3" | "redis-cli" | "mongosh" | "watch"
        | "clear" | "reset" | "man" | "info" | "bc" | "dc" | "gdb" | "lldb" | "strace" | "tar"
        | "gzip" | "gunzip" | "zip" | "unzip" | "xz" | "bz2" | "make" | "cmake" | "ninja"
        | "meson" => CommandClass::Interactive,
        "sudo" | "su" | "systemctl" | "service" | "systemd-run" | "kill" | "pkill" | "killall"
        | "reboot" | "shutdown" | "halt" | "poweroff" | "mkfs" | "mkfs.ext4" | "parted"
        | "fdisk" | "gdisk" | "iptables" | "ip6tables" | "ufw" | "firewall-cmd" | "passwd"
        | "chroot" | "nsenter" | "unshare" | "mount" | "umount" | "swapoff" | "rmmod"
        | "modprobe" | "insmod" | "sysctl" | "cron" | "at" | "batch" | "docker" | "podman"
        | "kubectl" | "helm" | "terraform" | "vault" | "base64" | "openssl" => {
            CommandClass::Dangerous
        }
        _ => CommandClass::Unknown,
    }
}

fn is_path_like(a: &str) -> bool {
    a.starts_with('.')
        || a.starts_with('/')
        || a.starts_with('~')
        || a.contains('/')
        || a.contains('.') // loose: treat dotted tokens as paths
}

/// Classify a full command line into its effects. This is heuristic, not a
/// shell: it looks at the head binary and a handful of flags/arguments.
pub fn classify(line: &str) -> CommandEffect {
    let (bin, args) = head_command(line);
    let mut eff = CommandEffect {
        raw: bin.clone(),
        binary: bin.clone(),
        ..Default::default()
    };
    if bin.is_empty() {
        eff.class = CommandClass::Unknown;
        return eff;
    }
    // Skip benign prefixes that do not change the effective binary.
    let mut effective = bin.clone();
    let mut it = args.iter();
    let mut pending_output: Option<String> = None;
    let mut paths = Vec::new();
    for a in it.by_ref() {
        match a.as_str() {
            "sudo" | "command" | "exec" | "nohup" | "nice" | "time" if effective == bin => {
                effective = a.clone();
                continue;
            }
            _ => {}
        }
        if let Some(flag) = pending_output.take() {
            if !a.is_empty() {
                paths.push(flag);
            }
            if is_path_like(a) {
                paths.push(a.clone());
            }
            continue;
        }
        if a == "-o"
            || a == "--output"
            || a == "-t"
            || a == "--target"
            || a == "-d"
            || a == "--directory"
        {
            pending_output = Some(a.clone());
            continue;
        }
        if is_path_like(a) {
            paths.push(a.clone());
        }
    }
    eff.paths = paths;
    eff.binary = effective.trim().to_string();

    let base = eff
        .binary
        .rsplit('/')
        .next()
        .unwrap_or(&eff.binary)
        .to_string();
    match base.as_str() {
        "git" => {
            // Refine git: some subcommands are read-only, others write/network.
            match args.first().map(|s| s.as_str()) {
                Some(
                    "status" | "log" | "diff" | "show" | "branch" | "tag" | "remote" | "rev-parse"
                    | "ls-files" | "ls-tree" | "config" | "help" | "blame" | "grep" | "whatchanged"
                    | "describe" | "shortlog" | "check-ignore",
                ) => {
                    eff.class = CommandClass::ReadOnly;
                }
                Some("clone" | "fetch" | "pull" | "push" | "submodule" | "ls-remote") => {
                    eff.class = CommandClass::Network;
                    eff.network = true;
                    if args.first().map(|s| s.as_str()) == Some("push") {
                        eff.destructive = true;
                    }
                }
                Some(
                    "commit" | "add" | "mv" | "rm" | "reset" | "rebase" | "merge" | "cherry-pick"
                    | "checkout" | "switch" | "restore" | "stash" | "clean" | "revert",
                ) => {
                    eff.class = CommandClass::Write;
                    eff.writes = true;
                    if matches!(
                        args.first().map(|s| s.as_str()),
                        Some("clean" | "reset" | "rebase")
                    ) {
                        eff.destructive = true;
                    }
                }
                _ => {
                    eff.class = CommandClass::Write;
                    eff.writes = true;
                }
            }
        }
        "sed" => {
            // sed without -i is read-only in practice (prints to stdout).
            let inplace = args.iter().any(|a| a == "-i" || a.starts_with("-i"));
            if inplace {
                eff.class = CommandClass::Write;
                eff.writes = true;
            } else {
                eff.class = CommandClass::ReadOnly;
            }
        }
        "curl" => {
            eff.network = true;
            let writes_body = args.iter().any(|a| {
                a == "-X"
                    || a.starts_with("-X")
                    || a == "--data"
                    || a.starts_with("--data")
                    || a == "-d"
                    || a == "--upload-file"
                    || a == "-F"
                    || a == "--form"
                    || a == "-T"
                    || a == "--request"
            }) || args.iter().any(|a| a.starts_with("-X") && a != "-X");
            let output_file = args
                .iter()
                .any(|a| a == "-o" || a == "-O" || a == "--output");
            // Downloading to disk is a network write: it must not fall through
            // the read-only auto-allow path.
            eff.class = if writes_body || output_file {
                CommandClass::Network
            } else {
                CommandClass::ReadOnly
            };
            eff.writes = output_file;
        }
        "tee" => {
            eff.class = CommandClass::Write;
            eff.writes = true;
        }
        "dd" => {
            // dd can write raw devices; treat as destructive unless confined.
            eff.class = CommandClass::Dangerous;
            eff.writes = true;
            eff.destructive = true;
        }
        "tar" | "gzip" | "gunzip" | "zip" | "unzip" | "xz" | "bzip2" | "make" | "cmake"
        | "ninja" | "meson" => {
            eff.class = CommandClass::Interactive; // may be long-running/write; not auto-allowed
            eff.writes = true;
        }
        "go" => {
            // go build/test are fine to classify as Package-ish writes.
            eff.class = CommandClass::Package;
            eff.writes = true;
        }
        "cargo" => match args.first().map(|s| s.as_str()) {
            Some("build" | "check" | "test" | "fmt" | "clippy") => {
                eff.class = CommandClass::Write;
                eff.writes = true;
            }
            Some("install" | "add" | "update" | "publish") => {
                eff.class = CommandClass::Package;
                eff.network = true;
                eff.writes = true;
            }
            _ => {
                eff.class = CommandClass::Package;
                eff.writes = true;
            }
        },
        "docker" | "podman" | "kubectl" | "terraform" | "vault" | "helm" => {
            eff.class = CommandClass::Dangerous;
            eff.writes = true;
            eff.destructive = true;
        }
        "python" | "python3" | "node" | "ruby" | "perl" | "php" => {
            // A one-shot `python -c '...'` is common in agent flows; still
            // considered interactive/spawning by default.
            eff.class = CommandClass::Interactive;
        }
        _ => {
            let cls = classify_binary(&base);
            eff.class = cls;
            match cls {
                CommandClass::ReadOnly => {}
                CommandClass::Write => {
                    eff.writes = true;
                }
                CommandClass::Network => {
                    eff.network = true;
                }
                CommandClass::Dangerous => {
                    eff.writes = true;
                    eff.destructive = true;
                }
                CommandClass::Package | CommandClass::Interactive | CommandClass::Unknown => {}
            }
        }
    }
    eff
}

/// Convenience: is this command line safe to auto-allow as read-only?
pub fn is_read_only(line: &str) -> bool {
    matches!(classify(line).class, CommandClass::ReadOnly)
}

/// Best-effort: does the command line reference a path outside `base`?
/// Returns the first offending path argument, if any.
pub fn path_outside(line: &str, base: &Path) -> Option<String> {
    let eff = classify(line);
    for p in &eff.paths {
        let expanded = p.replace('~', "");
        let cand = Path::new(&expanded);
        if cand.is_absolute() && !cand.starts_with(base) {
            return Some(p.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_basic_words_and_ops() {
        let toks = lex("echo hello world");
        assert_eq!(
            toks,
            vec![
                Token::Word("echo".into()),
                Token::Word("hello".into()),
                Token::Word("world".into())
            ]
        );
    }

    #[test]
    fn lex_quotes_and_escapes() {
        let toks = lex(r#"echo "a b" 'c d' e\ f"#);
        assert_eq!(toks.len(), 4);
        assert_eq!(toks[1], Token::Word("a b".into()));
        assert_eq!(toks[2], Token::Word("c d".into()));
        assert_eq!(toks[3], Token::Word("e f".into()));
    }

    #[test]
    fn lex_assignments_before_command() {
        let toks = lex("FOO=bar BAZ=qux echo hi");
        assert_eq!(toks[0], Token::Assignment("FOO".into(), "bar".into()));
        assert_eq!(toks[1], Token::Assignment("BAZ".into(), "qux".into()));
        assert_eq!(toks[2], Token::Word("echo".into()));
    }

    #[test]
    fn head_command_pipeline() {
        let (bin, args) = head_command("ls -la | grep src && echo done");
        assert_eq!(bin, "ls");
        assert_eq!(args, vec!["-la"]);
    }

    #[test]
    fn classify_read_only() {
        assert_eq!(classify("git status").class, CommandClass::ReadOnly);
        assert_eq!(classify("grep -rn foo src").class, CommandClass::ReadOnly);
        assert!(is_read_only("cat Cargo.toml"));
    }

    #[test]
    fn classify_writes() {
        assert_eq!(classify("cp a.txt b.txt").class, CommandClass::Write);
        assert!(classify("cp a.txt b.txt").writes);
        assert!(classify("sed -i 's/x/y/' f.txt").writes);
        assert!(!classify("sed 's/x/y/' f.txt").writes);
    }

    #[test]
    fn curl_download_to_disk_is_network_write() {
        let c = classify("curl -o /tmp/x https://example.com/data");
        assert_eq!(c.class, CommandClass::Network);
        assert!(c.writes);
    }

    #[test]
    fn classify_network() {
        let c = classify("curl https://example.com");
        assert!(c.network);
        assert_eq!(c.class, CommandClass::ReadOnly);
        assert!(classify("curl -X POST -d '{}' https://x").network);
        assert!(classify("git push origin main").destructive);
        assert!(classify("git clone https://github.com/x/y").network);
    }

    #[test]
    fn classify_dangerous() {
        assert!(classify("sudo apt update").destructive);
        assert_eq!(
            classify("dd if=/dev/zero of=/dev/sda").class,
            CommandClass::Dangerous
        );
        assert_eq!(classify("rm -rf /").class, CommandClass::Write);
    }

    #[test]
    fn heredoc_consumed() {
        let toks = lex("cat << EOF\nhello\nEOF\n");
        assert_eq!(toks[0], Token::Word("cat".into()));
        assert_eq!(toks[1], Token::Operator("<<".into()));
    }
}
