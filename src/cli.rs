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

    // 8. Terminal integration (tmux) — needed for the `terminal` tool.
    if crate::terminal::tmux_available() {
        out.push(('w', "tmux detected (terminal sessions enabled)".into()));
    } else {
        out.push((
            'w',
            "tmux not found — terminal sessions require tmux (apt/brew install tmux)".into(),
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

    // 10. Terminal store: parse + live session count.
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
        }
        Err(e) => out.push(('w', format!("terminal store unreadable: {e:#}"))),
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
