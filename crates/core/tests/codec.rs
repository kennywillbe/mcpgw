//! Codec-level tests for the machinery the four current clients do not
//! exercise: comment-preserving JSONC and TOML edits, and nested root-key
//! addressing. Each upcoming client adapter is then a fact — a format, a
//! root path, an entry schema — on top of a seam that is already covered.

use mcpgw_core::ClientKind;
use mcpgw_core::clients::codec::{ClientDocument, Codec, EntrySchema, Format, RootPath};
use serde_json::{Value, json};

/// A codec for a client that does not exist yet, so the machinery is tested
/// without pinning a real adapter to a shape N2+ may still discover.
fn codec(format: Format, root: RootPath) -> Codec {
    Codec {
        format,
        root,
        entries: EntrySchema::McpServers,
    }
}

fn edit(doc: &mut ClientDocument, root: RootPath, removes: &[&str], upserts: &[(&str, Value)]) {
    let removes: Vec<String> = removes.iter().map(|&n| n.to_owned()).collect();
    let upserts: Vec<(&str, &Value)> = upserts.iter().map(|(n, v)| (*n, v)).collect();
    doc.edit(root, &removes, &upserts);
}

const JSONC: &str = r#"// Hand-written config, comments and all.
{
  // The servers I added myself.
  "mcpServers": {
    // Keep this one exactly as it is.
    "mine": { "command": "deno" },
    "stale": { "command": "gone" },
    "outdated": { "command": "old" },
  },
  "telemetry": false, // and a trailing note
}
"#;

