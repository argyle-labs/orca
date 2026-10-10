//! Guards against CI or a build path silently dropping `.cargo/config.toml`,
//! which would build the bundled SQLite without our compile-time flags.

#[test]
fn bundled_sqlite_has_configured_compile_flags() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let mut stmt = conn.prepare("PRAGMA compile_options").unwrap();
    let options: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    // `PRAGMA compile_options` reports names without the `SQLITE_` prefix.
    let expected = [
        "DEFAULT_MEMSTATUS=0",
        "DQS=0",
        "THREADSAFE=2",
        "ENABLE_FTS5",
        "ENABLE_STAT4",
        "DEFAULT_WAL_SYNCHRONOUS=1",
    ];
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|flag| !options.iter().any(|o| o == flag))
        .collect();
    assert!(
        missing.is_empty(),
        ".cargo/config.toml LIBSQLITE3_FLAGS not applied: missing {missing:?}; compiled with {options:?}"
    );
}
