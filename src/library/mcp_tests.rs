// Ports of internal/library/library_test.go's MCP tests.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;
use crate::library::Target;
use crate::library::testing::{Sandbox, ok, read as read_file, write};

fn ids(targets: &[Target]) -> Vec<String> {
    targets.iter().map(|t| t.agent.spec.id.to_owned()).collect()
}

fn stdio_server(name: &str, agents: Vec<String>) -> Server {
    Server {
        name: name.to_owned(),
        transport: "stdio".to_owned(),
        command: "npx".to_owned(),
        args: vec!["-y".to_owned(), "@mcp/fs".to_owned(), "/tmp/a b".to_owned()],
        env: BTreeMap::from([("TOKEN".to_owned(), "t\"q".to_owned())]),
        agents,
        ..Server::default()
    }
}

fn remote_server(name: &str, agents: Vec<String>) -> Server {
    Server {
        name: name.to_owned(),
        transport: "http".to_owned(),
        url: "https://example.com/mcp".to_owned(),
        headers: BTreeMap::from([("Authorization".to_owned(), "Bearer x".to_owned())]),
        agents,
        ..Server::default()
    }
}

#[test]
fn test_targets() {
    let sb = Sandbox::new();
    let got = ids(&crate::library::targets(&sb.homes));
    for id in [
        "claude", "codex", "gemini", "opencode", "pi", "goose", "cursor", "copilot", "crush",
    ] {
        assert!(got.iter().any(|x| x == id), "{id} not a target: {got:?}");
    }
}

// Takes refuses an agent the library has no place in: it isn't recorded as
// given anything and then given nothing.
#[test]
fn test_takes_refuses_alma() {
    let sb = Sandbox::new();
    for kind in ["instructions", "mcp", "skills"] {
        let error = crate::library::takes(&sb.homes, "alma", kind)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Alma has no user-wide place"),
            "{kind}: {error}"
        );
    }
}

// Every format writes a server so that reading it back gives it again, and
// taking it out leaves the file as the user had it.
#[test]
fn test_server_every_format() {
    let sb = Sandbox::new();
    write(
        sb.home.join(".claude.json"),
        r#"{"numStartups": 3, "mcpServers": {"mine": {"command": "x"}}}"#,
    );
    write(
        sb.home.join(".codex/config.toml"),
        "model = \"gpt-5\"\n\n[mcp_servers.mine]\ncommand = \"x\"\n\n[profiles.a]\nmodel = \"o3\"\n",
    );
    write(
        sb.home.join(".config/goose/config.yaml"),
        "GOOSE_MODEL: x\nextensions:\n  developer:\n    enabled: true\n    type: builtin\n    name: developer\n",
    );
    write(
        sb.home.join(".config/opencode/opencode.json"),
        "{\n  // mine\n  \"theme\": \"x\"\n}\n",
    );
    let before: BTreeMap<String, String> = crate::library::targets(&sb.homes)
        .iter()
        .filter_map(|t| {
            t.mcp
                .as_ref()
                .map(|f| (t.agent.spec.id.to_owned(), read_file(&f.path)))
        })
        .collect();
    let all = ids(&crate::library::targets(&sb.homes));
    let stdio = stdio_server("fs", all.clone());
    let remote = remote_server("web", all.clone());
    ok(save_server_at(&sb.store, &sb.homes, "", stdio.clone()));
    ok(save_server_at(&sb.store, &sb.homes, "", remote.clone()));

    for t in &crate::library::targets(&sb.homes) {
        let Some(f) = &t.mcp else { continue };
        let got = read(f).unwrap();
        for want in [&stdio, &remote] {
            let s = got.get(&want.name).unwrap_or_else(|| {
                panic!(
                    "{}: {} missing\n{}",
                    t.agent.spec.id,
                    want.name,
                    read_file(&f.path)
                )
            });
            assert!(
                s.same(want),
                "{}: {} read back as {s:?}",
                t.agent.spec.id,
                want.name
            );
        }
        if !got.contains_key("mine") && before[t.agent.spec.id].contains("\"mine\"") {
            panic!("{} lost the user's server", t.agent.spec.id);
        }
    }
    let codex = read_file(sb.home.join(".codex/config.toml"));
    assert!(
        codex.starts_with("model = \"gpt-5\"") && codex.contains("[profiles.a]"),
        "codex config lost the user's:\n{codex}"
    );
    let opencode = read_file(sb.home.join(".config/opencode/opencode.json"));
    assert!(
        opencode.contains("// mine"),
        "opencode lost its comment:\n{opencode}"
    );

    ok(remove_server_at(&sb.store, &sb.homes, "fs"));
    ok(remove_server_at(&sb.store, &sb.homes, "web"));
    for t in &crate::library::targets(&sb.homes) {
        let Some(f) = &t.mcp else { continue };
        let got = read(f).unwrap();
        assert!(
            !got.contains_key("fs") && !got.contains_key("web"),
            "{} still has them:\n{}",
            t.agent.spec.id,
            read_file(&f.path)
        );
    }
    for id in ["codex", "goose"] {
        let t = crate::library::target_by_id(&sb.homes, id).unwrap();
        let f = t.mcp.as_ref().unwrap();
        let a = before[id].trim();
        let file = read_file(&f.path);
        let b = file.trim();
        assert_eq!(a, b, "{id} isn't as it was");
    }
}

