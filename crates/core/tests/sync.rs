use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use mcpgw_core::state::ManagedState;
use mcpgw_core::sync::{
    apply_plan, apply_plan_to, client_entry, per_server_gateway_server, per_server_gateway_servers,
    plan_client_context, plan_sync,
};
use mcpgw_core::{ClientKind, Config, backup};

fn canonical() -> BTreeMap<String, mcpgw_core::Server> {
    Config::parse(
        r#"
version = 1

[servers.github]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { TOKEN = "t" }

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"

[servers.parked]
type = "stdio"
command = "npx"
enabled = false
"#,
        Path::new("c.toml"),
    )
    .unwrap()
    .servers
}

fn managed(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|&n| n.to_owned()).collect()
}

#[test]
fn plan_covers_all_categories() {
    // Client currently has: an outdated managed github, a managed entry
    // whose canonical source is now disabled, a user's own linear
    // (conflict), and an unrelated foreign entry.
    let current_json = serde_json::json!({
        "github": { "command": "npx", "args": ["old"] },
        "parked": { "command": "npx" },
        "linear": { "url": "https://user-added.example/mcp" },
        "users-own": { "command": "deno" }
    });
    let current = current_json.as_object().unwrap();
    let plan = plan_sync(
        ClientKind::Cursor,
        current,
        &canonical(),
        &managed(&["github", "parked"]),
    );
    assert_eq!(plan.updates, ["github"]);
    assert_eq!(plan.removes, ["parked"]);
    assert_eq!(plan.conflicts, ["linear"]);
    assert_eq!(plan.foreign, ["users-own"]);
    assert!(plan.adds.is_empty());
    assert!(plan.has_changes());
    // The conflicted name must not become managed.
    assert_eq!(plan.managed_after(), managed(&["github"]));
}

#[test]
fn plan_is_idempotent_after_apply() {
    let mut root = serde_json::json!({
        "otherSetting": true,
        "mcpServers": { "users-own": { "command": "deno" } }
    });
    let plan = plan_sync(
        ClientKind::Cursor,
        root["mcpServers"].as_object().unwrap(),
        &canonical(),
        &managed(&[]),
    );
    assert_eq!(plan.adds, ["github", "linear"]);
    apply_plan(ClientKind::Cursor, &mut root, &plan).unwrap();

    // Foreign entry and unrelated root keys survive.
    assert_eq!(root["otherSetting"], true);
    assert_eq!(root["mcpServers"]["users-own"]["command"], "deno");
    insta::assert_snapshot!(serde_json::to_string_pretty(&root).unwrap());

    // A second plan over the applied state sees nothing to do.
    let again = plan_sync(
        ClientKind::Cursor,
        root["mcpServers"].as_object().unwrap(),
        &canonical(),
        &plan.managed_after(),
    );
    assert!(!again.has_changes());
}

/// Cline's `disabled` and `autoApprove` are the user's, set from inside
/// Cline and unexpressible canonically. Sync used to write the emitted entry
/// straight over them, so a managed server the user had switched off came
/// back on — and the entry differed from the file again the next run, and the
/// one after that, forever.
#[test]
fn a_managed_entry_keeps_the_fields_the_client_owns() {
    let canonical = canonical();
    let mut on_disk = client_entry(ClientKind::Cline, &canonical["github"]);
    on_disk["disabled"] = true.into();
    on_disk["autoApprove"] = serde_json::json!(["list_issues"]);
    let current = serde_json::json!({ "github": on_disk });

    // Nothing to do to it: the entry is what mcpgw would write plus what
    // Cline owns, which is exactly the state the previous sync left behind.
    // Without the carry-over it re-diffed on this run and every one after.
    let plan = plan_sync(
        ClientKind::Cline,
        current.as_object().unwrap(),
        &canonical,
        &managed(&["github"]),
    );
    assert!(plan.updates.is_empty(), "{plan:?}");

    // And when the canonical server really has changed, the rewrite carries
    // both fields over rather than resetting them.
    let mut stale = current.clone();
    stale["github"]["command"] = "old".into();
    let plan = plan_sync(
        ClientKind::Cline,
        stale.as_object().unwrap(),
        &canonical,
        &managed(&["github"]),
    );
    assert_eq!(plan.updates, ["github"]);
    let mut root = serde_json::json!({ "mcpServers": stale });
    apply_plan(ClientKind::Cline, &mut root, &plan).unwrap();
    let written = &root["mcpServers"]["github"];
    assert_eq!(written["command"], "npx");
    assert_eq!(written["disabled"], true);
    assert_eq!(written["autoApprove"], serde_json::json!(["list_issues"]));

    // A client that defines none of those fields is unaffected: its entry is
    // the emitted value and nothing else, so the stray fields are a diff.
    let plan = plan_sync(
        ClientKind::Cursor,
        current.as_object().unwrap(),
        &canonical,
        &managed(&["github"]),
    );
    assert_eq!(plan.updates, ["github"]);
}

