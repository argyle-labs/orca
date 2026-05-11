//! Proxmox VE API client. API-token auth only for the MVP — ticket/cookie
//! flow can be added later when a password-based caller appears.
//!
//! Token format follows Proxmox's documented header:
//!   Authorization: PVEAPIToken=USER@REALM!TOKENID=UUID

pub mod tool;

use orca_utils::http::{Client as HttpClient, HttpError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    /// `user@realm!tokenid` — the part before the `=` in the auth header.
    pub token_id: String,
    /// The UUID secret — the part after the `=`.
    pub token_secret: String,
    /// Skip TLS verification (homelab self-signed certs).
    pub insecure: bool,
}

impl Config {
    pub fn new(
        base_url: impl Into<String>,
        token_id: impl Into<String>,
        token_secret: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            token_id: token_id.into(),
            token_secret: token_secret.into(),
            insecure: false,
        }
    }

    pub fn insecure(mut self, on: bool) -> Self {
        self.insecure = on;
        self
    }

    fn auth_header(&self) -> String {
        format!("PVEAPIToken={}={}", self.token_id, self.token_secret)
    }
}

#[derive(Debug, Error)]
pub enum ProxmoxError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("missing required field: {0}")]
    Missing(&'static str),
    #[error("unsupported action '{0}' (expected start | stop | shutdown | reboot)")]
    BadAction(String),
    #[error("unsupported guest kind '{0}' (expected qemu | lxc)")]
    BadGuestKind(String),
}

/// Allowed lifecycle actions on VMs and containers. Constraining the set up
/// front keeps callers from forwarding arbitrary strings to the Proxmox API.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    Start,
    Stop,
    Shutdown,
    Reboot,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Shutdown => "shutdown",
            Action::Reboot => "reboot",
        }
    }
}

/// Guest kind — used by snapshot ops that work for both VMs and containers.
#[derive(Debug, Clone, Copy)]
pub enum GuestKind {
    Qemu,
    Lxc,
}

impl GuestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GuestKind::Qemu => "qemu",
            GuestKind::Lxc => "lxc",
        }
    }
}

impl std::str::FromStr for GuestKind {
    type Err = ProxmoxError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "qemu" | "vm" => Ok(GuestKind::Qemu),
            "lxc" | "container" => Ok(GuestKind::Lxc),
            other => Err(ProxmoxError::BadGuestKind(other.to_string())),
        }
    }
}

impl std::str::FromStr for Action {
    type Err = ProxmoxError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(Action::Start),
            "stop" => Ok(Action::Stop),
            "shutdown" => Ok(Action::Shutdown),
            "reboot" => Ok(Action::Reboot),
            other => Err(ProxmoxError::BadAction(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub node: String,
    pub vmid: u64,
    pub action: String,
    /// Proxmox returns a UPID (Unique Process ID) for async tasks.
    pub upid: Option<String>,
    pub status: u16,
}

#[derive(Clone)]
pub struct Client {
    cfg: Config,
    http: HttpClient,
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            http: HttpClient::new(),
        }
    }

    pub fn with_http(cfg: Config, http: HttpClient) -> Self {
        Self { cfg, http }
    }

    /// `GET /api2/json/nodes` — cluster node list.
    pub async fn nodes(&self) -> Result<Value, ProxmoxError> {
        self.get("/api2/json/nodes").await
    }