#[test]
fn test_server_keeps_users_keys() {
    let sb = Sandbox::new();
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "fs".to_owned(),
            transport: "stdio".to_owned(),
            command: "npx".to_owned(),
            agents: vec![
                "codex".to_owned(),
                "copilot".to_owned(),
                "gemini".to_owned(),
            ],
            ..Server::default()
        },
    ));
    let cfg = sb.home.join(".codex/config.toml");
    let text = read_file(&cfg);
    write(
        &cfg,
        &text.replace("command = ", "startup_timeout_sec = 30\ncommand = "),
    );
    let cp = sb.home.join(".copilot/mcp-config.json");
    let text = read_file(&cp);
    write(&cp, &text.replacen(r#""*""#, r#""read""#, 1));
    let gm = sb.home.join(".gemini/settings.json");
    let mut g: Value = serde_json::from_str(&read_file(&gm)).unwrap();
    g["mcpServers"]["fs"]["trust"] = json!(true);
    write(&gm, &serde_json::to_string(&g).unwrap());

    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "fs",
        Server {
            name: "fs".to_owned(),
            transport: "stdio".to_owned(),
            command: "uvx".to_owned(),
            agents: vec![
                "codex".to_owned(),
                "copilot".to_owned(),
                "gemini".to_owned(),
            ],
            ..Server::default()
        },
    ));
    let codex = read_file(&cfg);
    assert!(
        codex.contains("startup_timeout_sec = 30") && codex.contains("\"uvx\""),
        "codex:\n{codex}"
    );
    let copilot = read_file(&cp);
    assert!(
        copilot.contains("\"read\"") && copilot.contains("\"uvx\""),
        "copilot:\n{copilot}"
    );
    let gemini: Value = serde_json::from_str(&read_file(&gm)).unwrap();
    assert_eq!(gemini["mcpServers"]["fs"]["trust"], json!(true));
    assert_eq!(gemini["mcpServers"]["fs"]["command"], json!("uvx"));
}

#[test]
fn test_del_codex_removes_child_tables() {
    let sb = Sandbox::new();
    let before = "[user]\nnote = '''\n[mcp_servers.x.fake]\n'''\n\n";
    let server = "[mcp_servers.x]\ncommand = \"runner\"\n\n";
    let env = "[mcp_servers.x.env]\nTOKEN = \"value\"\n\n";
    let arrays = "[[mcp_servers.x.env_vars]]\nname = \"FIRST\"\nsource = \"local\"\n\n[[mcp_servers.x.env_vars]]\nname = \"SECOND\"\nsource = \"local\"\n\n";
    let after = "[[skills.config]]\npath = \"/keep-the-skill\"\n\n[mcp_servers.xy]\ncommand = \"same prefix but another server\"\n\n[mcp_servers.y]\ncommand = \"keep\"\n";
    let path = sb.home.join("config.toml");
    write(&path, &format!("{before}{server}{env}{arrays}{after}"));
    codex_del(&path, "x").unwrap();
    let got = read_file(&path);
    assert_eq!(
        got,
        format!("{before}{after}"),
        "the server and its child tables should be gone"
    );
    // nothing implicitly recreated the removed server
    let f = McpFile {
        path: path.clone(),
        format: Format::Codex,
    };
    assert!(!entries(&f).unwrap().contains_key("x"));
}

