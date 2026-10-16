//! In-core behaviour tests for the derive's SHARED-mode `endpoint_resource`.
//!
//! A shared-mode resource (no explicit `table:`) writes provider-tagged rows
//! into the ONE core-migrated `endpoints` table, scopes reads to its provider
//! client-side, and keys Update/Delete by the minted `id`. `managed_mounts`
//! (opt-out) is tested separately in `managed_mounts.rs`.

// A thin, shared-mode resource. `token_id` maps onto `auth_principal`,
// `insecure` onto `insecure`; `base_url` persists in the JSON `config` column.
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
        routes: Default::default(),
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
        testprov::endpoint_db::insert(&mk("frigg", "https://10.0.0.7:8006", "root@pam!orca", true))
            .expect("insert");

        // A DIFFERENT provider with the SAME name coexists as a distinct row.
        otherprov::endpoint_db::insert(&otherprov::EndpointRow {
            name: "frigg".to_string(),
            base_url: "https://elsewhere".to_string(),
            routes: Default::default(),
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
        assert_eq!(ours[0].base_url, "https://10.0.0.7:8006");

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

mod secretprov {
    use plugin_toolkit::endpoint_resource;

    #[endpoint_resource(plugin = "secretprov")]
    pub struct SecretEndpoint {
        pub name: String,
        pub url: String,
        pub username: String,
        pub port: Option<u32>,
        #[secret]
        pub password: Option<String>,
        pub enabled: bool,
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use plugin_toolkit::secrets;

        fn ctx() -> contract::ToolCtx {
            use contract::config::{Config, Model};
            let dir = std::env::temp_dir().join(format!("orca-secretprov-{}", std::process::id()));
            contract::ToolCtx::new(std::sync::Arc::new(Config {
                anthropic_api_key: None,
                lmstudio_url: String::new(),
                ollama_url: String::new(),
                default_model: Model::LMStudio {
                    id: String::new(),
                    url: String::new(),
                },
                app_dir: dir.clone(),
                memory_root: dir.clone(),
                db_path: dir.join("t.db"),
                ports: Default::default(),
            }))
        }

        fn rt() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
        }

        fn create_args(name: &str, password: Option<&str>) -> SecretprovCreateArgs {
            SecretprovCreateArgs {
                name: name.to_string(),
                url: "http://10.0.0.5:8080".to_string(),
                username: "admin".to_string(),
                port: Some(8080),
                password: password.map(str::to_string),
                routes: Vec::new(),
            }
        }

        #[test]
        fn secrets_stored_scoped_and_config_round_trips() {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("secretprov.db");
            db::with_thread_db_path(&path, || {
                drop(db::open_default().expect("open temp db"));
                let ctx = ctx();
                let rt = rt();
                let key = "secretprov.qb1.password";

                let out = rt
                    .block_on(secretprov_create(
                        create_args("qb1", Some("s3cret-one")),
                        &ctx,
                    ))
                    .expect("create");
                assert_eq!(
                    secrets::get(key).expect("get").as_deref(),
                    Some("s3cret-one")
                );
                assert!(out.endpoint.has_password);
                let json = serde_json::to_string(&out).expect("json");
                assert!(!json.contains("s3cret-one"));

                let row = endpoint_db::get("qb1").expect("get").expect("some");
                assert_eq!(row.url, "http://10.0.0.5:8080");
                assert_eq!(row.username, "admin");
                assert_eq!(row.port, Some(8080));
                assert_eq!(row.password, None);

                let upd = rt
                    .block_on(secretprov_update(
                        SecretprovUpdateArgs {
                            name: "qb1".to_string(),
                            password: Some("s3cret-two".to_string()),
                            ..Default::default()
                        },
                        &ctx,
                    ))
                    .expect("update");
                assert_eq!(upd.applied, vec!["password".to_string()]);
                assert!(
                    !serde_json::to_string(&upd)
                        .expect("json")
                        .contains("s3cret-two")
                );
                assert_eq!(
                    secrets::get(key).expect("get").as_deref(),
                    Some("s3cret-two")
                );
                assert_eq!(
                    endpoint_db::get("qb1")
                        .expect("get")
                        .expect("some")
                        .username,
                    "admin"
                );

                rt.block_on(secretprov_delete(
                    SecretprovDeleteArgs {
                        name: "qb1".to_string(),
                    },
                    &ctx,
                ))
                .expect("delete");
                assert!(!secrets::exists(key).expect("exists"));

                for bad in ["", "Qb", "a.b", "-x", "a\nb", &"a".repeat(65)] {
                    assert!(
                        rt.block_on(secretprov_create(create_args(bad, Some("x")), &ctx))
                            .is_err(),
                        "{bad:?} must be rejected"
                    );
                }
                assert!(endpoint_db::list().expect("list").is_empty());
            });
        }
    }
}
