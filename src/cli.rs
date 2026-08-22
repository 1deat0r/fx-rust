//! CLI surface: `fxrs [command]` — mirrors fx's Unix-shell CLI.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _, Result};

use crate::agent::{AgentOutput, AgentRequest, FinishReason};
use crate::config;
use crate::sessions::SessionStore;
use crate::ui::QuietHuman;

pub async fn run_main(args: Vec<String>) -> Result<i32> {
    let mut args = args.iter().skip(1); // drop argv[0]
    let cmd = args.next();

    match cmd.map(|s| s.as_str()) {
        None | Some("") | Some("repl") => {
            if std::io::stdin().is_terminal() {
                let cfg = Arc::new(config::resolve(&cwd())?);
                let store = SessionStore::new()?;
                crate::ui::run_interactive(cfg, &store, None, false).await?;
                Ok(0)
            } else {
                // Non-TTY stdin: treat piped input as a one-shot prompt,
                // mirroring fx (echo "..." | fxrs). A bare pipe with no
                // content gets a hint instead of a silent exit.
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                let prompt = buf.trim().to_string();
                if prompt.is_empty() {
                    eprintln!("fxrs: no interactive terminal and no piped prompt");
                    eprintln!("       open a real terminal to start the shell, or use:");
                    eprintln!("         fxrs ask '<prompt>'      one-shot prompt");
                    eprintln!("         echo '<prompt>' | fxrs  piped one-shot");
                    return Ok(0);
                }
                run_ask(&[prompt]).await
            }
        }
        Some("ask") | Some("a") => {
            let rest: Vec<String> = args.map(|s| s.to_string()).collect();
            run_ask(&rest).await
        }
        Some("resume") | Some("r") => {
            let id = args.next().map(|s| s.to_string());
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            let id = match id {
                Some(i) if i == "last" => None,
                Some(i) => Some(i),
                None => None,
            };
            crate::ui::run_interactive(cfg, &store, id, false).await?;
            Ok(0)
        }
        Some("sessions") => {
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            let sessions = store.list(Some(&cfg.workspace))?;
            let wants_json = args.clone().any(|a| a == "--json");
            if wants_json {
                let arr: Vec<_> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id,
                            "workspace": s.workspace,
                            "updated_ms": s.updated_ms,
                            "model": s.model,
                            "messages": s.messages,
                            "last_text": s.last_text,
                            "tokens": s.tokens,
                            "tool_calls": s.tool_calls,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else if sessions.is_empty() {
                println!("no sessions");
            } else {
                let mut iter: Vec<_> = sessions;
                if let Some(term) = args
                    .clone()
                    .find(|a| a.as_str() == "--search")
                    .and_then(|_| args.clone().skip_while(|a| a.as_str() != "--search").nth(1))
                {
                    let q = term.to_lowercase();
                    iter.retain(|s| {
                        s.id.to_lowercase().contains(&q)
                            || s.model.to_lowercase().contains(&q)
                            || s.last_text.to_lowercase().contains(&q)
                    });
                }
                for s in iter {
                    println!(
                        "{}\t{}\t{}\t{} msgs\t{}k tok\t{}s\t{}",
                        s.id,
                        s.updated_ms,
                        s.model,
                        s.messages,
                        s.tokens / 1000,
                        s.duration_ms / 1000,
                        s.last_text,
                    );
                }
            }
            Ok(0)
        }
        Some("session") => {
            let mut id = args.next().map(|s| s.to_string());
            let wants_json = args.clone().any(|a| a == "--json");
            let wants_delete = args.clone().any(|a| a == "--delete");
            let cfg = Arc::new(config::resolve(&cwd())?);
            let store = SessionStore::new()?;
            // `session last` / `session latest` resolve via the latest pointer.
            if id
                .as_deref()
                .map(|s| s == "last" || s == "latest")
                .unwrap_or(false)
            {
                id = store.latest(&cfg.workspace)?.map(|s| s.id);
            }
            let id = id.ok_or_else(|| {
                anyhow::anyhow!("usage: fxrs session <id|last> [--json] [--delete]")
            })?;
            if wants_delete {
                match store.delete(&cfg.workspace, &id)? {
                    true => println!("deleted session {id}"),
                    false => bail!("no session `{id}`"),
                }
                return Ok(0);
            }
            match store.load(&cfg.workspace, &id)? {
                Some(sess) => {
                    if wants_json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "id": sess.id,
                                "workspace": sess.workspace,
                                "created_ms": sess.created_ms,
                                "updated_ms": sess.updated_ms,
                                "model": sess.model,
                                "mode": sess.mode.to_string(),
                                "interactive": sess.interactive,
                                "schema_version": sess.schema_version,
                                "usage": {
                                    "input_tokens": sess.usage.input_tokens,
                                    "output_tokens": sess.usage.output_tokens,
                                    "total_tokens": sess.usage.total_tokens,
                                    "cost_usd": sess.usage.cost_usd,
                                    "steps": sess.usage.steps,
                                    "tool_calls": sess.usage.tool_calls,
                                },
                                "messages": sess.messages.len(),
                                "grants": sess.grants,
                            }))?
                        );
                    } else {
                        println!("id: {}", sess.id);
                        println!("workspace: {}", sess.workspace);
                        println!(
                            "created: {} ({}s duration)",
                            sess.created_ms,
                            sess.updated_ms.saturating_sub(sess.created_ms) / 1000
                        );
                        println!("updated_ms: {}", sess.updated_ms);
                        println!("model: {}", sess.model);
                        println!("mode: {:?}", sess.mode);
                        println!("interactive: {}", sess.interactive);
                        println!("schema_version: {}", sess.schema_version);
                        println!(
                            "usage: {}k tokens in / {}k out ({} total) · {} tool calls · ${:.4}",
                            sess.usage.input_tokens / 1000,
                            sess.usage.output_tokens / 1000,
                            sess.usage.total_tokens,
                            sess.usage.tool_calls,
                            sess.usage.cost_usd,
                        );
                        for m in &sess.messages {
                            println!("-- {}: {}", m.role_str(), m.last_text().unwrap_or_default());
                        }
                    }
                }
                None => bail!("no session `{id}`"),
            }
            Ok(0)
        }
        Some("status") => {
            let cfg = config::resolve(&cwd())?;
            println!("fxrs {}", crate::version::VERSION);
            println!("workspace: {}", cfg.workspace.display());
            println!("model: {}", cfg.model);
            println!("permission mode: {:?}", cfg.permission_mode.to_string());
            println!("max_agent_steps: {}", cfg.max_agent_steps);
            println!("max_tool_result_bytes: {}", cfg.max_tool_result_bytes);
            println!("context: {}", cfg.context);
            println!("sandbox: {:?}", cfg.sandbox);
            Ok(0)
        }
        Some("upgrade") => {
            let current = crate::upgrade::version_tag();
            println!("current version: {current}");
            match crate::upgrade::latest_release() {
                Ok(Some(latest)) if latest.trim_start_matches('v') != crate::version::VERSION => {
                    println!("latest release:  {latest}");
                    println!(
                        "run `fxrs upgrade --install` to rebuild from source and install in PATH"
                    );
                }
                Ok(Some(latest)) => println!("already up to date ({latest})"),
                Ok(None) => println!("no releases found yet on GitHub; nothing to upgrade"),
                Err(e) => println!("could not check for updates: {e:#}"),
            }
            let wants_install = args.clone().any(|a| a.as_str() == "--install");
            if wants_install {
                crate::upgrade::install_from_git()?;
                println!("fxrs upgraded");
            }
            Ok(0)
        }
        Some("mcp") => {
            let cfg = config::resolve(&cwd())?;
            let wants_json = args.clone().any(|a| a == "--json");
            if cfg.mcp_servers.is_empty() {
                if wants_json {
                    println!("{}", serde_json::json!({ "servers": [], "tools": [] }));
                } else {
                    println!("no MCP servers configured");
                    println!("add an 'mcpServers' array to ~/.fx/settings.json or .fx.json");
                    println!("  settings.json: mcpServers: [{{ name, command|transport, args, env, url, headers }}]");
                    println!("example: npx -y @modelcontextprotocol/server-fetch");
                    println!("remote:  {{ name, transport: \"http\"|\"sse\", url, headers, bearer_token_env }}");
                }
                return Ok(0);
            }
            let discovery = crate::mcp::discover(&cfg.mcp_servers);
            if wants_json {
                let servers: Vec<_> = discovery
                    .states
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "transport": s.transport,
                            "enabled": s.enabled,
                            "state": s.availability.as_str(),
                            "tools": s.tool_count,
                            "error": s.error,
                        })
                    })
                    .collect();
                let tools: Vec<_> = discovery
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": crate::mcp::prefixed_name(&t.server, &t.name),
                            "server": t.server,
                            "description": t.description,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "servers": servers, "tools": tools })
                );
                return Ok(0);
            }
            for s in &discovery.states {
                let state = s.availability.as_str();
                if let Some(err) = &s.error {
                    println!("{} [{}] ({}) {}", s.name, s.transport, state, err);
                } else {
                    println!("{} [{}] ({} tools)", s.name, s.transport, s.tool_count);
                }
                for t in discovery.tools.iter().filter(|t| t.server == s.name) {
                    println!(
                        "  {} -- {}",
                        t.name,
                        t.description.lines().next().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        Some("hooks") => {
            let cfg = config::resolve(&cwd())?;
            use crate::hooks::discover;
            for def in crate::hooks::definitions().iter() {
                println!(
                    "\x1b[1m{}\x1b[0m — {}",
                    def.lifecycle_event, def.agent_loop_point
                );
                println!("  purpose: {}", def.purpose);
                let found = discover(def.kind, &cfg.workspace);
                if found.is_empty() {
                    println!("  scripts: (none)");
                } else {
                    for f in found {
                        println!("  script: {}", f.display());
                    }
                }
                println!();
            }
            Ok(0)
        }
        Some("permissions") => {
            let cfg = config::resolve(&cwd())?;
            println!("permission mode: {:?}", cfg.permission_mode.to_string());
            println!(
                "rules: {}",
                if cfg.permission_rules.is_empty() {
                    "(default)"
                } else {
                    "see settings"
                }
            );
            for (k, v) in &cfg.permission_rules {
                println!("  {k}: {v:?}");
            }
            Ok(0)
        }
        Some("auth") | Some("login") => {
            let args: Vec<String> = args.map(|s| s.to_string()).collect();
            let sub = args.first().map(|s| s.as_str());
            let is_login = cmd.map(|s| s.as_str()) == Some("login");
            match (is_login, sub) {
                (true, _) | (_, Some("add")) | (_, Some("set")) => {
                    let mut provider = args
                        .iter()
                        .skip(1)
                        .find(|a| !a.starts_with('-'))
                        .cloned()
                        .unwrap_or_else(|| "auto".to_string());
                    let key = args
                        .iter()
                        .position(|a| a == "--key")
                        .and_then(|i| args.get(i + 1).cloned())
                        .or_else(|| std::env::var("FX_API_KEY").ok())
                        .or_else(|| std::env::var("AI_API_KEY").ok());
                    let base_url = args
                        .iter()
                        .position(|a| a == "--base-url")
                        .and_then(|i| args.get(i + 1).cloned());
                    if provider == "auto" {
                        let cfg = config::resolve(&cwd())?;
                        provider = if cfg.model.starts_with("anthropic/")
                            || cfg.model.starts_with("claude-")
                        {
                            "anthropic".to_string()
                        } else {
                            "gateway".to_string()
                        };
                    }
                    let canonical = crate::auth::canonical_provider(&provider)?;
                    let key = match key {
                        Some(k) => k,
                        None => {
                            eprintln!(
                                "fxrs auth add {canonical}: no API key provided\n  pass --key <key>, set FX_API_KEY/AI_API_KEY, or set the provider env var"
                            );
                            return Ok(1);
                        }
                    };
                    if let Some(base) = &base_url {
                        crate::auth::set_key(canonical, &key, Some(base))?;
                    } else {
                        crate::auth::set_key(canonical, &key, None)?;
                    }
                    println!(
                        "stored credential for `{canonical}` in {}",
                        crate::auth::auth_path().display()
                    );
                    println!(
                        "env fallback still wins; remove the env var or run `fxrs auth remove {canonical}` to disable"
                    );
                    Ok(0)
                }
                (false, Some("remove") | Some("rm") | Some("delete")) => {
                    let provider = args
                        .iter()
                        .skip(1)
                        .find(|a| !a.starts_with('-'))
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("fxrs auth remove <provider>"))?;
                    let canonical = crate::auth::canonical_provider(&provider)?;
                    let removed = crate::auth::remove_key(canonical)?;
                    if removed {
                        println!("removed credential for `{canonical}`");
                    } else {
                        println!("no stored credential for `{canonical}`");
                    }
                    Ok(0)
                }
                (false, Some("list") | Some("status") | Some("ls")) | (false, None) => {
                    let store = crate::auth::load()?;
                    if store.providers.is_empty() {
                        println!(
                            "no stored credentials ({} exists: {})",
                            {
                                let path = crate::auth::auth_path();
                                if path.exists() { "yes" } else { "no" }.to_string()
                            },
                            crate::auth::auth_path().display()
                        );
                    } else {
                        for (name, cred) in &store.providers {
                            let masked = cred
                                .api_key
                                .as_deref()
                                .map(|k| {
                                    if k.len() <= 4 {
                                        "****".to_string()
                                    } else {
                                        format!("{}…{}", &k[..2], &k[k.len() - 2..])
                                    }
                                })
                                .unwrap_or_else(|| "—".to_string());
                            let base = cred
                                .base_url
                                .as_deref()
                                .map(|u| format!(" ({u})"))
                                .unwrap_or_default();
                            let created = cred
                                .created_at
                                .as_deref()
                                .map(|c| format!(" since {c}"))
                                .unwrap_or_default();
                            println!("{name}: {masked}{base}{created}");
                        }
                    }
                    Ok(0)
                }
                _ => {
                    eprintln!("fxrs auth: usage: add <provider> [--key KEY] [--base-url URL] | remove <provider> | list | status");
                    Ok(1)
                }
            }
        }
        Some("background") | Some("bg") => {
            let args: Vec<String> = args.cloned().collect();
            let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
            let mut store = crate::background::BackgroundStore::open()?;
            match sub {
                "list" | "ls" => {
                    let records = store.list().to_vec();
                    if records.is_empty() {
                        println!("no background processes");
                        println!(
                            "start one with the background_process tool inside an agent session"
                        );
                    } else {
                        println!("{}", crate::background::render_table(&records));
                    }
                }
                "supervise" | "ps" => {
                    if store.list().is_empty() {
                        println!("no background processes");
                    } else {
                        println!(
                            "{}",
                            crate::background::render_supervise(&store.supervise())
                        );
                    }
                }
                "tree" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs background tree <id>"))?;
                    let record = store
                        .get(id)
                        .ok_or_else(|| anyhow::anyhow!("unknown background process id `{id}`"))?;
                    let table = crate::background::process_table();
                    println!("{}", crate::background::render_tree(record, &table));
                }
                "get" | "log" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs background get <id>"))?;
                    let text = store.log_text(id, 64 * 1024, None)?;
                    print!("{text}");
                    if !text.ends_with('\n') {
                        println!();
                    }
                }
                "stop" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs background stop <id>"))?;
                    let r = store.stop(id, 5000)?;
                    println!("stopped {} (pid {})", r.id, r.pid);
                }
                "stop-tree" | "stop_tree" | "kill-tree" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs background stop-tree <id>"))?;
                    let r = store.stop_tree(id, 5000)?;
                    println!("stopped {} (pid {}) and descendants", r.id, r.pid);
                }
                other => {
                    bail!(
                        "unknown background subcommand `{other}` (list | supervise | tree <id> | get <id> | stop <id> | stop-tree <id>)"
                    )
                }
            }
            Ok(0)
        }
        Some("terminal") | Some("term") => {
            let args: Vec<String> = args.cloned().collect();
            let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
            let mut store = crate::terminal::TerminalStore::open()?;
            match sub {
                "list" | "ls" => {
                    let records = store.list().to_vec();
                    if records.is_empty() {
                        println!("no terminal sessions");
                        println!("create one with the terminal tool inside an agent session");
                    } else {
                        println!("{}", crate::terminal::render_table(&records));
                    }
                }
                "get" | "read" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs terminal get <id>"))?;
                    let text = store.read(id, 200, 64 * 1024, false, false)?;
                    print!("{text}");
                    if !text.ends_with('\n') {
                        println!();
                    }
                }
                "send" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs terminal send <id> <text>"))?;
                    let input = args
                        .get(2)
                        .ok_or_else(|| anyhow::anyhow!("fxrs terminal send <id> <text>"))?;
                    store.send(id, input, true)?;
                    println!("sent {} chars to {}", input.chars().count(), id);
                }
                "stop" => {
                    let id = args
                        .get(1)
                        .ok_or_else(|| anyhow::anyhow!("fxrs terminal stop <id>"))?;
                    let r = store.stop(id)?;
                    println!("stopped {} (pid {})", r.id, r.pid);
                }
                other => bail!(
                    "unknown terminal subcommand `{other}` (list | get <id> | send <id> <text> | stop <id>)"
                ),
            }
            Ok(0)
        }
        Some("setup") => {
            println!("fxrs needs a model endpoint. Configure one of:");
            println!("  AI_GATEWAY_API_KEY=...            (Vercel AI Gateway, default)");
            println!("  FX_GATEWAY_BASE_URL=...           (gateway base, default https://gateway.vercel.ai)");
            println!("  ANTHROPIC_API_KEY=...             (native Anthropic)");
            println!("  AI_BASE_URL=... AI_API_KEY=...    (OpenAI-compatible local server)");
            println!("  FX_MODEL=...                      (model id, default openai/gpt-5.4)");
            println!("  FX_PERMISSION_MODE=ask|auto|yolo  (default auto)");
            println!("\nExample for a local server:");
            println!("  export AI_BASE_URL=http://localhost:11434/v1 AI_API_KEY=ollama FX_MODEL=llama3.1");
            Ok(0)
        }
        Some("models") => {
            let args: Vec<String> = args.cloned().collect();
            let cfg = config::resolve(&cwd())?;
            let wants_json = args.iter().any(|a| a == "--json");
            let wants_offline = args.iter().any(|a| a == "--offline");
            let limit = args
                .iter()
                .position(|a| a == "--limit")
                .and_then(|i| args.get(i + 1))
                .and_then(|v| v.parse::<usize>().ok());

            // Fetch the gateway model catalog (public endpoint; falls back
            // anonymously on 401/403). Cache loads for capability-derived
            // context limits; --offline skips the network.
            let catalog: Option<
                std::result::Result<
                    Vec<crate::gateway::ModelCatalogEntry>,
                    crate::gateway::Failure,
                >,
            > = if wants_offline {
                None
            } else {
                let store = crate::auth::load().unwrap_or_default();
                let (key, team_url) = crate::auth::resolve_key("gateway", &store);
                match crate::gateway::fetch_catalog(key.as_deref(), team_url.as_deref()) {
                    crate::gateway::CatalogResult::Loaded {
                        entries,
                        anonymous_fallback_used,
                        ..
                    } => {
                        if !anonymous_fallback_used {
                            crate::gateway::refresh_cache();
                        }
                        Some(Ok(entries))
                    }
                    crate::gateway::CatalogResult::Failed { failure, .. } => Some(Err(failure)),
                }
            };

            if wants_json {
                let discovery = crate::mcp::discover(&cfg.mcp_servers);
                let servers: Vec<_> = discovery
                    .states
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "transport": s.transport,
                            "state": s.availability.as_str(),
                            "tools": s.tool_count,
                            "error": s.error,
                        })
                    })
                    .collect();
                let catalog_json = match &catalog {
                    Some(Ok(entries)) => Some(
                        entries
                            .iter()
                            .map(|e| {
                                serde_json::json!({
                                    "id": e.id,
                                    "context_window": e.context_window,
                                    "max_tokens": e.max_tokens,
                                    "tool_use": e.has_tool_use,
                                    "vision": e.has_vision,
                                    "reasoning": e.has_reasoning,
                                    "file_input": e.has_file_input,
                                    "web_search": e.has_web_search,
                                    "fast_mode": e.supports_fast_mode,
                                })
                            })
                            .collect::<Vec<_>>(),
                    ),
                    Some(Err(f)) => Some(vec![serde_json::json!({ "error": f.describe() })]),
                    None => None,
                };
                println!(
                    "{}",
                    serde_json::json!({
                        "model": cfg.model,
                        "provider": providers_summary(&cfg),
                        "mcp_servers": servers,
                        "gateway_catalog": catalog_json,
                    })
                );
                return Ok(0);
            }

            println!("resolved model: {}", cfg.model);
            println!("provider: {}", providers_summary(&cfg));
            let keys = crate::auth::load()
                .map(|s| crate::auth::resolve_key("gateway", &s))
                .unwrap_or_default();
            let cred_note = if keys.0.is_some() {
                ""
            } else {
                " (public catalog)"
            };
            let discovery = crate::mcp::discover(&cfg.mcp_servers);
            if !discovery.states.is_empty() {
                println!();
                println!("MCP servers:");
                print!(
                    "{}",
                    crate::model_catalog::render_models_table(&discovery.states)
                );
            }
            match &catalog {
                Some(Ok(entries)) => {
                    let wanted = limit.unwrap_or(40);
                    let show: Vec<_> = entries.iter().take(wanted).collect();
                    println!();
                    println!("Gateway model catalog{cred_note}:");
                    let caps_header = "CAPABILITIES";
                    println!("{:<34} {:<7} {:<9} {caps_header}", "ID", "CTX", "OUT");
                    for e in &show {
                        let ctx = e
                            .context_window
                            .map(|c| format!("{}k", c / 1000))
                            .unwrap_or_else(|| "—".into());
                        let out = e
                            .max_tokens
                            .map(|c| format!("{}k", c / 1000))
                            .unwrap_or_else(|| "—".into());
                        let marker = if e.id == cfg.model { " *" } else { "" };
                        println!(
                            "{:<34} {:<7} {:<9} {}{}",
                            e.id,
                            ctx,
                            out,
                            e.capability_flags(),
                            marker
                        );
                    }
                    if entries.len() > wanted {
                        println!("… ({} more; use --limit N)", entries.len() - wanted);
                    }
                }
                Some(Err(f)) => {
                    println!();
                    println!("gateway model catalog unavailable: {}", f.describe());
                }
                None => {}
            }
            println!("\nSet FX_MODEL to choose. Examples:");
            println!("  openai/gpt-5.4        (AI Gateway default)");
            println!("  claude-sonnet-4-6      (Anthropic API)");
            println!("  ollama/llama3.1        (OpenAI-compatible base URL)");
            Ok(0)
        }
        Some("doctor") => {
            let cfg = config::resolve(&cwd())?;
            let issues = doctor_checks(&cfg);
            if issues.is_empty() {
                println!("fxrs doctor: all checks passed");
            } else {
                let mut hard = 0;
                for (sev, msg) in &issues {
                    let tag = if *sev == 'f' { "FAIL" } else { "WARN" };
                    println!("[{tag}] {msg}");
                    if *sev == 'f' {
                        hard += 1;
                    }
                }
                if hard > 0 {
                    bail!("fxrs doctor: {hard} failing check(s)");
                }
            }
            Ok(0)
        }
        Some("subagent") => {
            let sub_args: Vec<String> = args.clone().cloned().collect();
            return cmd_subagent(&sub_args).await;
        }
        Some("gh") => {
            let rest: Vec<String> = args.map(|s| s.to_string()).collect();
            run_gh(&rest).await
        }
        Some("modes") => {
            let cfg = config::resolve(&cwd())?;
            let registry = crate::modes::builtin_registry();
            if args.clone().any(|a| a == "--json") {
                let modes: Vec<serde_json::Value> = registry
                    .modes
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "name": m.name,
                            "description": m.description,
                            "permission_mode": format!("{}", m.permission_mode),
                            "tool_policy": format!("{:?}", m.tool_policy).to_ascii_lowercase(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "default_mode": registry.default_mode_id,
                        "modes": modes,
                    }))?
                );
                return Ok(0);
            }
            println!("modes (default: {})", registry.default_mode_id);
            for m in &registry.modes {
                println!(
                    "  {} — {} (permission: {}, tools: {})",
                    m.id,
                    m.name,
                    m.permission_mode,
                    match m.tool_policy {
                        crate::modes::ToolPolicy::Full => "full",
                        crate::modes::ToolPolicy::ReadOnly => "read-only",
                    }
                );
            }
            println!(
                "active: {} (permission {})",
                cfg.mode,
                cfg.effective_permission_mode()
            );
            Ok(0)
        }
        Some("usage") => {
            let mut period = "7d";
            let wants_json = args.clone().any(|a| a == "--json");
            if let Some(p) = args.clone().find(|a| !a.starts_with('-')) {
                period = p;
            }
            let since = crate::usage::parse_period(period);
            let totals = crate::usage::UsageStore::new().aggregate(since);
            if wants_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "period": period,
                        "turns": totals.turns,
                        "sessions": totals.sessions.len(),
                        "input_tokens": totals.input_tokens,
                        "output_tokens": totals.output_tokens,
                        "total_tokens": totals.total_tokens,
                        "tool_calls": totals.tool_calls,
                        "steps": totals.steps,
                        "cost_usd": totals.cost_usd,
                    }))?
                );
                return Ok(0);
            }
            println!("fxrs usage (last {period}):");
            println!("  turns: {}", totals.turns);
            println!("  sessions: {}", totals.sessions.len());
            println!(
                "  tokens in:  {}	({:.1}M)",
                totals.input_tokens,
                totals.input_tokens as f64 / 1e6
            );
            println!(
                "  tokens out: {}	({:.1}M)",
                totals.output_tokens,
                totals.output_tokens as f64 / 1e6
            );
            println!(
                "  tokens total: {}	({:.1}M)",
                totals.total_tokens,
                totals.total_tokens as f64 / 1e6
            );
            println!("  tool calls: {}", totals.tool_calls);
            println!("  steps: {}", totals.steps);
            println!("  est. cost: ${:.4}", totals.cost_usd);
            let recovery = crate::usage_recovery::collect_from_home_conservative();
            if !recovery.pending.is_empty()
                || !recovery.incidents.is_empty()
                || recovery.unknown_pending
            {
                println!(
                    "  recovery: {} unresolved session(s), {} incident(s){}",
                    recovery.pending.len(),
                    recovery.incidents.len(),
                    if recovery.unknown_pending {
                        " (some state unknown)"
                    } else {
                        ""
                    }
                );
            }
            Ok(0)
        }
        Some("replay") => {
            let tape_mode = args.clone().any(|a| a == "tape");
            let id = args
                .clone()
                .find(|a| a.as_str() != "tape")
                .map(|s| s.to_string())
                .unwrap_or_default();
            if id.is_empty() {
                bail!("usage: fxrs replay <session-id> | replay tape <session-id>");
            }
            let cfg = config::resolve(&cwd())?;
            let store = SessionStore::new()?;
            if tape_mode {
                let tstore = crate::tape::TapeStore::for_session(&cfg.workspace, &id);
                let entries = tstore.read(&id);
                if entries.is_empty() {
                    println!("no tape for session `{id}` (no tool calls recorded?)");
                }
                for e in &entries {
                    let mark = if e.ok { "ok" } else { "ERR" };
                    println!("{}\t{}\t{mark}\t{}", e.ts_ms, e.tool, e.target);
                    if !e.preview.is_empty() {
                        println!("    {}", shade_line(&e.preview));
                    }
                }
                println!("({} tape entries)", entries.len());
                return Ok(0);
            }
            let sess = store.load_or_error(&cfg.workspace, &id)?;
            for m in &sess.messages {
                match m.role_str() {
                    "user" => println!(
                        "\x1b[1;32muser\x1b[0m: {}",
                        m.last_text().unwrap_or_default()
                    ),
                    "assistant" => println!(
                        "\x1b[1;36massistant\x1b[0m: {}",
                        m.last_text().unwrap_or_default()
                    ),
                    "tool" => println!(
                        "\x1b[90mtool\x1b[0m: {}",
                        shade_line(&m.last_text().unwrap_or_default())
                    ),
                    _ => {}
                }
            }
            Ok(0)
        }
        Some("history") => {
            let store = crate::history::HistoryStore::new();
            let wants_json = args.clone().any(|a| a == "--json");
            let search = args
                .clone()
                .find(|a| a.as_str() == "--search")
                .and_then(|_| args.clone().skip_while(|a| a.as_str() != "--search").nth(1))
                .cloned();
            let mut limit = 20usize;
            if let Some(l) = args
                .clone()
                .find(|a| a.as_str() == "--limit")
                .and_then(|_| args.clone().skip_while(|a| a.as_str() != "--limit").nth(1))
            {
                limit = l.parse().unwrap_or(20);
            }
            let recs = store.query(search.as_deref(), limit);
            if wants_json {
                println!("{}", serde_json::to_string_pretty(&recs)?);
                return Ok(0);
            }
            if recs.is_empty() {
                println!("no history");
            }
            for r in &recs {
                println!(
                    "{} {}: {}",
                    r.timestamp_ms,
                    workspace_basename(&r.workspace_root),
                    shade_line(&r.text)
                );
            }
            Ok(0)
        }
        Some("settings") => {
            let cfg = config::resolve(&cwd())?;
            print!("{}", crate::settings_catalog::render(&cfg));
            Ok(0)
        }
        Some("help") | Some("-h") | Some("--help") => {
            show_cli_help();
            Ok(0)
        }
        Some("version") | Some("-v") | Some("--version") => {
            println!("fxrs {}", crate::version::VERSION);
            Ok(0)
        }

        Some(other) => {
            eprintln!("fxrs: unknown command `{other}`");
            show_cli_help();
            bail!("unknown command")
        }
    }
}

