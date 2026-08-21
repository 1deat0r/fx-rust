//! Slash-command router + catalog (fx's `core/slash_commands`): the in-shell
//! command surface (`/help`, `/sessions`, `/usage`, ...). The router parses a
//! line into a typed command; `catalog` drives help output and completion.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    History(Option<String>),
    Settings,
    Help,
    Exit,
    Clear,
    Version,
    Status,
    Model,
    Permissions,
    Sessions,
    Session(Option<String>),
    Resume(Option<String>),
    Trace,
    Feedback,
    Usage(Option<String>),
    Doctor,
    Setup,
    Compact,
    Workspace,
    Background(Option<String>),
    Login,
    Logout,
    /// Unknown / unimplemented slash command (kept so the shell can explain).
    Unknown(String),
}

/// Static catalog of every slash command we route (upstream-slash parity +
/// local extras). `ready` is false for commands whose subsystem is not part of
/// the ported surface yet (they still route, but the shell tells the user).
#[derive(Debug, Clone)]
pub struct Spec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub description: &'static str,
    pub ready: bool,
}

pub fn catalog() -> &'static [Spec] {
    &[
        Spec {
            name: "help",
            aliases: &["h"],
            usage: "/help",
            description: "show this help",
            ready: true,
        },
        Spec {
            name: "exit",
            aliases: &["quit", "q"],
            usage: "/exit",
            description: "leave the shell",
            ready: true,
        },
        Spec {
            name: "clear",
            aliases: &["cls"],
            usage: "/clear",
            description: "clear the screen",
            ready: true,
        },
        Spec {
            name: "version",
            aliases: &["v"],
            usage: "/version",
            description: "version info",
            ready: true,
        },
        Spec {
            name: "status",
            aliases: &["s"],
            usage: "/status",
            description: "model / permissions / workspace",
            ready: true,
        },
        Spec {
            name: "model",
            aliases: &[],
            usage: "/model",
            description: "show current model",
            ready: true,
        },
        Spec {
            name: "permissions",
            aliases: &["perm"],
            usage: "/permissions",
            description: "show permission mode + rules",
            ready: true,
        },
        Spec {
            name: "sessions",
            aliases: &["ls"],
            usage: "/sessions",
            description: "list sessions for this workspace",
            ready: true,
        },
        Spec {
            name: "session",
            aliases: &[],
            usage: "/session [id]",
            description: "show a session (default latest)",
            ready: true,
        },
        Spec {
            name: "resume",
            aliases: &["r"],
            usage: "/resume [last|<id>]",
            description: "resume a session",
            ready: true,
        },
        Spec {
            name: "usage",
            aliases: &["credits"],
            usage: "/usage [24h|7d|30d|all]",
            description: "token usage / cost summary",
            ready: true,
        },
        Spec {
            name: "doctor",
            aliases: &[],
            usage: "/doctor",
            description: "run config + environment diagnostics",
            ready: true,
        },
        Spec {
            name: "history",
            aliases: &[],
            usage: "/history",
            description: "recent prompt history (tap /history 50)",
            ready: true,
        },
        Spec {
            name: "settings",
            aliases: &[],
            usage: "/settings",
            description: "show config catalog + effective settings",
            ready: true,
        },
        Spec {
            name: "setup",
            aliases: &[],
            usage: "/setup",
            description: "provider configuration guide",
            ready: true,
        },
        Spec {
            name: "trace",
            aliases: &[],
            usage: "/trace",
            description: "toggle tool-call tracing",
            ready: true,
        },
        Spec {
            name: "feedback",
            aliases: &[],
            usage: "/feedback",
            description: "report an issue",
            ready: true,
        },
        Spec {
            name: "compact",
            aliases: &[],
            usage: "/compact",
            description: "compact the conversation context",
            ready: false,
        },
        Spec {
            name: "workspace",
            aliases: &[],
            usage: "/workspace",
            description: "workspace + AGENTS.md info",
            ready: true,
        },
        Spec {
            name: "background",
            aliases: &["bg"],
            usage: "/background [supervise | tree <id> | get <id> | stop <id> | stop-tree <id>]",
            description: "list, supervise, tree, inspect, or stop background processes",
            ready: true,
        },
        Spec {
            name: "login",
            aliases: &[],
            usage: "/login",
            description: "authenticate a provider",
            ready: false,
        },
        Spec {
            name: "logout",
            aliases: &[],
            usage: "/logout",
            description: "clear provider credentials",
            ready: false,
        },
    ]
}

