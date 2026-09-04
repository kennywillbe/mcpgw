//! The install token's file: where it lands, who can read it, and what a
//! rotate does to whatever was there.

use mcpgw_core::gateway_token::GatewayToken;

#[test]
fn the_first_read_mints_a_token_and_every_later_one_returns_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    assert!(GatewayToken::load(&state).unwrap().is_none());

    let (token, minted) = GatewayToken::load_or_create(&state).unwrap();
    assert!(minted);
    assert!(GatewayToken::path(&state).is_file());

    // Every later caller — a second `serve`, a `sync`, the bridge — gets the
    // one already on disk. A second mint would lock out every client entry
    // the first one was written into.
    let (again, minted) = GatewayToken::load_or_create(&state).unwrap();
    assert!(!minted);
    assert_eq!(again.secret(), token.secret());
    assert_eq!(
        GatewayToken::load(&state).unwrap().unwrap().secret(),
        token.secret()
    );
}

#[test]
fn a_rotate_replaces_the_token_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (old, _) = GatewayToken::load_or_create(&state).unwrap();

    let new = GatewayToken::rotate(&state).unwrap();
    assert_ne!(new.secret(), old.secret());
    assert_eq!(
        GatewayToken::load(&state).unwrap().unwrap().secret(),
        new.secret()
    );
    // And the old one is genuinely gone, which is the whole reason every
    // client has to be re-synced afterwards.
    assert!(!new.matches(old.secret()));
}

#[test]
fn a_hand_edited_file_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    mcpgw_core::private::create_dir_all(&state).unwrap();
    // What an editor leaves behind. A token read with the whitespace still on
    // it would match nothing and lock the machine out of its own gateway.
    std::fs::write(GatewayToken::path(&state), "  abc123\n\n").unwrap();
    let token = GatewayToken::load(&state).unwrap().unwrap();
    assert!(token.matches("abc123"));
}

#[cfg(unix)]
#[test]
fn the_token_is_owner_only_under_an_owner_only_dir() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    GatewayToken::load_or_create(&state).unwrap();
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&GatewayToken::path(&state)), 0o600);
    assert_eq!(mode(&state), 0o700);

    // A rotate publishes over the old file by rename, so the mode has to come
    // from the temp file rather than from what it replaced.
    GatewayToken::rotate(&state).unwrap();
    assert_eq!(mode(&GatewayToken::path(&state)), 0o600);
}