async fn cmd_subagent(args: &[String]) -> anyhow::Result<i32> {
    use crate::subagent_control::{apply_command, inspect_record, now_ms, SubagentStore};
    use crate::subagent_domain::{
        Command, CommandInput, ConfigureInput, InspectSection, LifecycleAction, LifecycleInput,
        MessageInput, MessageMilestoneInput, MessageSendInput, RelationshipAction,
        RelationshipInput,
    };

    let store = SubagentStore::new()?;
    let sub = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str());
    let sub = sub.unwrap_or("list");
    match sub {
        "create" => {
            let name = args
                .iter()
                .position(|a| a == "create")
                .and_then(|i| args.get(i + 1));
            let prompt = args
                .iter()
                .position(|a| a == "create")
                .and_then(|i| args.get(i + 2));
            let Some(name) = name.filter(|s| !s.starts_with('-')) else {
                bail!(
                    "usage: fxrs subagent create <name> <prompt> [--permission-mode ask|auto|yolo]"
                );
            };
            let Some(prompt) = prompt.filter(|s| !s.starts_with('-')) else {
                bail!(
                    "usage: fxrs subagent create <name> <prompt> [--permission-mode ask|auto|yolo]"
                );
            };
            let permission_mode = args
                .iter()
                .position(|a| a == "--permission-mode")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string());
            let model = args
                .iter()
                .position(|a| a == "--model")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string());
            let input = CommandInput {
                create: Some(crate::subagent_domain::CreateInput {
                    name: Some(name.to_string()),
                    mode: Some(crate::subagent_domain::Mode::OneOff),
                    prompt: Some(prompt.to_string()),
                    model,
                    permission_mode,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let cmd = crate::subagent_domain::validate_command(&input)
                .map_err(|e| anyhow::anyhow!("invalid create command: {e}"))?;
            let child_id = format!("sub-{}", now_ms());
            let mut record = store.create(
                &child_id,
                match &cmd {
                    Command::Create(c) => c,
                    _ => unreachable!(),
                },
            )?;
            let admission = crate::subagent_authority::AdmissionInput {
                parent_id: "cli".into(),
                source_id: crate::operation_id::operation_id("cli-subagent"),
                model: record.configuration.model.clone().unwrap_or_default(),
                permission_mode: crate::permissions::PermissionMode::parse(
                    &record.configuration.permission_mode,
                )
                .unwrap_or(crate::permissions::PermissionMode::Yolo),
                tool_names: Vec::new(),
                ..Default::default()
            };
            record.admission = crate::subagent_authority::capture_admission(&admission).ok();
            store.save(&record)?;
            println!(
                "created subagent `{}` (name: {}, state: {:?}, parent: {:?})",
                record.child_id, record.configuration.name, record.state, record.parent_id
            );
            Ok(0)
        }
        "run" => {
            let id = args
                .iter()
                .position(|a| a == "run")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent run <id> [--json]");
            };
            let wanted_json = args.iter().any(|a| a == "--json");
            let cfg = config::resolve(&cwd())?;
            let cfg = Arc::new(cfg);
            let store = SessionStore::new()?;
            let result = crate::subagent_executor::run_work_item(cfg, store, id).await?;
            if wanted_json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", serde_json::to_string(&result)?);
            }
            Ok(0)
        }
        "deliveries" => {
            let id = args
                .iter()
                .position(|a| a == "deliveries")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent deliveries <id> [--after N] [--limit N] [--json]");
            };
            let comm = crate::subagent_communication::CommunicationStore::new()?;
            let ledger = comm.load(id)?;
            let after = args
                .iter()
                .position(|a| a == "--after")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let limit = args
                .iter()
                .position(|a| a == "--limit")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(crate::subagent_domain::DEFAULT_PAGE_LIMIT);
            let page = crate::subagent_communication::read_page(&ledger, after, limit);
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                if page.is_empty() {
                    println!("no deliveries");
                }
                for d in page {
                    println!(
                        "d-{}  {} -> {}  {}",
                        d.sequence,
                        d.source_id,
                        d.target_id,
                        describe_payload(&d.payload)
                    );
                }
                println!(
                    "cursor: {}",
                    crate::subagent_communication::cursor_for(&ledger, "parent-model")
                );
            }
            Ok(0)
        }
        "approvals" => {
            let id = args
                .iter()
                .position(|a| a == "approvals")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent approvals <child-id> [--pending] [--limit N] [--json]");
            };
            let limit = args
                .iter()
                .position(|a| a == "--limit")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(crate::subagent_domain::DEFAULT_PAGE_LIMIT);
            let pending_only = args.iter().any(|a| a == "--pending");
            let comm = crate::subagent_communication::CommunicationStore::new()?;
            let ledger = comm.load(id)?;
            let approvals: Vec<_> = ledger
                .approvals
                .iter()
                .filter(|a| {
                    !pending_only || a.status == crate::subagent_approval::ApprovalStatus::Pending
                })
                .take(limit)
                .collect();
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&approvals)?);
            } else {
                if approvals.is_empty() {
                    println!("no approvals");
                }
                for a in approvals {
                    println!(
                        "{}  {}  {}://{}  status={:?}  {}",
                        a.id,
                        match a.kind {
                            crate::subagent_approval::ApprovalKind::Tool => "tool",
                            crate::subagent_approval::ApprovalKind::Relationship => "relationship",
                        },
                        a.child_id,
                        a.root_id,
                        a.status,
                        shade_n(&a.label, 60)
                    );
                }
            }
            Ok(0)
        }
        "approval" => {
            let action = args
                .iter()
                .position(|a| a == "approval")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            let Some(action) = action.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent approval resolve <child-id> <request-id> once|always|deny [--feedback TEXT]");
            };
            match action {
                "register" => {
                    // fxrs subagent approval register <child-id> <request-id> <label> [--tool|--relationship] [--root ROOT] [--work WORK] [--command CMD] [--explanation TEXT]
                    let id = args
                        .iter()
                        .position(|a| a == "register")
                        .and_then(|i| args.get(i + 1));
                    let req = args
                        .iter()
                        .position(|a| a == "register")
                        .and_then(|i| args.get(i + 2));
                    let label = args
                        .iter()
                        .position(|a| a == "register")
                        .and_then(|i| args.get(i + 3));
                    let (Some(id), Some(req), Some(label)) = (
                        id.filter(|s| !s.starts_with('-')),
                        req.filter(|s| !s.starts_with('-')),
                        label.filter(|s| !s.starts_with('-')),
                    ) else {
                        bail!("usage: fxrs subagent approval register <child-id> <request-id> <label> [--tool|--relationship] [--root ROOT] [--work WORK] [--command CMD] [--explanation TEXT]");
                    };
                    let kind = if args.iter().any(|a| a == "--relationship") {
                        crate::subagent_approval::ApprovalKind::Relationship
                    } else {
                        crate::subagent_approval::ApprovalKind::Tool
                    };
                    let root = args
                        .iter()
                        .position(|a| a == "--root")
                        .and_then(|i| args.get(i + 1))
                        .cloned()
                        .unwrap_or_else(|| "parent".to_string());
                    let work = args
                        .iter()
                        .position(|a| a == "--work")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.to_string());
                    let command = args
                        .iter()
                        .position(|a| a == "--command")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.to_string());
                    let explanation = args
                        .iter()
                        .position(|a| a == "--explanation")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.to_string());
                    let input = crate::subagent_approval::ApprovalInput {
                        id: req.to_string(),
                        kind,
                        child_id: id.to_string(),
                        root_id: root,
                        work_id: work,
                        relationship: None,
                        prepared_fingerprint: [3u8; 32],
                        label: label.to_string(),
                        explanation,
                        command,
                        grants: vec![(label.to_string(), "*".into())],
                        created_at_ms: now_ms(),
                    };
                    let comm = crate::subagent_communication::CommunicationStore::new()?;
                    let mut ledger = comm.load(id)?;
                    let result = crate::subagent_approval::register_approval(&mut ledger, input)?;
                    comm.save(&ledger)?;
                    println!("approval registered: {result:?}");
                    Ok(0)
                }
                "resolve" => {
                    let id = args
                        .iter()
                        .position(|a| a == "resolve")
                        .and_then(|i| args.get(i + 1));
                    let req = args
                        .iter()
                        .position(|a| a == "resolve")
                        .and_then(|i| args.get(i + 2));
                    let decision = args
                        .iter()
                        .position(|a| a == "resolve")
                        .and_then(|i| args.get(i + 3));
                    let (Some(id), Some(req), Some(decision)) =
                        (id.filter(|s| !s.starts_with('-')), req.filter(|s| !s.starts_with('-')), decision.filter(|s| !s.starts_with('-')))
                    else {
                        bail!("usage: fxrs subagent approval resolve <child-id> <request-id> once|always|deny [--feedback TEXT]");
                    };
                    let decision = match decision.as_str() {
                        "once" => crate::subagent_approval::ApprovalDecision::Once,
                        "always" => crate::subagent_approval::ApprovalDecision::Always,
                        "deny" => crate::subagent_approval::ApprovalDecision::Deny,
                        _ => bail!("decision must be once|always|deny"),
                    };
                    let feedback = args
                        .iter()
                        .position(|a| a == "--feedback")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.to_string());
                    let comm = crate::subagent_communication::CommunicationStore::new()?;
                    let mut ledger = comm.load(id)?;
                    let ctrl = SubagentStore::new()?;
                    let mut record = ctrl
                        .load(id)?
                        .ok_or_else(|| anyhow::anyhow!("no subagent `{id}`"))?;
                    let context = crate::subagent_approval::ApprovalContext {
                        attached: record.parent_id.is_some(),
                        child_cancelled: record.state == crate::subagent_domain::State::Cancelled,
                        child_closed: record.state == crate::subagent_domain::State::Archived,
                    };
                    let response = crate::subagent_approval::ApprovalResponse {
                        request_id: req.to_string(),
                        child_id: id.to_string(),
                        decision,
                        feedback,
                        timestamp_ms: now_ms(),
                    };
                    let revision = ledger.generation;
                    let outcome = crate::subagent_approval::resolve_approval(
                        &mut ledger,
                        &response,
                        &context,
                        revision,
                    )?;
                    comm.save(&ledger)?;
                    // Approval resolutions that grant one-shot / always keep
                    // the work running; denied resolves cancel the work item.
                    if outcome == crate::subagent_approval::ApprovalDecisionOutcome::Deny {
                        if let Some(work_id) = ledger
                            .approvals
                            .iter()
                            .find(|a| a.id == *req)
                            .and_then(|a| a.work_id.clone())
                        {
                            if let Some(w) =
                                record.queue.iter_mut().find(|w| w.id == work_id)
                            {
                                w.status = crate::subagent_domain::QueueStatus::Cancelled;
                            }
                            record.updated_at_ms = now_ms();
                            let _ = ctrl.save(&record);
                        }
                    }
                    println!("approval resolved: {outcome:?}");
                    Ok(0)
                }
                _ => bail!(
                    "usage: fxrs subagent approval resolve <child-id> <request-id> once|always|deny [--feedback TEXT]"
                ),
            }
        }
        "parent-turn" => {
            let id = args
                .iter()
                .position(|a| a == "parent-turn")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent parent-turn <parent-id> [--limit N] [--json]");
            };
            let limit = args
                .iter()
                .position(|a| a == "--limit")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(crate::subagent_domain::DEFAULT_PAGE_LIMIT);
            let store = crate::subagent_control::SubagentStore::new()?;
            let records = crate::subagent_relationship::load_records(&store);
            let children = crate::subagent_relationship::children_of(&records, id);
            let comm = crate::subagent_communication::CommunicationStore::new()?;
            let deliveries = crate::subagent_communication::project_parent_deliveries(
                &comm, &children, id, limit,
            );
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&deliveries)?);
            } else {
                if deliveries.is_empty() {
                    println!("no deliveries for parent `{id}`");
                }
                for d in deliveries {
                    println!(
                        "d-{}  {} -> {}  {}",
                        d.sequence,
                        d.source_id,
                        d.target_id,
                        describe_payload(&d.payload)
                    );
                }
            }
            Ok(0)
        }
        "inspect" => {
            let id = args
                .iter()
                .position(|a| a == "inspect")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent inspect <id> [--json]");
            };
            let wanted_json = args.iter().any(|a| a == "--json");
            let Some(record) = store.load(id)? else {
                bail!("no subagent `{id}`");
            };
            let sections = vec![
                InspectSection::Status,
                InspectSection::Messages,
                InspectSection::Events,
                InspectSection::Configuration,
                InspectSection::Relationship,
            ];
            let inspection = inspect_record(&record, &sections);
            if wanted_json {
                println!("{}", serde_json::to_string_pretty(&inspection)?);
                return Ok(0);
            }
            println!(
                "subagent {} (state: {:?}, mode: {:?})",
                inspection.child_id, inspection.state, inspection.mode
            );
            println!(
                "  parent: {}",
                inspection.parent_id.as_deref().unwrap_or("-")
            );
            println!("  generation: {}", inspection.generation);
            println!("  name: {}", inspection.configuration.name);
            println!(
                "  model: {}",
                inspection
                    .configuration
                    .model
                    .as_deref()
                    .unwrap_or("(default)")
            );
            println!("  queued work: {}", inspection.queue.len());
            for w in &inspection.queue {
                println!(
                    "    - [{}] {} (from {})",
                    debug_qs(&w.status),
                    w.content,
                    w.source_id
                );
            }
            println!(
                "  events: {} (after eviction {})",
                inspection.events.len(),
                record.events_evicted_through
            );
            for e in inspection.events.iter().rev().take(8) {
                println!(
                    "    - seq {} rev {}: {}",
                    e.sequence,
                    e.revision,
                    debug_event(&e.kind)
                );
            }
            Ok(0)
        }
        "message" => {
            let kind = args
                .iter()
                .position(|a| a == "message")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            let id = args
                .iter()
                .position(|a| a == "message")
                .and_then(|i| args.get(i + 2));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent message send|milestone <id> <content|name>");
            };
            let Some(payload) = args
                .iter()
                .position(|a| a == "message")
                .and_then(|i| args.get(i + 3))
            else {
                bail!("usage: fxrs subagent message send|milestone <id> <content|name>");
            };
            let mut record = store
                .load(id)?
                .ok_or_else(|| anyhow::anyhow!("no subagent `{id}`"))?;
            let input = match kind {
                Some("send") => MessageInput {
                    send: Some(MessageSendInput {
                        id: id.to_string(),
                        content: payload.to_string(),
                    }),
                    ..Default::default()
                },
                Some("milestone") => MessageInput {
                    milestone: Some(MessageMilestoneInput {
                        name: payload.to_string(),
                    }),
                    ..Default::default()
                },
                _ => bail!("usage: fxrs subagent message send|milestone <id> <content|name>"),
            };
            let cmd = crate::subagent_domain::validate_command(&CommandInput {
                message: Some(input),
                ..Default::default()
            })
            .map_err(|e| anyhow::anyhow!("invalid message command: {e}"))?;
            let outcome = apply_command(&mut record, &cmd, now_ms())?;
            store.save(&record)?;
            // Deliveries: message sends append a Message delivery; milestones
            // append a Milestone delivery (upstream communication_manager).
            let comm = crate::subagent_communication::CommunicationStore::new()?;
            let mut ledger = comm.load(&record.child_id)?;
            match kind {
                Some("send") => {
                    crate::subagent_communication::deliver(
                        &mut ledger,
                        "parent",
                        &record.child_id,
                        crate::subagent_control::DeliveryPayload::Message(payload.to_string()),
                        now_ms(),
                    );
                }
                Some("milestone") => {
                    crate::subagent_communication::deliver(
                        &mut ledger,
                        &record.child_id,
                        record
                            .parent_id
                            .clone()
                            .unwrap_or_else(|| "parent".into())
                            .as_str(),
                        crate::subagent_control::DeliveryPayload::Milestone(payload.to_string()),
                        now_ms(),
                    );
                }
                _ => {}
            }
            comm.save(&ledger)?;
            println!("message applied: {outcome:?}");
            Ok(0)
        }
        "configure" => {
            let id = args
                .iter()
                .position(|a| a == "configure")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent configure <id> [--name N] [--model M] [--permission-mode P] [--effort E]");
            };
            let mut record = store
                .load(id)?
                .ok_or_else(|| anyhow::anyhow!("no subagent `{id}`"))?;
            let name = args
                .iter()
                .position(|a| a == "--name")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string());
            let model = args
                .iter()
                .position(|a| a == "--model")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string());
            let permission_mode = args
                .iter()
                .position(|a| a == "--permission-mode")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string());
            let effort = args
                .iter()
                .position(|a| a == "--effort")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string());
            let input = CommandInput {
                configure: Some(ConfigureInput {
                    id: id.to_string(),
                    name,
                    model,
                    effort,
                    permission_mode,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let cmd = crate::subagent_domain::validate_command(&input)
                .map_err(|e| anyhow::anyhow!("invalid configure command: {e}"))?;
            let outcome = apply_command(&mut record, &cmd, now_ms())?;
            store.save(&record)?;
            println!("configured: {outcome:?}");
            Ok(0)
        }
        "relationship" => {
            let action = args
                .iter()
                .position(|a| a == "relationship")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            let id = args
                .iter()
                .position(|a| a == "relationship")
                .and_then(|i| args.get(i + 2));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent relationship attach|detach|reparent <id> [parent-id]");
            };
            let parent = args
                .iter()
                .position(|a| a == "relationship")
                .and_then(|i| args.get(i + 3));
            let mut record = store
                .load(id)?
                .ok_or_else(|| anyhow::anyhow!("no subagent `{id}`"))?;
            let action = match action {
                Some("attach") => RelationshipAction::Attach,
                Some("detach") => RelationshipAction::Detach,
                Some("reparent") => RelationshipAction::Reparent,
                _ => bail!(
                    "usage: fxrs subagent relationship attach|detach|reparent <id> [parent-id]"
                ),
            };
            let input = CommandInput {
                relationship: Some(RelationshipInput {
                    action,
                    id: id.to_string(),
                    parent_id: parent.map(|s| s.to_string()),
                }),
                ..Default::default()
            };
            let cmd = crate::subagent_domain::validate_command(&input)
                .map_err(|e| anyhow::anyhow!("invalid relationship command: {e}"))?;
            let outcome = apply_command(&mut record, &cmd, now_ms())?;
            store.save(&record)?;
            println!("relationship: {outcome:?}");
            Ok(0)
        }
        "lifecycle" => {
            let action = args
                .iter()
                .position(|a| a == "lifecycle")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            let id = args
                .iter()
                .position(|a| a == "lifecycle")
                .and_then(|i| args.get(i + 2));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent lifecycle cancel|resume|close|reopen <id>");
            };
            let action = match action {
                Some("cancel") => LifecycleAction::Cancel,
                Some("resume") => LifecycleAction::Resume,
                Some("close") => LifecycleAction::Close,
                Some("reopen") => LifecycleAction::Reopen,
                _ => bail!("usage: fxrs subagent lifecycle cancel|resume|close|reopen <id>"),
            };
            let mut record = store
                .load(id)?
                .ok_or_else(|| anyhow::anyhow!("no subagent `{id}`"))?;
            let input = CommandInput {
                lifecycle: Some(LifecycleInput {
                    id: id.to_string(),
                    action,
                }),
                ..Default::default()
            };
            let cmd = crate::subagent_domain::validate_command(&input)
                .map_err(|e| anyhow::anyhow!("invalid lifecycle command: {e}"))?;
            let outcome = apply_command(&mut record, &cmd, now_ms())?;
            store.save(&record)?;
            // Cancelling/closing a child invalidates its pending approvals
            // (upstream approval_persistence.invalidate).
            if matches!(action, LifecycleAction::Cancel | LifecycleAction::Close) {
                let comm = crate::subagent_communication::CommunicationStore::new()?;
                let mut ledger = comm.load(id)?;
                let status = if action == LifecycleAction::Cancel {
                    crate::subagent_approval::ApprovalStatus::Cancelled
                } else {
                    crate::subagent_approval::ApprovalStatus::Stale
                };
                let revision = ledger.generation;
                let changed = crate::subagent_approval::invalidate_child_approvals(
                    &mut ledger,
                    id,
                    status,
                    now_ms(),
                    revision,
                );
                comm.save(&ledger)?;
                println!("approvals invalidated: {changed}");
            }
            println!("lifecycle: {outcome:?}");
            Ok(0)
        }
        "list" => {
            let records = store.list();
            if records.is_empty() {
                println!("no subagents");
                return Ok(0);
            }
            let tree = args.iter().any(|a| a == "--tree");
            if tree {
                let roots = crate::subagent_relationship::roots(&records);
                let by_parent: std::collections::BTreeMap<String, Vec<String>> = records
                    .iter()
                    .filter_map(|r| r.parent_id.clone().map(|p| (p, r.child_id.clone())))
                    .fold(std::collections::BTreeMap::new(), |mut m, (p, c)| {
                        m.entry(p).or_insert_with(Vec::new).push(c);
                        m
                    });
                fn print_node(
                    records: &[crate::subagent_control::SubagentRecord],
                    by_parent: &std::collections::BTreeMap<String, Vec<String>>,
                    id: &str,
                    prefix: &str,
                    is_last: bool,
                ) {
                    let r = records.iter().find(|r| r.child_id == id);
                    let (state, name) = match r {
                        Some(r) => (
                            format!("{:?}", r.state).to_lowercase(),
                            r.configuration.name.clone(),
                        ),
                        None => ("?".into(), "-".into()),
                    };
                    let connector = if is_last { "└─ " } else { "├─ " };
                    println!("{prefix}{connector}{id} [{state}] {name}");
                    let children = by_parent.get(id).cloned().unwrap_or_default();
                    let child_prefix = format!("{prefix}{}", if is_last { "   " } else { "│  " });
                    for (i, child) in children.iter().enumerate() {
                        print_node(
                            records,
                            by_parent,
                            child,
                            &child_prefix,
                            i == children.len() - 1,
                        );
                    }
                }
                if roots.is_empty() {
                    // All records have parents in a broken cycle: print flat.
                    for r in records {
                        println!(
                            "{}  {:11}  parent={:11}  name={}",
                            r.child_id,
                            format!("{:?}", r.state).to_lowercase(),
                            r.parent_id.as_deref().unwrap_or("-"),
                            r.configuration.name
                        );
                    }
                } else {
                    for (i, root) in roots.iter().enumerate() {
                        print_node(&records, &by_parent, root, "", i == roots.len() - 1);
                    }
                }
                return Ok(0);
            }
            for r in records {
                println!(
                    "{}  {:11}  parent={:11}  queue={}  events={}  name={}",
                    r.child_id,
                    format!("{:?}", r.state).to_lowercase(),
                    r.parent_id.as_deref().unwrap_or("-"),
                    r.queue.len(),
                    r.events.len(),
                    r.configuration.name
                );
            }
            Ok(0)
        }
        "delete" => {
            let id = args
                .iter()
                .position(|a| a == "delete")
                .and_then(|i| args.get(i + 1));
            let Some(id) = id.filter(|s| !s.starts_with('-')) else {
                bail!("usage: fxrs subagent delete <id>");
            };
            {
                // Delete removes the control record; pending approvals are
                // invalidated so a later parent-turn can't see stale ones.
                let comm = crate::subagent_communication::CommunicationStore::new()?;
                let mut ledger = comm.load(id)?;
                let revision = ledger.generation;
                let changed = crate::subagent_approval::invalidate_child_approvals(
                    &mut ledger,
                    id,
                    crate::subagent_approval::ApprovalStatus::Stale,
                    now_ms(),
                    revision,
                );
                comm.save(&ledger)?;
                if changed > 0 {
                    println!("approvals invalidated: {changed}");
                }
            }
            store.delete(id)?;
            println!("deleted subagent `{id}`");
            Ok(0)
        }
        other => {
            bail!("unknown subagent command: {other}");
        }
    }
}