/// Amp and Zoo Code carry their own subsets of the same fields, and every
/// other client carries none — a client that gained one silently would start
/// echoing back a field mcpgw does not understand.
#[test]
fn the_fields_each_client_owns_are_what_they_were() {
    let owned: Vec<(&str, Vec<&str>)> = ClientKind::ALL
        .into_iter()
        .map(|kind| (kind.id(), kind.codec().entries.preserved_fields().to_vec()))
        .collect();
    insta::assert_debug_snapshot!(owned);
}

/// Gemini has no per-entry off switch: a name in `mcp.excluded` stops the
/// server whatever its entry says. Writing the entry and leaving the list
/// alone reported `+ name` for a server Gemini refuses to start, and the next
/// plan then saw the entry already correct and nothing left to do.
#[test]
fn gemini_takes_only_its_own_names_out_of_the_excluded_list() {
    let canonical = canonical();
    let document = serde_json::json!({
        "mcp": { "excluded": ["github", "parked", "users-own", "github"] },
        "mcpServers": {
            "github": client_entry(ClientKind::Gemini, &canonical["github"]),
            "parked": { "command": "npx" },
            "users-own": { "command": "deno" },
        }
    });
    let mut plan = plan_sync(
        ClientKind::Gemini,
        document["mcpServers"].as_object().unwrap(),
        &canonical,
        &managed(&["github", "parked"]),
    );
    // `github` already matches byte for byte — that is the case which used
    // to stay silently wrong forever, because nothing else in the plan
    // touched it either.
    assert!(!plan.updates.contains(&"github".to_owned()));
    plan_client_context(ClientKind::Gemini, &document, &mut plan);

    // A managed server that has to run, and a managed name whose entry is
    // going away — leaving that one listed would silently disable a server
    // the user later re-added by hand. Deduplicated, and the user's own
    // exclusion of an entry mcpgw does not manage is left where it is.
    assert_eq!(plan.unexclude, ["github", "parked"]);
    assert!(plan.has_changes());

    let mut doc = mcpgw_core::clients::codec::ClientDocument::Json(document);
    apply_plan_to(ClientKind::Gemini, &mut doc, &plan).unwrap();
    assert_eq!(
        doc.to_value()["mcp"]["excluded"],
        serde_json::json!(["users-own"])
    );

    // A second plan over the written state has nothing left to do.
    let written = doc.to_value();
    let mut again = plan_sync(
        ClientKind::Gemini,
        written["mcpServers"].as_object().unwrap(),
        &canonical,
        &plan.managed_after(),
    );
    plan_client_context(ClientKind::Gemini, &written, &mut again);
    assert!(!again.has_changes(), "{again:?}");
}

/// Only Gemini keeps part of a server's state outside its entry; for every
/// other client the pass is a no-op whatever the document holds.
#[test]
fn no_other_client_has_an_exclusion_list_to_reconcile() {
    for kind in ClientKind::ALL {
        assert_eq!(
            kind.exclusion_list().is_some(),
            kind == ClientKind::Gemini,
            "{}",
            kind.id()
        );
    }
}

#[test]
fn apply_creates_root_key_in_empty_document() {
    let mut root = serde_json::json!({});
    let plan = plan_sync(
        ClientKind::VsCode,
        &serde_json::Map::new(),
        &canonical(),
        &managed(&[]),
    );
    apply_plan(ClientKind::VsCode, &mut root, &plan).unwrap();
    assert!(root["servers"]["github"].is_object());
}