#[test]
fn jsonc_entry_edits_keep_every_comment_and_untouched_byte() {
    let root = RootPath::new(&["mcpServers"]);
    let codec = codec(Format::Jsonc, root);
    let mut doc = codec.parse_document(JSONC).unwrap();
    edit(
        &mut doc,
        root,
        &["stale"],
        &[
            ("outdated", json!({ "command": "new" })),
            ("added", json!({ "command": "npx", "args": ["-y", "srv"] })),
        ],
    );

    let text = doc.to_text().unwrap();
    for comment in [
        "// Hand-written config, comments and all.",
        "// The servers I added myself.",
        "// Keep this one exactly as it is.",
        "// and a trailing note",
    ] {
        assert!(text.contains(comment), "lost {comment:?} in:\n{text}");
    }
    // The untouched entry keeps its own single-line spelling, which a
    // reserialize-everything write would have expanded.
    assert!(text.contains(r#""mine": { "command": "deno" }"#), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn jsonc_edits_are_idempotent_on_the_values_not_the_text() {
    let root = RootPath::new(&["mcpServers"]);
    let codec = codec(Format::Jsonc, root);
    let entry = json!({ "command": "npx", "args": ["-y", "srv"] });

    let mut doc = codec.parse_document(JSONC).unwrap();
    edit(&mut doc, root, &[], &[("added", entry.clone())]);
    let text = doc.to_text().unwrap();

    // Re-reading the written file yields exactly the emitted value, so the
    // next plan sees no change and the file is never rewritten for
    // cosmetic reasons.
    let again = codec.parse_document(&text).unwrap();
    assert_eq!(again.entries(root)["added"], entry);
    assert_eq!(again.entries(root)["mine"], json!({ "command": "deno" }));
}

const TOML: &str = r#"# My codex config.
model = "gpt-5"

[mcp_servers.mine]
# Hand-written; leave it alone.
command = "deno"

[mcp_servers.stale]
command = "gone"
"#;

#[test]
fn toml_entry_edits_keep_every_comment_and_untouched_byte() {
    let root = RootPath::new(&["mcp_servers"]);
    let codec = codec(Format::Toml, root);
    let mut doc = codec.parse_document(TOML).unwrap();
    edit(
        &mut doc,
        root,
        &["stale"],
        &[(
            "added",
            json!({ "command": "npx", "args": ["-y", "srv"], "env": { "TOKEN": "t" } }),
        )],
    );

    let text = doc.to_text().unwrap();
    assert!(text.contains("# My codex config."), "{text}");
    assert!(text.contains("# Hand-written; leave it alone."), "{text}");
    assert!(!text.contains("stale"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn toml_edits_are_idempotent_on_the_values_not_the_text() {
    let root = RootPath::new(&["mcp_servers"]);
    let codec = codec(Format::Toml, root);
    let entry = json!({ "command": "npx", "args": ["-y", "srv"], "env": { "TOKEN": "t" } });

    let mut doc = codec.parse_document(TOML).unwrap();
    edit(&mut doc, root, &[], &[("added", entry.clone())]);
    let text = doc.to_text().unwrap();

    let again = codec.parse_document(&text).unwrap();
    assert_eq!(again.entries(root)["added"], entry);
    assert_eq!(again.entries(root)["mine"], json!({ "command": "deno" }));
}

#[test]
fn a_map_missing_from_an_empty_document_is_created_per_format() {
    for format in [Format::Json, Format::Jsonc, Format::Toml] {
        let root = RootPath::new(&["mcp_servers"]);
        let codec = codec(format, root);
        let mut doc = codec.empty_document();
        assert!(doc.entries(root).is_empty(), "{format:?}");
        edit(&mut doc, root, &[], &[("added", json!({ "command": "x" }))]);

        let text = doc.to_text().unwrap();
        let reread = codec.parse_document(&text).unwrap();
        assert_eq!(
            reread.entries(root)["added"],
            json!({ "command": "x" }),
            "{format:?} wrote:\n{text}"
        );
    }
}

#[test]
fn nested_root_keys_address_a_map_several_levels_down() {
    let root = RootPath::new(&["tools", "mcp", "servers"]);
    let codec = codec(Format::Json, root);
    let mut doc = codec
        .parse_document(r#"{"tools": {"mcp": {"servers": {"mine": {"command": "deno"}}}}}"#)
        .unwrap();
    assert_eq!(doc.entries(root).len(), 1);

    edit(&mut doc, root, &["mine"], &[("added", json!({"url": "u"}))]);
    let text = doc.to_text().unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["tools"]["mcp"]["servers"]["added"]["url"], "u");
    assert!(value["tools"]["mcp"]["servers"].get("mine").is_none());

    // Missing intermediate levels are created rather than refused.
    let mut fresh = codec.empty_document();
    edit(&mut fresh, root, &[], &[("added", json!({"url": "u"}))]);
    insta::assert_snapshot!(fresh.to_text().unwrap());
}

#[test]
fn a_namespaced_key_is_one_literal_segment_not_a_path() {
    // Amp's shape: the dot belongs to the key, so it must not be read as a
    // nested `amp` object — and the nested shape must not be read as it.
    let literal = RootPath::new(&["amp.mcpServers"]);
    let nested = RootPath::new(&["amp", "mcpServers"]);
    let codec = codec(Format::Json, literal);

    let flat = r#"{"amp.mcpServers": {"mine": {"command": "deno"}}}"#;
    let deep = r#"{"amp": {"mcpServers": {"mine": {"command": "deno"}}}}"#;
    let doc = codec.parse_document(flat).unwrap();
    assert_eq!(doc.entries(literal).len(), 1);
    assert!(doc.entries(nested).is_empty());
    let doc = codec.parse_document(deep).unwrap();
    assert!(doc.entries(literal).is_empty());
    assert_eq!(doc.entries(nested).len(), 1);

    let mut fresh = codec.empty_document();
    edit(&mut fresh, literal, &[], &[("mine", json!({"url": "u"}))]);
    assert_eq!(
        serde_json::from_str::<Value>(&fresh.to_text().unwrap()).unwrap(),
        serde_json::from_str::<Value>(r#"{"amp.mcpServers": {"mine": {"url": "u"}}}"#).unwrap()
    );
}

#[test]
fn an_unparseable_file_is_a_typed_failure_per_format() {
    let root = RootPath::new(&["mcpServers"]);
    // JSONC accepts what strict JSON rejects; that difference is the whole
    // reason the CLI can only skip strict-JSON clients over comments.
    let comments = "// hi\n{\"mcpServers\": {}}\n";
    assert!(codec(Format::Json, root).parse_document(comments).is_err());
    assert!(codec(Format::Jsonc, root).parse_document(comments).is_ok());
    assert!(codec(Format::Toml, root).parse_document(comments).is_err());
    assert!(codec(Format::Jsonc, root).parse_document("{ not ").is_err());
}

#[test]
fn the_shipped_clients_codecs_are_what_they_were() {
    let matrix: Vec<(&str, String)> = ClientKind::ALL
        .into_iter()
        .map(|kind| {
            let codec = kind.codec();
            (
                kind.id(),
                format!(
                    "{:?} / {} / {:?}",
                    codec.format,
                    codec.root.display(),
                    codec.entries
                ),
            )
        })
        .collect();
    insta::assert_debug_snapshot!(matrix);
}