fn shade_n(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

fn describe_payload(payload: &crate::subagent_control::DeliveryPayload) -> String {
    match payload {
        crate::subagent_control::DeliveryPayload::Message(text) => {
            let s: String = text.chars().take(80).collect();
            format!("message: {s}")
        }
        crate::subagent_control::DeliveryPayload::Milestone(name) => format!("milestone: {name}"),
        crate::subagent_control::DeliveryPayload::Terminal(state) => {
            format!("terminal: {state:?}")
        }
        crate::subagent_control::DeliveryPayload::Interval {
            state,
            coalesced_ticks,
        } => {
            format!("interval: {state:?} ticks={coalesced_ticks}")
        }
        crate::subagent_control::DeliveryPayload::Approval(label) => format!("approval: {label}"),
        crate::subagent_control::DeliveryPayload::ToolActivity(activity) => {
            format!(
                "tool_activity: {} {}",
                activity.tool_name,
                format!("{:?}", activity.phase).to_lowercase()
            )
        }
    }
}

fn debug_qs(s: &crate::subagent_domain::QueueStatus) -> String {
    format!("{s:?}").to_lowercase()
}

fn debug_event(kind: &crate::subagent_domain::EventKind) -> String {
    match kind {
        crate::subagent_domain::EventKind::Created => "created".into(),
        crate::subagent_domain::EventKind::MessageQueued { .. } => "message_queued".into(),
        crate::subagent_domain::EventKind::RelationshipChanged { .. } => {
            "relationship_changed".into()
        }
        crate::subagent_domain::EventKind::Configured => "configured".into(),
        crate::subagent_domain::EventKind::LifecycleChanged { .. } => "lifecycle_changed".into(),
        crate::subagent_domain::EventKind::WorkTransition { .. } => "work_transition".into(),
        crate::subagent_domain::EventKind::MilestoneEmitted { .. } => "milestone_emitted".into(),
    }
}

fn workspace_basename(ws: &str) -> String {
    crate::sessions::workspace_name(ws)
}

fn cwd() -> PathBuf {
    std::env::current_dir()
        .context("getting current directory")
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn providers_summary(_cfg: &config::Config) -> String {
    if std::env::var("AI_GATEWAY_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "AI Gateway".into()
    } else if std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "Anthropic".into()
    } else if std::env::var("AI_BASE_URL")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        "OpenAI-compatible base URL".into()
    } else {
        "not configured (run `fxrs setup`)".into()
    }
}