    /// `GET /api2/json/nodes/{node}/qemu` — VMs on a node.
    pub async fn vms(&self, node: &str) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        self.get(&format!(
            "/api2/json/nodes/{}/qemu",
            urlencoding::encode(node)
        ))
        .await
    }

    /// `GET /api2/json/nodes/{node}/lxc` — containers on a node.
    pub async fn containers(&self, node: &str) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        self.get(&format!(
            "/api2/json/nodes/{}/lxc",
            urlencoding::encode(node)
        ))
        .await
    }

    /// `GET /api2/json/nodes/{node}/status` — CPU/mem/storage for one node.
    pub async fn node_status(&self, node: &str) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        self.get(&format!(
            "/api2/json/nodes/{}/status",
            urlencoding::encode(node)
        ))
        .await
    }

    /// `GET /api2/json/nodes/{node}/qemu/{vmid}/status/current` — VM resource usage.
    pub async fn vm_status(&self, node: &str, vmid: u64) -> Result<Value, ProxmoxError> {
        self.guest_status(node, vmid, GuestKind::Qemu).await
    }

    /// `GET /api2/json/nodes/{node}/lxc/{vmid}/status/current` — container resource usage.
    pub async fn container_status(&self, node: &str, vmid: u64) -> Result<Value, ProxmoxError> {
        self.guest_status(node, vmid, GuestKind::Lxc).await
    }

    /// `GET /api2/json/nodes/{node}/{kind}/{vmid}/status/current` — resource usage for any guest.
    pub async fn guest_status(
        &self,
        node: &str,
        vmid: u64,
        kind: GuestKind,
    ) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        self.get(&format!(
            "/api2/json/nodes/{}/{}/{}/status/current",
            urlencoding::encode(node),
            kind.as_str(),
            vmid
        ))
        .await
    }

    /// `GET /api2/json/cluster/backup` — configured cluster backup jobs.
    pub async fn backup_list(&self) -> Result<Value, ProxmoxError> {
        self.get("/api2/json/cluster/backup").await
    }

    /// `GET /api2/json/nodes/{node}/{kind}/{vmid}/snapshot` — list snapshots.
    pub async fn snapshot_list(
        &self,
        node: &str,
        vmid: u64,
        kind: GuestKind,
    ) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        self.get(&format!(
            "/api2/json/nodes/{}/{}/{}/snapshot",
            urlencoding::encode(node),
            kind.as_str(),
            vmid
        ))
        .await
    }

    /// `POST /api2/json/nodes/{node}/{kind}/{vmid}/snapshot` — create snapshot.
    pub async fn snapshot_create(
        &self,
        node: &str,
        vmid: u64,
        kind: GuestKind,
        name: &str,
    ) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        if name.is_empty() {
            return Err(ProxmoxError::Missing("snapname"));
        }
        self.post_json(
            &format!(
                "/api2/json/nodes/{}/{}/{}/snapshot",
                urlencoding::encode(node),
                kind.as_str(),
                vmid
            ),
            serde_json::json!({ "snapname": name }),
        )
        .await
    }

    /// `POST /api2/json/nodes/{node}/lxc/{vmid}/exec` — run a shell command in
    /// an LXC container. Wraps `pct exec` server-side.
    pub async fn lxc_exec(&self, node: &str, vmid: u64, cmd: &str) -> Result<Value, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        if cmd.is_empty() {
            return Err(ProxmoxError::Missing("cmd"));
        }
        self.post_json(
            &format!(
                "/api2/json/nodes/{}/lxc/{}/exec",
                urlencoding::encode(node),
                vmid
            ),
            serde_json::json!({ "command": ["/bin/sh", "-c", cmd] }),
        )
        .await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, ProxmoxError> {
        let resp = self
            .http
            .post(self.url(path))
            .header("Authorization", self.cfg.auth_header())
            .insecure(self.cfg.insecure)
            .json(body)
            .send()
            .await?;
        Ok(resp
            .json::<Value>()
            .unwrap_or_else(|_| serde_json::json!({ "raw": resp.text() })))
    }

    /// `POST /api2/json/nodes/{node}/qemu/{vmid}/status/{action}`
    pub async fn vm_action(
        &self,
        node: &str,
        vmid: u64,
        action: Action,
    ) -> Result<ActionResult, ProxmoxError> {
        self.lifecycle("qemu", node, vmid, action).await
    }

    /// `POST /api2/json/nodes/{node}/lxc/{vmid}/status/{action}`
    pub async fn container_action(
        &self,
        node: &str,
        vmid: u64,
        action: Action,
    ) -> Result<ActionResult, ProxmoxError> {
        self.lifecycle("lxc", node, vmid, action).await
    }

    async fn lifecycle(
        &self,
        kind: &'static str,
        node: &str,
        vmid: u64,
        action: Action,
    ) -> Result<ActionResult, ProxmoxError> {
        if node.is_empty() {
            return Err(ProxmoxError::Missing("node"));
        }
        let path = format!(
            "/api2/json/nodes/{}/{}/{}/status/{}",
            urlencoding::encode(node),
            kind,
            vmid,
            action.as_str()
        );
        let resp = self
            .http
            .post(self.url(&path))
            .header("Authorization", self.cfg.auth_header())
            .insecure(self.cfg.insecure)
            .send()
            .await?;
        let upid = resp
            .json::<Value>()
            .ok()
            .and_then(|v| v.get("data").and_then(|d| d.as_str().map(str::to_string)));
        Ok(ActionResult {
            node: node.to_string(),
            vmid,
            action: action.as_str().to_string(),
            upid,
            status: resp.status,
        })
    }

    async fn get(&self, path: &str) -> Result<Value, ProxmoxError> {
        let resp = self
            .http
            .get(self.url(path))
            .header("Authorization", self.cfg.auth_header())
            .insecure(self.cfg.insecure)
            .send()
            .await?;
        Ok(resp
            .json::<Value>()
            .unwrap_or_else(|_| serde_json::json!({ "raw": resp.text() })))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(uri: String) -> Config {
        Config::new(uri, "user@pve!auto", "deadbeef-1111-2222-3333-444444444444")
    }

    #[tokio::test]
    async fn nodes_sends_pve_token_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api2/json/nodes"))
            .and(header(
                "authorization",
                "PVEAPIToken=user@pve!auto=deadbeef-1111-2222-3333-444444444444",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": [{"node": "pve1"}]})),
            )
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri())).nodes().await.unwrap();
        assert_eq!(v["data"][0]["node"], "pve1");
    }

    #[tokio::test]
    async fn vms_lists_for_node() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api2/json/nodes/pve1/qemu"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data": [{"vmid": 100}]})),
            )
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri())).vms("pve1").await.unwrap();
        assert_eq!(v["data"][0]["vmid"], 100);
    }

    #[tokio::test]
    async fn vm_action_returns_upid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api2/json/nodes/pve1/qemu/100/status/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": "UPID:pve1:00001234:0001ABCD:65000000:qmstart:100:user@pve!auto:"
            })))
            .mount(&server)
            .await;
        let r = Client::new(cfg(server.uri()))
            .vm_action("pve1", 100, Action::Start)
            .await
            .unwrap();
        assert_eq!(r.action, "start");
        assert_eq!(r.vmid, 100);
        assert!(r.upid.unwrap().starts_with("UPID:pve1:"));
    }

    #[tokio::test]
    async fn container_action_hits_lxc_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api2/json/nodes/pve1/lxc/200/status/stop"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Client::new(cfg(server.uri()))
            .container_action("pve1", 200, Action::Stop)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn empty_node_rejected_before_call() {
        let err = Client::new(cfg("http://nope".to_string()))
            .vms("")
            .await
            .unwrap_err();
        assert!(matches!(err, ProxmoxError::Missing("node")));
    }

    #[tokio::test]
    async fn node_status_hits_correct_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api2/json/nodes/pve1/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{"cpu":0.1}})),
            )
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri()))
            .node_status("pve1")
            .await
            .unwrap();
        assert_eq!(v["data"]["cpu"], 0.1);
    }

    #[tokio::test]
    async fn vm_status_uses_current_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api2/json/nodes/pve1/qemu/100/status/current"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":{"status":"running"}})),
            )
            .mount(&server)
            .await;
        let v = Client::new(cfg(server.uri()))
            .vm_status("pve1", 100)
            .await
            .unwrap();
        assert_eq!(v["data"]["status"], "running");
    }

    #[tokio::test]
    async fn backup_list_hits_cluster_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api2/json/cluster/backup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&server)
            .await;
        Client::new(cfg(server.uri())).backup_list().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_list_resolves_kind() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api2/json/nodes/pve1/lxc/200/snapshot"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": []})))
            .mount(&server)
            .await;
        Client::new(cfg(server.uri()))
            .snapshot_list("pve1", 200, GuestKind::Lxc)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_create_posts_snapname() {
        use wiremock::matchers::body_json;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api2/json/nodes/pve1/qemu/100/snapshot"))
            .and(body_json(serde_json::json!({"snapname":"pre-upgrade"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":"UPID:..."})),
            )
            .mount(&server)
            .await;
        Client::new(cfg(server.uri()))
            .snapshot_create("pve1", 100, GuestKind::Qemu, "pre-upgrade")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn lxc_exec_wraps_command() {
        use wiremock::matchers::body_json;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api2/json/nodes/pve1/lxc/200/exec"))
            .and(body_json(
                serde_json::json!({"command":["/bin/sh","-c","uptime"]}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":"ok"})),
            )
            .mount(&server)
            .await;
        Client::new(cfg(server.uri()))
            .lxc_exec("pve1", 200, "uptime")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn lxc_exec_rejects_empty_cmd() {
        let err = Client::new(cfg("http://nope".to_string()))
            .lxc_exec("pve1", 200, "")
            .await
            .unwrap_err();
        assert!(matches!(err, ProxmoxError::Missing("cmd")));
    }

    #[test]
    fn guest_kind_parses() {
        assert!(matches!(
            "qemu".parse::<GuestKind>().unwrap(),
            GuestKind::Qemu
        ));
        assert!(matches!(
            "vm".parse::<GuestKind>().unwrap(),
            GuestKind::Qemu
        ));
        assert!(matches!(
            "lxc".parse::<GuestKind>().unwrap(),
            GuestKind::Lxc
        ));
        assert!("foo".parse::<GuestKind>().is_err());
    }

    #[test]
    fn action_parses_known_strings() {
        assert!(matches!("start".parse::<Action>().unwrap(), Action::Start));
        assert!(matches!(
            "shutdown".parse::<Action>().unwrap(),
            Action::Shutdown
        ));
        assert!("foo".parse::<Action>().is_err());
    }
}