#[test]
fn entry_shapes_per_client() {
    let canonical = canonical();
    let vs_stdio = client_entry(ClientKind::VsCode, &canonical["github"]);
    let cursor_stdio = client_entry(ClientKind::Cursor, &canonical["github"]);
    let cursor_http = client_entry(ClientKind::Cursor, &canonical["linear"]);
    // VS Code carries an explicit type on stdio; mcpServers clients don't.
    assert_eq!(vs_stdio["type"], "stdio");
    assert!(cursor_stdio.get("type").is_none());
    assert_eq!(cursor_http["type"], "http");

    // Gemini has no `type`, and its remote field must be `httpUrl`: writing
    // `url` there would configure the legacy SSE transport instead.
    let gemini_stdio = client_entry(ClientKind::Gemini, &canonical["github"]);
    let gemini_http = client_entry(ClientKind::Gemini, &canonical["linear"]);
    assert_eq!(gemini_stdio, cursor_stdio);
    assert_eq!(gemini_http["httpUrl"], "https://mcp.linear.app/mcp");
    assert!(gemini_http.get("url").is_none());
    assert!(gemini_http.get("type").is_none());

    // Codex spells remote entries `url` + `http_headers`, and its stdio
    // shape is the plain one — the TOML rendering is the codec's job.
    let codex_stdio = client_entry(ClientKind::Codex, &canonical["github"]);
    let codex_http = client_entry(ClientKind::Codex, &canonical["linear"]);
    assert_eq!(codex_stdio, cursor_stdio);
    assert_eq!(codex_http["url"], "https://mcp.linear.app/mcp");
    assert!(codex_http.get("type").is_none());
    assert!(codex_http.get("headers").is_none());
    let with_headers = client_entry(
        ClientKind::Codex,
        &mcpgw_core::Server {
            enabled: true,
            tags: Vec::new(),
            transport: mcpgw_core::Transport::Http {
                url: "https://h.example/mcp".to_owned(),
                headers: [("Authorization".to_owned(), "Bearer t".to_owned())]
                    .into_iter()
                    .collect(),
            },
        },
    );
    assert_eq!(with_headers["http_headers"]["Authorization"], "Bearer t");

    // opencode spells the transport `local`/`remote`, folds the program and
    // its arguments into one array, and calls the variables `environment`.
    let opencode_stdio = client_entry(ClientKind::Opencode, &canonical["github"]);
    let opencode_http = client_entry(ClientKind::Opencode, &canonical["linear"]);
    assert_eq!(opencode_stdio["type"], "local");
    assert_eq!(
        opencode_stdio["command"],
        serde_json::json!(["npx", "-y", "@modelcontextprotocol/server-github"])
    );
    assert!(opencode_stdio.get("args").is_none());
    assert_eq!(opencode_stdio["environment"]["TOKEN"], "t");
    assert!(opencode_stdio.get("env").is_none());
    assert_eq!(opencode_http["type"], "remote");
    assert_eq!(opencode_http["url"], "https://mcp.linear.app/mcp");
    assert!(opencode_http.get("headers").is_none());

    // Windsurf is the shared stdio shape, and `serverUrl` for remote — a
    // plain `url` there is a field Windsurf does not read.
    let windsurf_stdio = client_entry(ClientKind::Windsurf, &canonical["github"]);
    let windsurf_http = client_entry(ClientKind::Windsurf, &canonical["linear"]);
    assert_eq!(windsurf_stdio, cursor_stdio);
    assert_eq!(windsurf_http["serverUrl"], "https://mcp.linear.app/mcp");
    assert!(windsurf_http.get("url").is_none());
    assert!(windsurf_http.get("type").is_none());

    // Zed is the shared stdio shape plus the `source` a Zed old enough to
    // discriminate on it requires — an entry written without it is one that
    // Zed drops on the floor. A remote entry gets no `source`: Zed's remote
    // shape is a bare `{url, headers}`, and a discriminator naming the
    // variant that carries `command` has no business on one that carries a
    // URL instead.
    let zed_stdio = client_entry(ClientKind::Zed, &canonical["github"]);
    let zed_http = client_entry(ClientKind::Zed, &canonical["linear"]);
    assert_eq!(zed_stdio["source"], "custom");
    assert_eq!(zed_stdio["command"], "npx");
    assert_eq!(zed_stdio["env"]["TOKEN"], "t");
    assert!(zed_http.get("source").is_none(), "{zed_http}");
    assert_eq!(zed_http["url"], "https://mcp.linear.app/mcp");
    assert!(zed_http.get("type").is_none());
    // Which leaves it byte-identical to Amp's remote shape, and that is the
    // shape Zed documents.
    assert_eq!(
        zed_http,
        client_entry(ClientKind::Amp, &canonical["linear"])
    );

    // Cline is the shared stdio shape; a remote entry needs its camelCase
    // `type`, because an untyped one means the legacy SSE transport there.
    let cline_stdio = client_entry(ClientKind::Cline, &canonical["github"]);
    let cline_http = client_entry(ClientKind::Cline, &canonical["linear"]);
    assert_eq!(cline_stdio, cursor_stdio);
    assert_eq!(cline_http["type"], "streamableHttp");
    assert_eq!(cline_http["url"], "https://mcp.linear.app/mcp");
    // `disabled` and `autoApprove` are Cline's own; mcpgw writes neither.
    assert!(cline_http.get("disabled").is_none());
    assert!(cline_stdio.get("autoApprove").is_none());
    // Both surfaces write the same bytes.
    assert_eq!(
        client_entry(ClientKind::ClineCli, &canonical["linear"]),
        cline_http
    );

    // Amp is the shared stdio shape, and a bare `url` for remote: it has no
    // `type` field, so writing one would be a field its schema does not have.
    let amp_stdio = client_entry(ClientKind::Amp, &canonical["github"]);
    let amp_http = client_entry(ClientKind::Amp, &canonical["linear"]);
    assert_eq!(amp_stdio, cursor_stdio);
    assert_eq!(amp_http["url"], "https://mcp.linear.app/mcp");
    assert!(amp_http.get("type").is_none());
    // `source` is Zed's, not Amp's, however alike the two shapes look.
    assert!(amp_http.get("source").is_none());
    assert!(amp_stdio.get("disabled").is_none());

    // Zoo Code is Cline's shape with the one spelling its schema accepts:
    // hyphenated, where Cline's is camelCase. Writing Cline's here would be
    // a value Zoo Code's own validator rejects.
    let zoo_stdio = client_entry(ClientKind::ZooCode, &canonical["github"]);
    let zoo_http = client_entry(ClientKind::ZooCode, &canonical["linear"]);
    assert_eq!(zoo_stdio, cursor_stdio);
    assert_eq!(zoo_http["type"], "streamable-http");
    assert_eq!(zoo_http["url"], "https://mcp.linear.app/mcp");
    assert_ne!(zoo_http["type"], cline_http["type"]);
    // Zoo Code's own bookkeeping fields are never written by mcpgw.
    assert!(zoo_http.get("disabled").is_none());
    assert!(zoo_stdio.get("alwaysAllow").is_none());

    insta::assert_snapshot!(serde_json::to_string_pretty(&vs_stdio).unwrap());
}

