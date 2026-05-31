//! Docker → TopologyClaim collector.
//!
//! Runs on the local host (the docker socket is the API endpoint). Emits
//! one claim per container with MACs from `NetworkSettings.Networks[*]`.
//! Consumed by `system::topology` and surfaced on `SystemInfoReport.claims`.

use contract::TopologyClaim;
use serde_json::Value;

/// Enumerate local containers via the `docker` CLI and build claims.
pub async fn collect_claims() -> anyhow::Result<Vec<TopologyClaim>> {
    let summaries = crate::containers::list(false).await?;
    let mut claims = Vec::with_capacity(summaries.len());
    for s in summaries {
        let inspected = crate::containers::inspect(&s.id).await?;
        let macs = extract_macs_from_inspect(&inspected);
        let id_short = s.id.chars().take(12).collect::<String>();
        let name = first_name(&s.names);
        claims.push(TopologyClaim {
            kind: "container".to_string(),
            id: id_short,
            name,
            macs,
            provider: "docker".to_string(),
            provider_instance: "local".to_string(),
        });
    }
    Ok(claims)
}

/// `docker inspect` returns `[ {...} ]`. Walk
/// `[0].NetworkSettings.Networks.<name>.MacAddress` and collect non-empty
/// MACs (lowercased).
fn extract_macs_from_inspect(v: &Value) -> Vec<String> {
    let Some(obj) = v.as_array().and_then(|a| a.first()) else {
        return Vec::new();
    };
    let Some(networks) = obj
        .get("NetworkSettings")
        .and_then(|n| n.get("Networks"))
        .and_then(|n| n.as_object())
    else {
        return Vec::new();
    };
    let mut macs = Vec::new();
    for (_name, net) in networks {
        if let Some(mac) = net.get("MacAddress").and_then(|m| m.as_str())
            && !mac.is_empty()
        {
            macs.push(mac.to_lowercase());
        }
    }
    macs
}

fn first_name(names: &str) -> String {
    names
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_macs_pulls_per_network_mac() {
        let v = json!([{
            "NetworkSettings": {
                "Networks": {
                    "bridge": {"MacAddress": "02:42:AC:11:00:02"},
                    "frontend": {"MacAddress": "02:42:AC:12:00:03"},
                }
            }
        }]);
        let mut macs = extract_macs_from_inspect(&v);
        macs.sort();
        assert_eq!(macs, vec!["02:42:ac:11:00:02", "02:42:ac:12:00:03"]);
    }

    #[test]
    fn extract_macs_skips_empty_and_missing() {
        let v = json!([{
            "NetworkSettings": {
                "Networks": {
                    "bridge": {"MacAddress": ""},
                    "none": {},
                }
            }
        }]);
        assert!(extract_macs_from_inspect(&v).is_empty());
    }

    #[test]
    fn extract_macs_handles_missing_networksettings() {
        assert!(extract_macs_from_inspect(&json!([{}])).is_empty());
        assert!(extract_macs_from_inspect(&json!([])).is_empty());
        assert!(extract_macs_from_inspect(&json!({})).is_empty());
    }

    #[test]
    fn first_name_strips_leading_slash_and_takes_first() {
        assert_eq!(first_name("/foo,bar"), "foo");
        assert_eq!(first_name("foo"), "foo");
        assert_eq!(first_name(""), "");
    }
}