#[test]
fn test_del_codex_parse_error_leaves_file_untouched() {
    let sb = Sandbox::new();
    let input = "[mcp_servers.x]\ncommand = \"runner\"\n\n[mcp_servers.x.env]\nTOKEN = \"value\"\n\n[other]\ninvalid = [\n";
    let path = sb.home.join("config.toml");
    write(&path, input);
    let error = codex_del(&path, "x").unwrap_err().to_string();
    assert!(
        error.contains("parse"),
        "expected a parse error, got {error}"
    );
    assert_eq!(read_file(&path), input);
}

#[test]
fn test_put_codex_preserves_child_array_values() {
    let sb = Sandbox::new();
    let other = "[mcp_servers.other]\ncommand = \"keep\"\n";
    let input = "[mcp_servers.x]\ncommand = \"old\"\n\n[[mcp_servers.x.env_vars]]\nname = \"FIRST\"\nsource = \"local\"\n\n[[mcp_servers.x.env_vars]]\nname = \"SECOND\"\nsource = \"local\"\n\n";
    let path = sb.home.join("config.toml");
    write(&path, &format!("{input}{other}"));
    let f = McpFile {
        path: path.clone(),
        format: Format::Codex,
    };
    let before = entries(&f).unwrap();
    put(
        &f,
        &Server {
            name: "x".to_owned(),
            transport: "stdio".to_owned(),
            command: "new".to_owned(),
            ..Server::default()
        },
        before.get("x"),
    )
    .unwrap();
    let after = entries(&f).unwrap();
    assert_eq!(after["x"]["command"], json!("new"));
    assert_eq!(
        after["x"]["env_vars"], before["x"]["env_vars"],
        "lost the user's array values while saving"
    );
    assert!(
        read_file(&path).ends_with(other),
        "changed the other server:\n{}",
        read_file(&path)
    );
    del(&f, "x").unwrap();
    let after = entries(&f).unwrap();
    assert!(!after.contains_key("x"));
    assert_eq!(read_file(&path), other);
}

#[test]
fn test_server_rename_and_agents() {
    let sb = Sandbox::new();
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "a".to_owned(),
            transport: "stdio".to_owned(),
            command: "x".to_owned(),
            agents: vec!["claude".to_owned(), "cursor".to_owned()],
            ..Server::default()
        },
    ));
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "a",
        Server {
            name: "b".to_owned(),
            transport: "stdio".to_owned(),
            command: "x".to_owned(),
            agents: vec!["claude".to_owned(), "cursor".to_owned()],
            ..Server::default()
        },
    ));
    let claude = read_file(sb.home.join(".claude.json"));
    assert!(
        !claude.contains("\"a\"") && claude.contains("\"b\""),
        "rename:\n{claude}"
    );
    ok(server_agents_at(
        &sb.store,
        &sb.homes,
        "b",
        vec!["claude".to_owned()],
    ));
    let cursor = read_file(sb.home.join(".cursor/mcp.json"));
    assert!(!cursor.contains("\"b\""), "cursor still has it:\n{cursor}");
    let error = save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "b".to_owned(),
            transport: "stdio".to_owned(),
            command: "y".to_owned(),
            ..Server::default()
        },
    )
    .unwrap_err();
    assert!(!error.to_string().is_empty());
    assert!(
        save_server_at(
            &sb.store,
            &sb.homes,
            "",
            Server {
                name: "../x".to_owned(),
                transport: "stdio".to_owned(),
                command: "y".to_owned(),
                ..Server::default()
            },
        )
        .is_err(),
        "a name that isn't one"
    );
}