/// Run a battery of diagnostics, returning `(severity, message)` pairs.
/// `'f'` = hard failure (exit 1), `'w'` = warning.
pub fn doctor_checks(cfg: &config::Config) -> Vec<(char, String)> {
    use std::path::PathBuf;
    let mut out: Vec<(char, String)> = Vec::new();

    // 1. fx home writable + settings parses.
    let home = config::fx_home();
    match std::fs::create_dir_all(&home) {
        Ok(_) => {}
        Err(e) => out.push((
            'f',
            format!("cannot create ~/.fx ({}): {e}", home.display()),
        )),
    }
    let settings = config::settings_path();
    if settings.exists() {
        if let Err(e) = config::resolve(&cfg.workspace) {
            out.push(('f', format!("settings.json failed to parse: {e:#}")));
        }
    }

    // 2. Model configured.
    let has_gateway = std::env::var("AI_GATEWAY_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_anthropic = std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_ai = std::env::var("AI_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
        && std::env::var("AI_BASE_URL")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
    if !(has_gateway || has_anthropic || has_ai) {
        out.push(('f', "no model endpoint configured (AI_GATEWAY_API_KEY / ANTHROPIC_API_KEY / AI_BASE_URL+AI_API_KEY). run `fxrs setup`".into()));
    } else {
        out.push((
            'w',
            "model endpoint configured; keys are not validated here".into(),
        ));
    }

    // 3. Workspace writable.
    match std::fs::metadata(&cfg.workspace) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => out.push((
            'f',
            format!("workspace is not a directory: {}", cfg.workspace.display()),
        )),
        Err(e) => out.push((
            'f',
            format!("workspace missing: {} ({e})", cfg.workspace.display()),
        )),
    }

    // 4. Sessions dir.
    let sessions_dir = home.join("sessions");
    if !sessions_dir.is_dir() {
        out.push(('w', format!("no sessions yet: {}", sessions_dir.display())));
    }

    // 5. MCP server configs: stdio commands resolve in PATH; remote
    //    endpoints are validated (https or loopback http).
    for srv in &cfg.mcp_servers {
        if !srv.is_enabled() {
            out.push(('w', format!("mcp server `{}`: disabled", srv.name)));
            continue;
        }
        if srv.requires_remote_url() {
            match srv.remote_url() {
                Some(url) => {
                    if let Err(e) = crate::mcp_transport::validate_endpoint(url) {
                        out.push(('f', format!("mcp server `{}`: {e}", srv.name)));
                    } else if srv.bearer_token_env.is_some() && srv.bearer_token().is_none() {
                        out.push((
                            'w',
                            format!(
                                "mcp server `{}`: bearer_token_env `{}` is unset",
                                srv.name,
                                srv.bearer_token_env.as_deref().unwrap_or("")
                            ),
                        ));
                    }
                }
                None => out.push((
                    'f',
                    format!("mcp server `{}`: remote transport needs a `url`", srv.name),
                )),
            }
            continue;
        }
        if let Some(cmd) = &srv.command {
            let path = PathBuf::from(cmd);
            let found = if path.is_absolute() {
                path.is_file()
            } else {
                which_in_path(cmd)
            };
            if !found {
                out.push((
                    'w',
                    format!(
                        "mcp server `{}`: command `{cmd}` not found in PATH",
                        srv.name
                    ),
                ));
            }
        }
    }

    // 6. Hooks discovered (informational).
    for kind in [
        crate::hooks::HookKind::PreToolUse,
        crate::hooks::HookKind::Stop,
        crate::hooks::HookKind::PostTurnEnd,
        crate::hooks::HookKind::AttentionRequired,
    ] {
        let found = crate::hooks::discover(kind, &cfg.workspace);
        if !found.is_empty() {
            out.push((
                'w',
                format!("hook {}: {} script(s)", kind.event_name(), found.len()),
            ));
        }
    }

    // 7. Git repo (informational).
    let git_dir = cfg.workspace.join(".git");
    if git_dir.exists() {
        out.push(('w', "git repository detected".into()));
    }

    // 8. Terminal integration — native PTY (default backend) + tmux (durable).
    if crate::terminal::native_pty_available() {
        out.push((
            'w',
            "native PTY detected (default terminal backend + browser_terminal enabled)".into(),
        ));
    } else {
        out.push((
            'w',
            "native PTY unavailable — terminal sessions default to tmux; browser_terminal exec disabled".into(),
        ));
    }
    if crate::terminal::tmux_available() {
        out.push((
            'w',
            "tmux detected (durable terminal sessions enabled)".into(),
        ));
    } else {
        out.push((
            'w',
            "tmux not found — durable terminal sessions require tmux (apt/brew install tmux)"
                .into(),
        ));
    }

    // 9. Background store: parse + live daemon count.
    match crate::background::BackgroundStore::open() {
        Ok(store) => {
            let running = store
                .list()
                .iter()
                .filter(|r| r.status == crate::background::BgStatus::Running)
                .count();
            if running > 0 {
                out.push((
                    'w',
                    format!(
                        "{running} background process(es) running — `fxrs background supervise`"
                    ),
                ));
            }
        }
        Err(e) => out.push(('w', format!("background store unreadable: {e:#}"))),
    }

    // 10. Terminal store: parse + live/lost session counts.
    match crate::terminal::TerminalStore::open() {
        Ok(store) => {
            let running = store
                .list()
                .iter()
                .filter(|r| r.status == crate::terminal::TermStatus::Running)
                .count();
            if running > 0 {
                out.push((
                    'w',
                    format!("{running} terminal session(s) running — `fxrs terminal`"),
                ));
            }
            let lost = store
                .list()
                .iter()
                .filter(|r| r.status == crate::terminal::TermStatus::Lost)
                .count();
            if lost > 0 {
                out.push((
                    'w',
                    format!("{lost} terminal session(s) marked lost (host gone) — `fxrs terminal`"),
                ));
            }
        }
        Err(e) => out.push(('w', format!("terminal store unreadable: {e:#}"))),
    }

    // 11. Usage recovery registry: unresolved usage markers left by crashes
    //     after a session checkpoint but before the ledger append.
    match crate::usage_recovery::list_recovery_sessions() {
        Ok(marked) => {
            if !marked.is_empty() {
                let recovery = crate::usage_recovery::collect_from_home_conservative();
                let mut msg = format!(
                    "usage recovery: {} marked session(s), {} unresolved, {} incident(s)",
                    marked.len(),
                    recovery.pending.len(),
                    recovery.incidents.len(),
                );
                if recovery.unknown_pending {
                    msg.push_str(" (some state unknown)");
                }
                out.push(('w', msg));
            }
        }
        Err(e) => out.push(('w', format!("usage recovery registry unreadable: {e:#}"))),
    }

    out
}