/// True when `line` starts with a slash (typed or pasted).
pub fn is_slash(line: &str) -> bool {
    line.starts_with('/')
}

/// Parse a slash line into a typed command. Unknown commands route to
/// `Unknown(name)` including any args.
pub fn parse(line: &str) -> Option<Slash> {
    let l = line.trim();
    if !l.starts_with('/') {
        return None;
    }
    let rest = l[1..].trim();
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n.trim().to_ascii_lowercase(), Some(a.trim().to_string())),
        None => (rest.to_ascii_lowercase(), None),
    };
    let dispatch = |canonical: &str| match canonical {
        "settings" => Slash::Settings,
        "help" => Slash::Help,
        "exit" | "quit" | "q" => Slash::Exit,
        "clear" | "cls" => Slash::Clear,
        "version" | "v" => Slash::Version,
        "status" | "s" => Slash::Status,
        "model" => Slash::Model,
        "permissions" | "perm" => Slash::Permissions,
        "sessions" | "ls" => Slash::Sessions,
        "session" => Slash::Session(arg.clone()),
        "resume" | "r" => Slash::Resume(arg.clone()),
        "usage" | "credits" => Slash::Usage(arg.clone()),
        "history" => Slash::History(arg.clone()),
        "doctor" => Slash::Doctor,
        "setup" => Slash::Setup,
        "trace" => Slash::Trace,
        "feedback" => Slash::Feedback,
        "compact" => Slash::Compact,
        "workspace" => Slash::Workspace,
        "background" | "bg" => Slash::Background(arg.clone()),
        "login" => Slash::Login,
        "logout" => Slash::Logout,
        other => Slash::Unknown(other.to_string()),
    };
    for spec in catalog() {
        if spec.name == name || spec.aliases.contains(&name.as_str()) {
            return Some(dispatch(spec.name));
        }
    }
    Some(Slash::Unknown(name))
}

/// Human-readable help listing (used by the shell's `/help`).
pub fn render_help() -> String {
    let mut out = String::from("commands:");
    for spec in catalog() {
        let ready = if spec.ready { "" } else { " (not ready)" };
        out.push_str(&format!(
            "\n  \x1b[32m{}\x1b[0m  {}{}",
            spec.usage, spec.description, ready
        ));
    }
    out.push_str(
        "\n\nanything else is sent to the model as a prompt. Ctrl-C during a turn interrupts.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(parse("/help"), Some(Slash::Help));
        assert_eq!(parse("/q"), Some(Slash::Exit));
        assert_eq!(parse("/sessions"), Some(Slash::Sessions));
        assert_eq!(
            parse("/session abc123"),
            Some(Slash::Session(Some("abc123".into())))
        );
        assert_eq!(
            parse("/resume last"),
            Some(Slash::Resume(Some("last".into())))
        );
        assert_eq!(parse("/usage 7d"), Some(Slash::Usage(Some("7d".into()))));
        assert_eq!(parse("/doctor"), Some(Slash::Doctor));
    }

    #[test]
    fn unknown_routes_safely() {
        assert_eq!(
            parse("/gibberish x y"),
            Some(Slash::Unknown("gibberish".into()))
        );
        assert_eq!(parse("hello world"), None);
        assert_eq!(parse("/  "), Some(Slash::Unknown("".into())));
    }

    #[test]
    fn catalog_has_no_duplicate_names() {
        let names: Vec<_> = catalog().iter().map(|s| s.name).collect();
        let mut uniq = names.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(names.len(), uniq.len());
    }
}
