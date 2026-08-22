//! `fxrs teams` — choose the Vercel team used by AI Gateway (port of the
//! upstream `fx teams` command). Teams are persisted per-install in
//! `~/.fx/teams.json`; a best-effort remote fetch merges gateway teams into
//! the listing and always degrades gracefully when offline.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::fx_home;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub is_current: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TeamsStore {
    teams: Vec<Team>,
    current: Option<String>,
}

impl TeamsStore {
    pub fn path() -> PathBuf {
        fx_home().join("teams.json")
    }

    pub fn open() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
    }

    pub fn teams(&self) -> &[Team] {
        &self.teams
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn set_current(&mut self, id: &str) -> Result<()> {
        if !self.teams.iter().any(|t| t.id == id) {
            // Allow setting a team we have not listed yet (manual entry).
            self.teams.push(Team {
                id: id.to_string(),
                name: id.to_string(),
                slug: String::new(),
                is_current: true,
            });
        }
        for t in &mut self.teams {
            t.is_current = t.id == id;
        }
        self.current = Some(id.to_string());
        self.save()
    }

    pub fn upsert(&mut self, family: Vec<Team>) {
        // Remote teams become part of the local catalog without clobbering the
        // stored current selection.
        for team in family {
            if !self.teams.iter().any(|t| t.id == team.id) {
                self.teams.push(team);
            }
        }
        self.teams.sort_by(|a, b| a.id.cmp(&b.id));
    }
}

/// Best-effort remote fetch of gateway teams. Returns Ok(None) when the
/// gateway is unreachable or unconfigured (callers must degrade gracefully).
pub fn fetch_remote_teams() -> Result<Option<Vec<Team>>> {
    let base = crate::providers::gateway_base_url();
    if base.is_none() {
        return Ok(None);
    }
    let base = base.unwrap();
    let key = std::env::var("AI_GATEWAY_API_KEY").unwrap_or_default();
    let url = format!("{base}/coding-agent/v1/teams");
    let mut req = ureq::get(&url).timeout(std::time::Duration::from_secs(5));
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    match req.call() {
        Ok(resp) => {
            let body = resp.into_string()?;
            let parsed: serde_json::Value = serde_json::from_str(&body)?;
            let arr = parsed.get("teams").cloned().unwrap_or(parsed);
            let mut out = Vec::new();
            if let Some(rows) = arr.as_array() {
                for row in rows {
                    let id = row
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    out.push(Team {
                        id: id.clone(),
                        name: row
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&id)
                            .to_string(),
                        slug: row
                            .get("slug")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        is_current: false,
                    });
                }
            }
            Ok(Some(out))
        }
        Err(e) => {
            eprintln!("fxrs teams: gateway unreachable ({e}); showing local teams");
            Ok(None)
        }
    }
}

/// Run the `fxrs teams` CLI. `args` are everything after `teams`.
pub fn run_teams(args: &[String]) -> Result<i32> {
    let sub = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str());
    let wants_json = args.iter().any(|a| a == "--json");

    let mut store = TeamsStore::open()?;
    match sub {
        None | Some("list") | Some("ls") => {
            if let Ok(Some(remote)) = fetch_remote_teams() {
                store.upsert(remote);
                let _ = store.save();
            }
            let mut map: BTreeMap<String, &Team> = BTreeMap::new();
            for t in store.teams() {
                map.insert(t.id.clone(), t);
            }
            if wants_json {
                let arr: Vec<_> = map
                    .values()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "name": t.name,
                            "slug": t.slug,
                            "is_current": t.is_current,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                let current = store.current().unwrap_or_default();
                let mut any = false;
                for t in map.values() {
                    any = true;
                    if t.id == current {
                        println!("* {}\t{}", t.id, t.name);
                    } else {
                        println!("  {}\t{}", t.id, t.name);
                    }
                }
                if !any {
                    if current.is_empty() {
                        println!("no teams configured (run `fxrs teams set <id>`)");
                    } else {
                        println!("* {}", current);
                    }
                }
            }
        }
        Some("current") => {
            let current = store.current().unwrap_or_default();
            if wants_json {
                println!("{}", serde_json::json!({ "id": current }));
            } else if current.is_empty() {
                println!("no current team");
            } else {
                println!("{current}");
            }
        }
        Some("set") => {
            let id = args
                .iter()
                .skip_while(|a| a.as_str() != "set")
                .nth(1)
                .map(|s| s.to_string())
                .or_else(|| {
                    // support `fxrs teams set <id>` and `fxrs teams <id>`
                    args.first().filter(|a| !a.starts_with('-')).cloned()
                })
                .ok_or_else(|| anyhow::anyhow!("usage: fxrs teams set <id>"))?;
            store.set_current(&id)?;
            println!("current team: {id}");
        }
        Some(other) => {
            eprintln!("fxrs teams: unknown subcommand `{other}` (list | current | set <id>)");
            return Ok(2);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env;

    #[test]
    fn store_roundtrip_and_current() {
        test_env::with(|| {
            let _dir = std::env::temp_dir().join(format!("fxrs-teams-test-{}", std::process::id()));
            let mut store = TeamsStore::default();
            store.teams.push(Team {
                id: "team_a".into(),
                name: "Alpha".into(),
                slug: "alpha".into(),
                is_current: false,
            });
            store.set_current("team_a").unwrap();
            assert_eq!(store.current(), Some("team_a"));
            assert!(store.teams[0].is_current);
        });
    }

    #[test]
    fn upsert_merges_without_clobbering() {
        let mut store = TeamsStore::default();
        store.set_current("team_b").unwrap();
        store.upsert(vec![
            Team {
                id: "team_a".into(),
                name: "A".into(),
                slug: "a".into(),
                is_current: false,
            },
            Team {
                id: "team_b".into(),
                name: "B".into(),
                slug: "b".into(),
                is_current: false,
            },
        ]);
        assert_eq!(store.teams().len(), 2);
        assert_eq!(store.current(), Some("team_b"));
        // The remote row must not clobber the local current selection.
        assert!(store
            .teams()
            .iter()
            .any(|t| t.id == "team_b" && t.is_current));
        assert!(store.teams().iter().any(|t| t.id == "team_a"));
    }

    #[test]
    fn set_current_to_unknown_adds() {
        let mut store = TeamsStore::default();
        store.set_current("manual_team").unwrap();
        assert_eq!(store.teams().len(), 1);
        assert_eq!(store.current(), Some("manual_team"));
    }
}