fn which_in_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(cmd).is_file())
}

fn shade_line(s: &str) -> String {
    if s.len() > 240 {
        format!("{}…", s.chars().take(240).collect::<String>())
    } else {
        s.to_string()
    }
}

fn show_cli_help() {
    println!(
        "fxrs — Rust port of fx (Vercel Labs) — a tiny terminal coding agent\n\n\
         usage:\n\
         \x1b[32m  fxrs\x1b[0m                    interactive shell\n\
         \x1b[32m  fxrs ask <prompt>\x1b[0m       one-shot prompt (reads prompt from argv or stdin)\n\
         \x1b[32m  fxrs resume [last|<id>]\x1b[0m resume a session (default: latest)\n\
         \x1b[32m  fxrs sessions\x1b[0m           list sessions\n\
         \x1b[32m  fxrs session <id>\x1b[0m       show a session\n\
         \x1b[32m  fxrs status\x1b[0m             show config status\n\
         \x1b[32m  fxrs permissions\x1b[0m        show permission rules\n\
         \x1b[32m  fxrs models\x1b[0m             show resolved model / provider\n\
         \x1b[32m  fxrs modes\x1b[0m              list built-in modes (ask/code)\n\
         \x1b[32m  fxrs subagent\x1b[0m          manage subagents (create/inspect/message/lifecycle)\n\
         \x1b[32m  fxrs doctor\x1b[0m            run environment diagnostics\n\
         \x1b[32m  fxrs usage [24h|7d|30d|all]\x1b[0m token usage and cost\n\
         \x1b[32m  fxrs settings\x1b[0m          show catalog + effective settings\n\
         \x1b[32m  fxrs session <id> [--json|--delete]\x1b[0m session details\n\
         \x1b[32m  fxrs replay <id>\x1b[0m         replay a session transcript\n\
         \x1b[32m  fxrs setup\x1b[0m              provider configuration guide\n\
         \x1b[32m  fxrs version\x1b[0m            version info\n\
         \x1b[32m  fxrs help\x1b[0m               this help\n\n\
         Environment: FX_MODEL, FX_PERMISSION_MODE, AI_GATEWAY_API_KEY,\n\
         FX_GATEWAY_BASE_URL, ANTHROPIC_API_KEY, AI_BASE_URL, AI_API_KEY\n\
         Config: ~/.fx/settings.json, <workspace>/.fx.json (see README)"
    );
}