/// The clients whose entry shape is rewritten rather than passed through:
/// opencode splits and rejoins the `command` array, Windsurf renames the
/// remote URL, Zed adds a `source` its reader has to ignore, Cline spells the
/// remote type its own way, Amp drops the `type` the shared shape carries,
/// and Zoo Code spells that type a third way again.
/// Emitting and re-reading has to give back the server that went in — for
/// every client, but for these it is load bearing rather than incidental.
#[test]
fn emitting_and_re_reading_an_entry_returns_the_same_server() {
    let mut servers: Vec<mcpgw_core::Server> = canonical().into_values().collect();
    // Disabled is a canonical fact, not an emitted one: a client only ever
    // receives entries that are on.
    for server in &mut servers {
        server.enabled = true;
    }
    servers.push(mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: [(
                "Authorization".to_owned(),
                "Bearer {env:LINEAR_TOKEN}".to_owned(),
            )]
            .into_iter()
            .collect(),
        },
    });
    // A bare command with no arguments: the array must not collapse.
    servers.push(mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Stdio {
            command: "notes-mcp".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    });

    for kind in [
        ClientKind::Opencode,
        ClientKind::Windsurf,
        ClientKind::Zed,
        ClientKind::Cline,
        ClientKind::ClineCli,
        ClientKind::Amp,
        ClientKind::ZooCode,
    ] {
        let entries = kind.codec().entries;
        for server in &servers {
            let emitted = entries.emit(server);
            let (parsed, note) = entries.parse(&emitted).unwrap();
            assert_eq!(
                &parsed,
                server,
                "{} round trip changed {emitted}",
                kind.id()
            );
            assert_eq!(note, None);
        }
    }
}

