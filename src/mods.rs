//! Mods — faithful port of upstream `core/mods/registry.zig`.
//!
//! Two tiny generic registries:
//!
//! * `ToolRegistry` — tool-like capabilities keyed by stable public name.
//! * `CommandRegistry` — command-like capabilities keyed by a public command
//!   token plus aliases, with exact-match (trailing-space tolerant) and
//!   entry-prefix matching used by the slash/command dispatcher.

/// Registry for tool-like capabilities keyed by stable public name.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry<T> {
    pub tools: Vec<T>,
}

impl<T> ToolRegistry<T> {
    pub fn lookup(&self, name: &str) -> Option<&T>
    where
        T: HasName,
    {
        self.tools.iter().find(|t| t.name() == name)
    }
}

pub trait HasName {
    fn name(&self) -> &str;
}

impl HasName for &'static str {
    fn name(&self) -> &str {
        self
    }
}

/// Registry for command-like capabilities keyed by a public command token
/// and aliases.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry<T> {
    pub commands: Vec<T>,
}

#[derive(Debug, Clone, Copy)]
pub struct Match<'a, T> {
    pub command: &'a T,
    pub token: &'a str,
}

pub trait HasCommands {
    fn command(&self) -> &str;
    fn aliases(&self) -> &[&str];
}

impl HasCommands for (&'static str, &'static [&'static str]) {
    fn command(&self) -> &str {
        self.0
    }
    fn aliases(&self) -> &[&str] {
        self.1
    }
}

impl<T: HasCommands> CommandRegistry<T> {
    pub fn lookup(&self, command: &str) -> Option<&T> {
        self.match_exact(command).map(|m| m.command)
    }

    /// Exact match, tolerant of trailing spaces/tabs (upstream
    /// `matchesCommandToken`).
    pub fn match_exact(&self, command: &str) -> Option<Match<'_, T>> {
        for entry in &self.commands {
            if matches_command_token(command, entry.command()) {
                return Some(Match {
                    command: entry,
                    token: entry.command(),
                });
            }
            for alias in entry.aliases() {
                if matches_command_token(command, alias) {
                    return Some(Match {
                        command: entry,
                        token: alias,
                    });
                }
            }
        }
        None
    }

    /// If `entry` is the registry entry whose token is a prefix of `command`,
    /// return the matched token (upstream `matchEntryPrefix`).
    pub fn match_entry_prefix(&self, command: &str, entry: &T) -> Option<&str> {
        for candidate in &self.commands {
            if !std::ptr::eq(candidate as *const T, entry as *const T) {
                continue;
            }
            if starts_with_command_prefix(command, candidate.command()) {
                return Some(candidate.command());
            }
            for alias in candidate.aliases() {
                if starts_with_command_prefix(command, alias) {
                    return Some(alias);
                }
            }
            return None;
        }
        None
    }
}

fn matches_command_token(input: &str, command: &str) -> bool {
    if input == command {
        return true;
    }
    if input.is_empty() {
        return false;
    }
    let last = input.chars().last().unwrap();
    if last != ' ' && last != '\t' {
        return false;
    }
    input.trim_end_matches([' ', '\t']) == command
}

fn starts_with_command_prefix(input: &str, command: &str) -> bool {
    if !input.starts_with(command) {
        return false;
    }
    let Some(next) = input.chars().nth(command.chars().count()) else {
        return true;
    };
    next == ' ' || next == '\t'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_registry_lookup_returns_hit_and_miss() {
        let registry = ToolRegistry {
            tools: vec!["read_file", "run_command"],
        };
        assert_eq!(registry.lookup("run_command"), Some(&"run_command"));
        assert_eq!(registry.lookup("missing"), None);
    }

    #[test]
    fn command_registry_lookup_and_prefix_matching_use_aliases() {
        let registry = CommandRegistry {
            commands: vec![("/model", &["/m"][..]), ("/models", &[][..])],
        };
        let alias = registry.lookup("/m  ").expect("alias lookup");
        assert_eq!(alias.command(), "/model");

        let entry = &registry.commands[0];
        let prefixed = registry
            .match_entry_prefix("/m model-id", entry)
            .expect("prefix");
        assert_eq!(prefixed, "/m");
        assert!(registry.match_entry_prefix("/m\nmodel-id", entry).is_none());
        assert!(registry.lookup("/missing").is_none());
    }

    #[test]
    fn command_registry_matches_a_specific_entry_prefix_when_commands_overlap() {
        let registry = CommandRegistry {
            commands: vec![("/background", &[][..]), ("/background stop", &[][..])],
        };
        let entry = &registry.commands[1];
        let matched = registry
            .match_entry_prefix("/background stop last", entry)
            .expect("prefix match");
        assert_eq!(matched, "/background stop");

        // A foreign entry (not in this registry) never matches.
        let foreign = ("/background stop", &[][..]);
        assert!(registry
            .match_entry_prefix("/background stop last", &foreign)
            .is_none());
    }
}
