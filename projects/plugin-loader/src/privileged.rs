use anyhow::{Result, anyhow};
use plugin_toolkit::abi::{HelperOutput, LXC_EXEC, LXC_PUSH, PRIVILEGED_APPLY};
use plugin_toolkit::serde_json::{self, Value};

pub const PRIVILEGED_CAPS: &[&str] = &[LXC_EXEC, LXC_PUSH, PRIVILEGED_APPLY];

pub fn is_privileged_cap(cap: &str) -> bool {
    PRIVILEGED_CAPS.contains(&cap)
}

fn helper_for(cap: &str) -> Option<&'static str> {
    match cap {
        LXC_EXEC => Some("lxc-exec"),
        LXC_PUSH => Some("lxc-push"),
        PRIVILEGED_APPLY => Some("plugin-apply"),
        _ => None,
    }
}

pub(crate) fn helper_payload(cap: &str, args: Value, principal: &str) -> Result<Vec<u8>> {
    let Value::Object(mut map) = args else {
        return Err(anyhow!("{cap}: payload must be an object"));
    };
    if cap == PRIVILEGED_APPLY {
        map.insert("plugin".into(), Value::String(principal.to_string()));
    }
    Ok(serde_json::to_vec(&map)?)
}

pub fn handle_privileged(
    cap: &str,
    args: Value,
    principal: &str,
    inv: Option<&spec::surfaces::Invocation>,
) -> Result<Value> {
    let helper = helper_for(cap).ok_or_else(|| anyhow!("unknown capability '{cap}'"))?;
    let inv = inv.ok_or_else(|| anyhow!("{cap} requires an authenticated caller"))?;
    spec::surfaces::authorize_capability(cap, inv)?;
    let payload = helper_payload(cap, args, principal)?;
    let out = crate::capability::run_on_cap_runtime(run_helper(helper, payload))??;
    Ok(serde_json::to_value(out)?)
}

async fn run_helper(helper: &'static str, payload: Vec<u8>) -> Result<HelperOutput> {
    let out = utils::privileged::run(utils::privileged::PrivilegedCall::helper(helper, payload))
        .await
        .map_err(|e| {
            anyhow!(
                "run `sudo -n {} admin {helper}`: {e}",
                utils::path::PRIVILEGED_BIN
            )
        })?;
    Ok(HelperOutput {
        success: out.success,
        code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn helpers_run_through_the_privileged_seam() {
        use utils::privileged::testing::{Recorder, with_exec};
        let rec = Recorder::new(utils::privileged::PrivilegedOutput {
            success: true,
            stdout: b"{}".to_vec(),
            ..Default::default()
        });
        let out = with_exec(rec.clone(), run_helper("lxc-exec", b"op".to_vec()))
            .await
            .unwrap();
        assert!(out.success);
        let calls = rec.calls();
        assert_eq!(
            calls[0].argv,
            [utils::path::PRIVILEGED_BIN, "admin", "lxc-exec"]
        );
        assert_eq!(calls[0].stdin, b"op");
    }

    #[tokio::test]
    async fn helpers_never_reach_real_sudo_in_tests() {
        let err = run_helper("lxc-exec", Vec::new()).await.unwrap_err();
        assert!(err.to_string().contains("refused in test builds"), "{err}");
    }

    fn invocation(role: &str, reaches: &[&str], dry_run: bool) -> spec::surfaces::Invocation {
        let mut ctx = contract::ToolCtx::new(std::sync::Arc::new(
            contract::config::Config::load().unwrap(),
        ));
        ctx.set_verified_caller(Some(contract::CallerIdentity {
            user_id: "u".into(),
            username: "u".into(),
            role: role.into(),
            can_mutate: true,
        }));
        let reaches: Vec<String> = reaches.iter().map(|r| r.to_string()).collect();
        let args = if dry_run {
            json!({ contract::plan::DRY_RUN_FIELD: true })
        } else {
            json!({})
        };
        spec::surfaces::Invocation::from_ctx(&ctx)
            .unwrap()
            .for_plugin_invoke(false, role, &reaches, &args)
    }

    fn refusal(cap: &str, inv: Option<&spec::surfaces::Invocation>) -> String {
        handle_privileged(cap, json!({"vmid": 1, "argv": ["true"]}), "p", inv)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn privileged_caps_are_advertised() {
        for cap in PRIVILEGED_CAPS {
            assert!(crate::capability::CAPABILITIES.contains(cap), "{cap}");
        }
    }

    #[test]
    fn refused_without_a_caller_role_or_declaration() {
        for cap in PRIVILEGED_CAPS {
            assert!(refusal(cap, None).contains("authenticated caller"), "{cap}");
            let user = invocation("user", PRIVILEGED_CAPS, false);
            assert!(
                refusal(cap, Some(&user)).contains("requires role `admin`"),
                "{cap}"
            );
            let undeclared = invocation("admin", &[], false);
            assert!(refusal(cap, Some(&undeclared)).contains("reaches"), "{cap}");
            let planned = invocation("admin", PRIVILEGED_CAPS, true);
            assert!(refusal(cap, Some(&planned)).contains("dry run"), "{cap}");
        }
    }

    #[test]
    fn allowed_for_a_declared_admin() {
        for cap in PRIVILEGED_CAPS {
            let inv = invocation("admin", &[cap], false);
            spec::surfaces::authorize_capability(cap, &inv).unwrap();
            let root = invocation("super-admin", &[cap], false);
            spec::surfaces::authorize_capability(cap, &root).unwrap();
        }
        let only_exec = invocation("admin", &[LXC_EXEC], false);
        assert!(spec::surfaces::authorize_capability(PRIVILEGED_APPLY, &only_exec).is_err());
    }

    #[test]
    fn plugin_apply_is_pinned_to_the_calling_plugin() {
        let body = helper_payload(
            PRIVILEGED_APPLY,
            json!({"plugin": "other", "op": "rebuild"}),
            "unraid",
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, json!({"plugin": "unraid", "op": "rebuild"}));
        let exec = helper_payload(LXC_EXEC, json!({"vmid": 5, "argv": ["true"]}), "p").unwrap();
        let v: Value = serde_json::from_slice(&exec).unwrap();
        assert_eq!(v, json!({"vmid": 5, "argv": ["true"]}));
        assert!(helper_payload(LXC_EXEC, json!([1]), "p").is_err());
    }
}
