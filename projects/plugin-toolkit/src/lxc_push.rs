use crate::lxc_exec::ORCA_BIN_ENV;

#[derive(Debug, Clone, Default, crate::serde::Serialize, crate::serde::Deserialize)]
#[serde(crate = "crate::serde")]
pub struct LxcPushResult {
    pub ok: bool,
    #[serde(default)]
    pub error: String,
}

pub async fn lxc_push(
    vmid: u32,
    path: &str,
    contents: &[u8],
    mode: Option<&str>,
    owner: Option<&str>,
) -> anyhow::Result<LxcPushResult> {
    let payload = crate::serde_json::json!({
        "vmid": vmid,
        "path": path,
        "contents": contents,
        "mode": mode,
        "owner": owner,
    })
    .to_string();
    if let Some(out) = crate::capsink::privileged_route(crate::abi::LXC_PUSH, &payload) {
        let out = out?;
        return helper_result(out.success, out.code, out.stdout.as_bytes(), &out.stderr);
    }
    let orca_bin = std::env::var(ORCA_BIN_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "writing into CT {vmid} needs orca's lxc-push seam, but {ORCA_BIN_ENV} is unset (not launched by the orca daemon)"
            )
        })?;
    via_orca(&orca_bin, &payload).await
}

async fn via_orca(orca_bin: &str, payload: &str) -> anyhow::Result<LxcPushResult> {
    let out = utils::privileged::run(utils::privileged::PrivilegedCall::new(
        [orca_bin, "admin", "lxc-push"],
        payload,
    ))
    .await
    .map_err(|e| anyhow::anyhow!("spawn `sudo {orca_bin} admin lxc-push`: {e}"))?;
    helper_result(
        out.success,
        out.code,
        &out.stdout,
        &String::from_utf8_lossy(&out.stderr),
    )
}

fn helper_result(
    success: bool,
    code: Option<i32>,
    stdout: &[u8],
    stderr: &str,
) -> anyhow::Result<LxcPushResult> {
    if !success {
        anyhow::bail!(
            "lxc-push privileged helper failed (exit {}): {}. Is the `orca admin lxc-push` \
             sudoers grant installed on this node?",
            code.map_or("signal".into(), |c| c.to_string()),
            stderr.trim()
        );
    }
    crate::serde_json::from_slice::<LxcPushResult>(stdout).map_err(|e| {
        anyhow::anyhow!(
            "parse lxc-push result: {e}: {}",
            String::from_utf8_lossy(stdout).trim()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(crate::serde::Deserialize)]
    #[serde(crate = "crate::serde")]
    struct PushOp {
        vmid: u32,
        path: String,
        contents: Vec<u8>,
        mode: Option<String>,
        owner: Option<String>,
    }

    #[test]
    fn an_advertised_core_runs_lxc_push_over_the_cap() {
        crate::capsink::set_core_privileged(&[crate::abi::LXC_PUSH.to_string()]);
        let sent = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let seen = sent.clone();
        let out = crate::capsink::with_cap_sink(
            Box::new(move |cap: &str, json: &str| {
                *seen.borrow_mut() = format!("{cap} {json}");
                Ok(r#"{"success":true,"code":0,"stdout":"{\"ok\":true,\"error\":\"\"}","stderr":""}"#.into())
            }),
            || {
                crate::reactor::block_on(lxc_push(
                    7,
                    "/etc/x",
                    b"hi",
                    Some("0644"),
                    Some("root:root"),
                ))
            },
        );
        crate::capsink::set_core_privileged(&[]);
        assert!(out.unwrap().ok);
        let sent = sent.borrow();
        let (cap, json) = sent.split_once(' ').unwrap();
        assert_eq!(cap, crate::abi::LXC_PUSH);
        let op: PushOp = crate::serde_json::from_str(json).unwrap();
        assert_eq!(op.vmid, 7);
        assert_eq!(op.path, "/etc/x");
        assert_eq!(op.contents, b"hi");
        assert_eq!(op.mode.as_deref(), Some("0644"));
        assert_eq!(op.owner.as_deref(), Some("root:root"));
    }

    #[test]
    fn a_failed_helper_is_an_error_not_a_result() {
        let err = helper_result(false, Some(1), b"", "sudo: a password is required").unwrap_err();
        assert!(err.to_string().contains("password is required"));
    }

    #[test]
    fn a_refused_push_is_a_result() {
        let r = helper_result(
            true,
            Some(0),
            br#"{"ok":false,"error":"outside allowlist"}"#,
            "",
        )
        .unwrap();
        assert!(!r.ok);
        assert_eq!(r.error, "outside allowlist");
    }
}