async fn run_ask(rest: &[String]) -> Result<i32> {
    let args = rest.to_vec();
    let mut resume: Option<String> = None;
    let mut prompt_parts: Vec<String> = vec![];
    let mut system: Option<String> = None;
    let mut messages: Vec<String> = vec![];

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--resume" | "-r" => {
                i += 1;
                resume = args.get(i).cloned();
            }
            "--system" => {
                i += 1;
                system = args.get(i).cloned();
            }
            "--message" | "-m" => {
                i += 1;
                if let Some(m) = args.get(i) {
                    messages.push(m.clone());
                }
            }
            "--json" => {
                // accepted for fx CLI compat; output is already structured-ish
            }
            "--help" | "-h" => {
                println!("usage: fxrs ask [--resume <id>] [--system <s>] [-m <msg>] <prompt>");
                return Ok(0);
            }
            _ => prompt_parts.push(a.clone()),
        }
        i += 1;
    }

    let prompt = if prompt_parts.is_empty() {
        // Read from stdin if piped, else prompt.
        use std::io::Read;
        let mut buf = String::new();
        if !std::io::stdin().is_terminal() {
            std::io::stdin().read_to_string(&mut buf)?;
            if buf.trim().is_empty() {
                bail!("fxrs ask: empty prompt (pass text or pipe stdin)");
            }
        } else {
            bail!("fxrs ask: pass a prompt: fxrs ask \"explain this repo\"");
        }
        Some(buf)
    } else {
        Some(prompt_parts.join(" "))
    };

    let cfg = Arc::new(config::resolve(&cwd())?);
    let store = SessionStore::new()?;
    let human = QuietHuman;

    // Non-interactive mode: a one-shot agent turn. Reuse the interactive where
    // resume requested — fall back to one-shot semantics via non-interactive.
    let req = AgentRequest {
        prompt,
        system,
        interactive: false,
        resume,
        messages,
    };
    let out: AgentOutput = crate::agent::run(req, cfg.clone(), &human, &store).await?;
    if let Some(e) = &out.error {
        eprintln!("fxrs error: {e}");
        return Ok(1);
    }
    if out.finish_reason == FinishReason::MaxSteps {
        eprintln!("fxrs: stopped after {} steps (max_agent_steps)", out.steps)
    }
    println!(
        "\n[fxrs] session {} · {} steps · {} tool calls · {} tokens (${:.4})",
        out.session_id, out.steps, out.tool_calls, out.total_tokens, out.cost_usd
    );
    Ok(0)
}

