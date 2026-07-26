//! In-core behaviour tests for the derive's SHARED-mode `endpoint_resource`.
//!
//! A shared-mode resource (no explicit `table:`) writes provider-tagged rows
//! into the ONE core-migrated `endpoints` table, scopes reads to its provider
//! client-side, and keys Update/Delete by the minted `id`. `managed_mounts`
//! (opt-out) is tested separately in `managed_mounts.rs`.

// A thin, shared-mode resource. `token_id` maps onto `auth_principal`,
// `insecure` onto `insecure`; `base_url` is a DROPPED provider-specific field
// (not persisted in the thin shared table — reconstructed via Default on read).
mod testprov {
    use plugin_toolkit::endpoint_resource;

    #[endpoint_resource(plugin = "testprov")]
    pub struct TestEndpoint {
        pub name: String,
        pub base_url: String,
        pub token_id: String,
        pub insecure: bool,
        pub enabled: bool,
    }
}

// Second shared-mode provider, sharing the same `endpoints` table.
mod otherprov {
    use plugin_toolkit::endpoint_resource;

    #[endpoint_resource(plugin = "otherprov")]
    pub struct OtherEndpoint {
        pub name: String,
        pub base_url: String,
        pub enabled: bool,
    }
}

use testprov::EndpointRow;

fn mk(name: &str, url: &str, token: &str, insecure: bool) -> EndpointRow {
    EndpointRow {
        name: name.to_string(),
        base_url: url.to_string(),
        token_id: token.to_string(),
        insecure,
        addresses: Vec::new(),
        enabled: true,
    }
}

#[test]
fn shared_mode_crud_is_provider_scoped_and_id_keyed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shared.db");
    db::with_thread_db_path(&path, || {
        // open_default runs apply_schema, which creates the `endpoints` table.
        let conn = db::open_default().expect("open temp db");
        drop(conn);

        // (a) shared-mode insert lands with provider tag + minted id.
        testprov::endpoint_db::insert(&mk(
            "frigg",
            "https://10.10.10.7:8006",
            "root@pam!orca",
            true,
        ))
        .expect("insert");

        // A DIFFERENT provider with the SAME name coexists as a distinct row.
        otherprov::endpoint_db::insert(&otherprov::EndpointRow {
            name: "frigg".to_string(),
            base_url: "https://elsewhere".to_string(),
            addresses: Vec::new(),
            enabled: true,
        })
        .expect("other insert");

        // (b) list is provider-scoped: each provider sees only its own row.
        let ours = testprov::endpoint_db::list().expect("list");
        assert_eq!(ours.len(), 1, "testprov sees only its own row");
        assert_eq!(ours[0].name, "frigg");
        // auth_principal round-trips back into the token_id field.
        assert_eq!(ours[0].token_id, "root@pam!orca");
        assert!(ours[0].insecure);
        // The dropped provider-specific field is NOT persisted → Default.
        assert_eq!(ours[0].base_url, "");

        let theirs = otherprov::endpoint_db::list().expect("other list");
        assert_eq!(theirs.len(), 1, "otherprov sees only its own row");

        // (c) get matches provider+name.
        assert!(testprov::endpoint_db::get("frigg").expect("get").is_some());
        assert!(
            testprov::endpoint_db::get("nope")
                .expect("get missing")
                .is_none(),
            "unknown name is None"
        );

        // (d) update matches provider+name and keys by id.
        let mut row = testprov::endpoint_db::get("frigg")
            .expect("get")
            .expect("some");
        row.insecure = false;
        assert!(testprov::endpoint_db::update(&row).expect("update"));
        assert!(
            !testprov::endpoint_db::get("frigg")
                .expect("get")
                .expect("some")
                .insecure
        );
        // otherprov's same-named row is untouched by testprov's update.
        assert_eq!(otherprov::endpoint_db::list().expect("other list").len(), 1);

        // (e) remove matches provider+name, keys by id, and is scoped.
        assert!(testprov::endpoint_db::remove("frigg").expect("remove"));
        assert!(testprov::endpoint_db::get("frigg").expect("get").is_none());
        assert_eq!(
            otherprov::endpoint_db::list().expect("other list").len(),
            1,
            "removing testprov's row leaves otherprov's same-named row"
        );
    });
}