// Codex and Goose can't reach a server over SSE: they're told so, the
// others get it.
#[test]
fn test_server_sse() {
    let sb = Sandbox::new();
    let r = save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "s".to_owned(),
            transport: "sse".to_owned(),
            url: "http://localhost:9/sse".to_owned(),
            agents: vec!["claude".to_owned(), "codex".to_owned(), "goose".to_owned()],
            ..Server::default()
        },
    )
    .unwrap();
    let mut who = r
        .problems
        .iter()
        .map(|p| p.agent.clone())
        .collect::<Vec<_>>();
    who.sort();
    assert_eq!(who, vec!["codex", "goose"], "problems: {:?}", r.problems);
    let t = crate::library::target_by_id(&sb.homes, "claude").unwrap();
    let got = read(t.mcp.as_ref().unwrap()).unwrap();
    let s = got.get("s").unwrap();
    assert_eq!(s.transport, "sse");
}

#[test]
fn test_import_server() {
    let sb = Sandbox::new();
    write(
        sb.home.join(".claude.json"),
        r#"{"mcpServers": {"gh": {"type": "stdio", "command": "gh-mcp", "args": ["serve"]}}}"#,
    );
    write(
        sb.home.join(".cursor/mcp.json"),
        r#"{"mcpServers": {"gh": {"command": "gh-mcp", "args": ["serve"]}}}"#,
    );
    write(
        sb.home.join(".gemini/settings.json"),
        r#"{"mcpServers": {"gh": {"command": "other"}}}"#,
    );
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    assert_eq!(v.found_servers.len(), 1);
    assert_eq!(v.found_servers[0].server.name, "gh");
    let f = &v.found_servers[0];
    assert_eq!(f.server.agents, vec!["claude", "cursor"]);
    assert_eq!(f.others, vec!["gemini"]);
    ok(import_server_at(&sb.store, &sb.homes, "gh"));
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    assert!(v.found_servers.is_empty() && v.servers.len() == 1);
    ok(remove_server_at(&sb.store, &sb.homes, "gh"));
    let gemini = read_file(sb.home.join(".gemini/settings.json"));
    assert!(gemini.contains("other"), "gemini's own went:\n{gemini}");
    let cursor = read_file(sb.home.join(".cursor/mcp.json"));
    assert!(!cursor.contains("gh-mcp"), "cursor kept it:\n{cursor}");
}

// The servers Codex's app writes into its config itself are told apart:
// its own, not ones to bring in.
#[test]
fn test_found_servers_app_owned() {
    let sb = Sandbox::new();
    write(
        sb.home.join(".codex/config.toml"),
        "[mcp_servers.node_repl]\ncommand = 'C:\\Users\\u\\AppData\\Local\\OpenAI\\Codex\\runtimes\\cua_node\\1.0\\node_repl.exe'\n\n[mcp_servers.cua_repl]\ncommand = 'C:\\Program Files\\WindowsApps\\OpenAI.Codex_1.0_x64\\ChatGPT.exe'\nenabled = false\n\n[mcp_servers.computer-use]\ncommand = \"./Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient\"\n\n[mcp_servers.gh]\ncommand = \"gh-mcp\"\n",
    );
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    let own: BTreeMap<String, bool> = v
        .found_servers
        .iter()
        .map(|f| (f.server.name.clone(), f.own))
        .collect();
    let want: BTreeMap<String, bool> = BTreeMap::from([
        ("node_repl".to_owned(), true),
        ("cua_repl".to_owned(), true),
        ("computer-use".to_owned(), true),
        ("gh".to_owned(), false),
    ]);
    assert_eq!(own, want);
}

