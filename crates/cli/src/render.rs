use mcpgw_core::{Config, Transport};
use owo_colors::OwoColorize as _;

const COLUMNS: usize = 5;
const HEADER: [&str; COLUMNS] = ["NAME", "TYPE", "TARGET", "ON", "TAGS"];

/// Renders the server list as an aligned table, one line per server.
#[must_use]
pub fn server_table(config: &Config, color: bool) -> String {
    let rows: Vec<([String; COLUMNS], bool)> = config
        .servers
        .iter()
        .map(|(name, server)| {
            let (kind, target) = match &server.transport {
                Transport::Stdio { command, args, .. } => {
                    let target = if args.is_empty() {
                        command.clone()
                    } else {
                        format!("{command} {}", args.join(" "))
                    };
                    ("stdio", target)
                }
                // The command, never what it prints: this column is read in
                // a terminal and pasted into issues, and an entry whose
                // credential is minted rather than written down otherwise
                // looks like one with no credential at all.
                Transport::Http {
                    url,
                    headers_command,
                    ..
                } => {
                    let target = if headers_command.is_empty() {
                        url.clone()
                    } else {
                        format!(
                            "{url} (headers {} {})",
                            mcpgw_core::doctor::HEADERS_FROM_COMMAND,
                            headers_command.join(" ")
                        )
                    };
                    ("http", target)
                }
            };
            let cells = [
                name.clone(),
                kind.to_owned(),
                target,
                if server.enabled { "on" } else { "off" }.to_owned(),
                server.tags.join(", "),
            ];
            (cells, server.enabled)
        })
        .collect();

    let mut widths = HEADER.map(str::len);
    for (cells, _) in &rows {
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let header_line = padded_line(&HEADER.map(str::to_owned), &widths);
    if color {
        out.push_str(&header_line.dimmed().to_string());
    } else {
        out.push_str(&header_line);
    }
    out.push('\n');
    for (cells, enabled) in &rows {
        let line = padded_line(cells, &widths);
        if color && !enabled {
            // Dimming the whole row makes disabled servers scannable at a glance.
            out.push_str(&line.dimmed().to_string());
        } else {
            out.push_str(&line);
        }
        out.push('\n');
    }
    out
}

fn padded_line(cells: &[String; COLUMNS], widths: &[usize; COLUMNS]) -> String {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        // Manual padding: format! width counts would be skewed once colored
        // cells contain ANSI escapes.
        let pad = widths[i].saturating_sub(cell.chars().count());
        line.extend(std::iter::repeat_n(' ', pad));
    }
    line.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn sample() -> Config {
        let text = r#"
version = 1

[servers.github]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
tags = ["work"]

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
enabled = false
"#;
        Config::parse(text, Path::new("sample.toml")).unwrap()
    }

    #[test]
    fn plain_table_layout() {
        insta::assert_snapshot!(server_table(&sample(), false));
    }

    /// The command shows because it is not a secret; nothing it would print
    /// is anywhere near this table.
    #[test]
    fn a_headers_command_is_named_in_the_target_column() {
        let config = Config::parse(
            "version = 1\n[servers.corp]\ntype = \"http\"\n\
             url = \"https://mcp.corp.example/mcp\"\n\
             headers_command = \"corp-auth print-mcp-headers\"\n",
            Path::new("sample.toml"),
        )
        .unwrap();
        let table = server_table(&config, false);
        assert!(table.contains("headers from command corp-auth"), "{table}");
    }

    #[test]
    fn colored_output_carries_ansi() {
        let plain = server_table(&sample(), false);
        let colored = server_table(&sample(), true);
        assert!(colored.contains('\u{1b}'));
        assert!(!plain.contains('\u{1b}'));
    }
}