/// The whole per-server emission, one golden entry per client kind. Written
/// out rather than field-probed: the failure this guards against is a client
/// gaining or losing a field, which a probe for the fields we thought of
/// cannot see.
#[test]
fn per_server_gateway_entry_shapes_per_client() {
    let base = "http://127.0.0.1:8137/mcp";
    let url = "http://127.0.0.1:8137/s/github";
    let canonical = canonical();
    let http = |extra: serde_json::Value| {
        let mut entry = serde_json::json!({ "url": url });
        for (key, value) in extra.as_object().unwrap() {
            entry[key] = value.clone();
        }
        entry
    };
    let expected = |kind| match kind {
        // Claude Desktop cannot take a URL at all, so it gets the bridge —
        // naming the server, with the gateway's own base URL beside it.
        ClientKind::ClaudeDesktop => serde_json::json!({
            "command": "mcpgw",
            "args": ["connect", "--server", "github", "--url", base],
        }),
        ClientKind::ClaudeCode | ClientKind::Cursor | ClientKind::VsCode => {
            http(serde_json::json!({ "type": "http" }))
        }
        ClientKind::Gemini => serde_json::json!({ "httpUrl": url }),
        // A gateway entry is remote, so Zed's `source` is not on it either.
        ClientKind::Codex | ClientKind::Amp | ClientKind::Zed => http(serde_json::json!({})),
        ClientKind::Opencode => http(serde_json::json!({ "type": "remote" })),
        ClientKind::Windsurf => serde_json::json!({ "serverUrl": url }),
        ClientKind::Cline | ClientKind::ClineCli => {
            http(serde_json::json!({ "type": "streamableHttp" }))
        }
        ClientKind::ZooCode => http(serde_json::json!({ "type": "streamable-http" })),
    };

    for kind in ClientKind::ALL {
        let server =
            per_server_gateway_server(kind, "github", &canonical["github"], base, "mcpgw").unwrap();
        assert_eq!(client_entry(kind, &server), expected(kind), "{}", kind.id());
    }

    // A gateway on another port moves the endpoint with it, path and all.
    let moved = per_server_gateway_server(
        ClientKind::Cursor,
        "github",
        &canonical["github"],
        "http://127.0.0.1:9000",
        "mcpgw",
    )
    .unwrap();
    assert_eq!(
        client_entry(ClientKind::Cursor, &moved)["url"],
        "http://127.0.0.1:9000/s/github"
    );
    assert!(
        per_server_gateway_server(
            ClientKind::Cursor,
            "github",
            &canonical["github"],
            "not a url",
            "mcpgw"
        )
        .is_err()
    );

    // Claude Desktop is the one client that cannot take a URL, and so the one
    // that gets the bridge: an adapter that lands without saying which side it
    // is on fails here rather than emitting the wrong entry shape in silence.
    for kind in ClientKind::ALL {
        assert_eq!(
            kind.supports_http_entries(),
            kind != ClientKind::ClaudeDesktop
        );
    }
}