#[allow(dead_code)]
fn _p(_: &Path) {}

// ------------------------------------------------------------------ gh

/// `fxrs gh <pr|issue|snapshot> ...` — upstream `core/github/*` surface.
async fn run_gh(args: &[String]) -> Result<i32> {
    let workspace = cwd();
    let sub = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str());
    match sub {
        None | Some("snapshot") | Some("status") => {
            let snap = crate::github::git_snapshot(&workspace);
            if args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "in_git_repo": snap.in_git_repo,
                        "text": snap.text,
                    }))?
                );
            } else {
                println!("{}", snap.text);
            }
            Ok(0)
        }
        Some("pr") => {
            let action = args
                .iter()
                .position(|a| a == "pr")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            match action {
                Some("draft" | "create" | "publish") => {
                    let draft_text = args
                        .iter()
                        .position(|a| a == "--draft")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.to_string())
                        .or_else(|| {
                            // Rest after `pr create` is the draft text.
                            let pos = args.iter().position(|a| a == "create")?;
                            let rest = &args[pos + 1..];
                            if rest.is_empty() {
                                None
                            } else {
                                Some(rest.join(" "))
                            }
                        })
                        .ok_or_else(|| anyhow::anyhow!("usage: fxrs gh pr create [--draft TEXT] <TEXT>"))?;
                    let draft = crate::github::parse_draft(&draft_text)?;
                    let _ = action;
                    if args.iter().any(|a| a == "--no-publish") {
                        println!("title: {}", draft.title);
                        println!("body:\n{}", draft.body);
                        return Ok(0);
                    }
                    let result = crate::github::publish(&crate::github::Workflow::PullRequest, &draft);
                    if result.ok {
                        println!("{}", result.text);
                        Ok(0)
                    } else {
                        bail!("{}{}", "gh publish failed: ", result.text)
                    }
                }
                Some("prompt") => {
                    let context = args
                        .iter()
                        .position(|a| a == "--context")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let prompt =
                        crate::github::build_prompt(&crate::github::Workflow::PullRequest, "en", context, &workspace)?;
                    println!("{prompt}");
                    Ok(0)
                }
                _ => bail!("usage: fxrs gh pr create [--draft TEXT] <TEXT> | pr prompt [--context TEXT]"),
            }
        }
        Some("issue") => {
            let action = args
                .iter()
                .position(|a| a == "issue")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.as_str());
            match action {
                Some("draft" | "create" | "publish") => {
                    let draft_text = args
                        .iter()
                        .position(|a| a == "--draft")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.to_string())
                        .or_else(|| {
                            let pos = args.iter().position(|a| a == "create")?;
                            let rest = &args[pos + 1..];
                            if rest.is_empty() {
                                None
                            } else {
                                Some(rest.join(" "))
                            }
                        })
                        .ok_or_else(|| anyhow::anyhow!("usage: fxrs gh issue create [--draft TEXT] <TEXT>"))?;
                    let draft = crate::github::parse_draft(&draft_text)?;
                    if args.iter().any(|a| a == "--no-publish") {
                        println!("title: {}", draft.title);
                        println!("body:\n{}", draft.body);
                        return Ok(0);
                    }
                    let result = crate::github::publish(&crate::github::Workflow::Issue, &draft);
                    if result.ok {
                        println!("{}", result.text);
                        Ok(0)
                    } else {
                        bail!("gh publish failed: {}", result.text)
                    }
                }
                Some("prompt") => {
                    let context = args
                        .iter()
                        .position(|a| a == "--context")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let prompt =
                        crate::github::build_prompt(&crate::github::Workflow::Issue, "en", context, &workspace)?;
                    println!("{prompt}");
                    Ok(0)
                }
                _ => bail!("usage: fxrs gh issue create [--draft TEXT] <TEXT> | issue prompt [--context TEXT]"),
            }
        }
        Some("feedback") => {
            println!("{}", crate::github::feedback_url);
            Ok(0)
        }
        _ => bail!("usage: fxrs gh snapshot | gh pr create <title>|<body> | gh issue create <title>|<body> | gh feedback"),
    }
}