// Pi's mcp.json is written so both its MCP extensions read the transport,
// and a server written there as the extensions' READMEs have it is read.
#[test]
fn test_pi_mcp() {
    let sb = Sandbox::new();
    let p = sb.home.join(".pi/agent/mcp.json");
    write(
        &p,
        r#"{"mcpServers": {"supabase": {"transport": "streamable-http", "url": "https://mcp.supabase.com/mcp", "lifecycle": "eager"}}}"#,
    );
    let t = crate::library::target_by_id(&sb.homes, "pi").unwrap();
    let f = t.mcp.as_ref().unwrap();
    assert_eq!(f.path, p);
    let got = read(f).unwrap();
    let s = got.get("supabase").unwrap();
    assert_eq!(s.transport, "http");
    assert_eq!(s.url, "https://mcp.supabase.com/mcp");
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "ev".to_owned(),
            transport: "sse".to_owned(),
            url: "https://example.com/sse".to_owned(),
            agents: vec!["pi".to_owned()],
            ..Server::default()
        },
    ));
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "supabase".to_owned(),
            transport: "http".to_owned(),
            url: "https://mcp.supabase.com/mcp".to_owned(),
            agents: vec!["pi".to_owned()],
            ..Server::default()
        },
    ));
    let doc: Value = serde_json::from_str(&read_file(&p)).unwrap();
    let ev = &doc["mcpServers"]["ev"];
    assert_eq!(ev["transport"], json!("sse"));
    assert_eq!(ev["httpTransport"], json!("sse"));
    assert_eq!(ev["url"], json!("https://example.com/sse"));
    let supabase = &doc["mcpServers"]["supabase"];
    assert_eq!(supabase["lifecycle"], json!("eager"));
    assert_eq!(supabase["transport"], json!("streamable-http"));
}

// Claude Desktop is given only the servers it runs itself: a remote one is
// its Connectors', and the page says so.
#[test]
fn test_claude_desktop_mcp() {
    let sb = Sandbox::new();
    let p = sb.home.join(".config/Claude/claude_desktop_config.json");
    write(
        &p,
        r#"{"globalShortcut": "Alt+Space", "mcpServers": {"mine": {"command": "x"}}}"#,
    );
    let t = crate::library::target_by_id(&sb.homes, "claude-desktop").unwrap();
    assert!(t.instructions.is_none() && t.skills.is_none());
    let f = t.mcp.as_ref().unwrap();
    assert_eq!(f.path, p);
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "fs".to_owned(),
            transport: "stdio".to_owned(),
            command: "npx".to_owned(),
            args: vec!["-y".to_owned(), "fs".to_owned()],
            agents: vec!["claude-desktop".to_owned()],
            ..Server::default()
        },
    ));
    let r = save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "web".to_owned(),
            transport: "http".to_owned(),
            url: "https://example.com/mcp".to_owned(),
            agents: vec!["claude-desktop".to_owned()],
            ..Server::default()
        },
    )
    .unwrap();
    assert_eq!(r.problems.len(), 1);
    assert_eq!(r.problems[0].error, "no-remote");
    let doc: Value = serde_json::from_str(&read_file(&p)).unwrap();
    let fs = &doc["mcpServers"]["fs"];
    assert_eq!(fs["command"], json!("npx"));
    assert_eq!(fs.get("type"), None, "fs written with a type: {fs}");
    assert!(doc["mcpServers"].get("web").is_none());
    assert_eq!(doc["mcpServers"]["mine"]["command"], json!("x"));
    assert_eq!(doc["globalShortcut"], json!("Alt+Space"));
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    let a = v.agents.iter().find(|a| a.id == "claude-desktop").unwrap();
    assert!(
        a.no_remote,
        "claude-desktop not said to take no remote server"
    );
    let s = v.servers.iter().find(|s| s.server.name == "web").unwrap();
    assert_eq!(
        s.problems.get("claude-desktop").map(String::as_str),
        Some("no-remote")
    );
}

// A server on no agent is listed with none, not null: the page looks in
// the list, and a null blanked the whole Library.
#[test]
fn test_read_no_agents() {
    let sb = Sandbox::new();
    ok(save_server_at(
        &sb.store,
        &sb.homes,
        "",
        Server {
            name: "lone".to_owned(),
            transport: "stdio".to_owned(),
            command: "lone-mcp".to_owned(),
            ..Server::default()
        },
    ));
    let v = crate::library::read_at(&sb.store, &sb.homes, &[]).unwrap();
    let b = serde_json::to_string(&v.servers).unwrap();
    assert!(b.contains(r#""agents":[]"#), "servers: {b}");
}