/// The point of naming per-server entries after their server: flipping a
/// client from direct to gateway mode rewrites entries mcpgw already manages,
/// so it is a set of updates. Under any other naming they would be adds
/// beside stale removes — or, worse, conflicts.
#[test]
fn flipping_to_per_server_gateway_mode_updates_the_same_names() {
    let canonical = canonical();
    let base = "http://127.0.0.1:8137/mcp";
    // What a direct sync left behind, plus an entry of the user's own.
    let current = serde_json::json!({
        "github": client_entry(ClientKind::Cursor, &canonical["github"]),
        "linear": client_entry(ClientKind::Cursor, &canonical["linear"]),
        "users-own": { "command": "deno" },
    });
    let desired =
        per_server_gateway_servers(ClientKind::Cursor, &canonical, base, "mcpgw").unwrap();
    let plan = plan_sync(
        ClientKind::Cursor,
        current.as_object().unwrap(),
        &desired,
        &managed(&["github", "linear"]),
    );
    assert_eq!(plan.updates, ["github", "linear"]);
    assert!(plan.adds.is_empty(), "{plan:?}");
    assert!(plan.conflicts.is_empty(), "{plan:?}");
    assert_eq!(plan.foreign, ["users-own"]);
    // The disabled canonical server is in the desired map but not mirrored:
    // per-server mode inherits direct mode's rule rather than restating it.
    assert!(!desired["parked"].enabled);
    assert_eq!(plan.managed_after(), managed(&["github", "linear"]));

    let mut root = serde_json::json!({ "mcpServers": current });
    apply_plan(ClientKind::Cursor, &mut root, &plan).unwrap();
    let entries = &root["mcpServers"];
    assert_eq!(entries["github"]["url"], "http://127.0.0.1:8137/s/github");
    assert_eq!(entries["linear"]["url"], "http://127.0.0.1:8137/s/linear");
    assert_eq!(entries["users-own"]["command"], "deno");

    // And the second run has nothing to do.
    let again = plan_sync(
        ClientKind::Cursor,
        entries.as_object().unwrap(),
        &desired,
        &plan.managed_after(),
    );
    assert!(!again.has_changes(), "{again:?}");
}

/// The migration off the old single-entry gateway shape: `mcpgw` was managed,
/// so it is a plain remove, and the per-server names come in beside it.
///
/// The entry is a literal because nothing emits that shape any more — which is
/// exactly why the bytes a 0.3.x `sync --aggregate` left in a live config are
/// worth pinning here.
#[test]
fn migrating_off_the_aggregate_entry_removes_it() {
    let canonical = canonical();
    let base = "http://127.0.0.1:8137/mcp";
    let current = serde_json::json!({
        "mcpgw": { "type": "http", "url": base },
    });
    let desired =
        per_server_gateway_servers(ClientKind::Cursor, &canonical, base, "mcpgw").unwrap();
    let plan = plan_sync(
        ClientKind::Cursor,
        current.as_object().unwrap(),
        &desired,
        &managed(&["mcpgw"]),
    );
    assert_eq!(plan.adds, ["github", "linear"]);
    assert_eq!(plan.removes, ["mcpgw"]);
    assert!(plan.conflicts.is_empty(), "{plan:?}");
}

/// A per-server gateway entry is the same server under the same name, so the
/// switch the user flipped inside the client applies to it as it did to the
/// direct entry: mode is mcpgw's decision, "off" is theirs.
#[test]
fn per_server_gateway_entries_keep_the_fields_the_client_owns() {
    let canonical = canonical();
    let base = "http://127.0.0.1:8137/mcp";
    let mut on_disk = client_entry(ClientKind::Cline, &canonical["github"]);
    on_disk["disabled"] = true.into();
    on_disk["autoApprove"] = serde_json::json!(["list_issues"]);
    let current = serde_json::json!({ "github": on_disk });

    let desired = per_server_gateway_servers(ClientKind::Cline, &canonical, base, "mcpgw").unwrap();
    let plan = plan_sync(
        ClientKind::Cline,
        current.as_object().unwrap(),
        &desired,
        &managed(&["github"]),
    );
    assert_eq!(plan.updates, ["github"]);
    let mut root = serde_json::json!({ "mcpServers": current });
    apply_plan(ClientKind::Cline, &mut root, &plan).unwrap();
    let written = &root["mcpServers"]["github"];
    assert_eq!(written["url"], "http://127.0.0.1:8137/s/github");
    assert_eq!(written["type"], "streamableHttp");
    assert!(written.get("command").is_none());
    assert_eq!(written["disabled"], true);
    assert_eq!(written["autoApprove"], serde_json::json!(["list_issues"]));
}

/// Per-server mode reaches Gemini's out-of-entry state through the same pass
/// direct mode does — the names are the same, so there is nothing to special
/// case, only something to keep true.
#[test]
fn per_server_gateway_sync_unexcludes_its_own_names_in_gemini() {
    let canonical = canonical();
    let base = "http://127.0.0.1:8137/mcp";
    let document = serde_json::json!({
        "mcp": { "excluded": ["github", "users-own"] },
        "mcpServers": {
            "github": client_entry(ClientKind::Gemini, &canonical["github"]),
            "users-own": { "command": "deno" },
        }
    });
    let desired =
        per_server_gateway_servers(ClientKind::Gemini, &canonical, base, "mcpgw").unwrap();
    let mut plan = plan_sync(
        ClientKind::Gemini,
        document["mcpServers"].as_object().unwrap(),
        &desired,
        &managed(&["github"]),
    );
    plan_client_context(ClientKind::Gemini, &document, &mut plan);
    assert_eq!(plan.unexclude, ["github"]);
    assert_eq!(
        client_entry(ClientKind::Gemini, &desired["github"])["httpUrl"],
        "http://127.0.0.1:8137/s/github"
    );
}

#[test]
fn backups_prune_to_keep_and_latest_wins() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let file = dir.path().join("mcp.json");
    for i in 0..8 {
        std::fs::write(&file, format!("{{\"gen\": {i}}}")).unwrap();
        backup::backup_file(&state_dir, "cursor", &file).unwrap();
    }
    let backups: Vec<_> = std::fs::read_dir(state_dir.join("backups/cursor"))
        .unwrap()
        .collect();
    assert_eq!(backups.len(), backup::KEEP);
    let latest = backup::latest_backup(&state_dir, "cursor")
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(latest).unwrap(), "{\"gen\": 7}");
    assert!(
        backup::latest_backup(&state_dir, "vscode")
            .unwrap()
            .is_none()
    );
}

#[test]
fn state_round_trips_and_tolerates_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.json");
    assert_eq!(ManagedState::load(&path).unwrap(), ManagedState::default());

    let mut state = ManagedState::default();
    state
        .clients
        .insert("cursor".to_owned(), managed(&["github"]));
    state.save(&path).unwrap();
    assert_eq!(ManagedState::load(&path).unwrap(), state);
}

/// Every state file on disk today was written before `migrated` existed.
/// Reading one has to keep working, and has to read as "not told yet" — the
/// installs with an old state file are exactly the ones the migration notice
/// is for.
#[test]
fn a_state_file_written_before_the_migrated_flag_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.json");
    std::fs::write(&path, r#"{"clients":{"cursor":["github"]}}"#).unwrap();

    let state = ManagedState::load(&path).unwrap();
    assert_eq!(state.clients["cursor"], managed(&["github"]));
    assert!(!state.migrated);
}

#[test]
fn client_ids_round_trip() {
    for kind in ClientKind::ALL {
        assert_eq!(ClientKind::from_id(kind.id()), Some(kind));
    }
    assert_eq!(ClientKind::from_id("emacs"), None);
}

/// Zed's `source` is the discriminator of a shape that carries `command`.
/// Putting it on a `url` entry described a variant the entry does not match,
/// and an entry Zed cannot deserialize is one it drops without a word — a
/// server synced and then silently never loaded.
#[test]
fn zed_writes_source_on_stdio_entries_only() {
    let stdio = mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Stdio {
            command: "npx".to_owned(),
            args: vec!["server-github".to_owned()],
            env: std::collections::BTreeMap::new(),
        },
    };
    let remote = mcpgw_core::Server {
        enabled: true,
        tags: Vec::new(),
        transport: mcpgw_core::Transport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            headers: [("Authorization".to_owned(), "Bearer t".to_owned())]
                .into_iter()
                .collect(),
        },
    };

    let written = client_entry(ClientKind::Zed, &stdio);
    assert_eq!(written["source"], "custom");

    let written = client_entry(ClientKind::Zed, &remote);
    assert!(written.get("source").is_none(), "{written}");
    // And nothing else was traded away for it: the documented remote shape is
    // exactly these two keys.
    assert_eq!(
        written,
        serde_json::json!({
            "url": "https://mcp.linear.app/mcp",
            "headers": {"Authorization": "Bearer t"},
        })
    );
}
