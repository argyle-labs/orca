use crate::SecretRef;
use derive::orca_async;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Torrent,
    Usenet,
}

impl Protocol {
    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Torrent => "torrent",
            Self::Usenet => "nzb",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexerProtocol {
    Newznab,
    Torznab,
}

impl IndexerProtocol {
    pub fn protocol(self) -> Protocol {
        match self {
            Self::Newznab => Protocol::Usenet,
            Self::Torznab => Protocol::Torrent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AcquisitionError {
    #[error("name is empty")]
    EmptyName,
    #[error("invalid url {0:?}")]
    InvalidUrl(String),
    #[error("drop folder path must be absolute: {0:?}")]
    RelativePath(String),
    #[error("no {0:?} client selected and no default set")]
    NoClient(Protocol),
    #[error("client {0} not registered")]
    UnknownClient(Uuid),
    #[error("client {id} is {actual:?}, expected {expected:?}")]
    ProtocolMismatch {
        id: Uuid,
        expected: Protocol,
        actual: Protocol,
    },
    #[error("payload is empty")]
    EmptyPayload,
    #[error("{0}")]
    Provider(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DownloadClient {
    pub id: Uuid,
    pub name: String,
    pub protocol: Protocol,
}

impl DownloadClient {
    pub fn validate(&self) -> Result<(), AcquisitionError> {
        non_empty(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TorrentPayload {
    Magnet(String),
    File(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NzbPayload(pub Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    Torrent(TorrentPayload),
    Nzb(NzbPayload),
}

impl Payload {
    pub fn protocol(&self) -> Protocol {
        match self {
            Self::Torrent(_) => Protocol::Torrent,
            Self::Nzb(_) => Protocol::Usenet,
        }
    }

    pub fn validate(&self) -> Result<(), AcquisitionError> {
        let empty = match self {
            Self::Torrent(TorrentPayload::Magnet(m)) => !m.starts_with("magnet:?"),
            Self::Torrent(TorrentPayload::File(b)) | Self::Nzb(NzbPayload(b)) => b.is_empty(),
        };
        if empty {
            return Err(AcquisitionError::EmptyPayload);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SubmissionId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionState {
    Queued,
    Downloading,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubmissionStatus {
    pub id: SubmissionId,
    pub state: SubmissionState,
    pub progress_pct: u8,
    #[serde(default)]
    pub message: Option<String>,
}

#[orca_async]
pub trait DownloadProvider: Send + Sync {
    fn client(&self) -> &DownloadClient;
    async fn submit(
        &self,
        payload: Payload,
        category: Option<String>,
        save_path: Option<String>,
    ) -> Result<SubmissionId, AcquisitionError>;
    async fn status(&self, id: &SubmissionId) -> Result<SubmissionStatus, AcquisitionError>;
    async fn test(&self) -> Result<(), AcquisitionError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolDefaults {
    #[serde(default)]
    pub torrent: Option<Uuid>,
    #[serde(default)]
    pub usenet: Option<Uuid>,
}

impl ProtocolDefaults {
    pub fn get(&self, protocol: Protocol) -> Option<Uuid> {
        match protocol {
            Protocol::Torrent => self.torrent,
            Protocol::Usenet => self.usenet,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DropFolder {
    pub protocol: Protocol,
    pub path: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub client_id: Option<Uuid>,
}

impl DropFolder {
    pub fn validate(&self) -> Result<(), AcquisitionError> {
        if !Path::new(&self.path).is_absolute() {
            return Err(AcquisitionError::RelativePath(self.path.clone()));
        }
        Ok(())
    }

    pub fn accepts(&self, file: &Path) -> bool {
        file.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(self.protocol.file_extension()))
    }

    pub fn select<'a>(
        &self,
        clients: &'a [DownloadClient],
        defaults: &ProtocolDefaults,
    ) -> Result<&'a DownloadClient, AcquisitionError> {
        let id = self
            .client_id
            .or_else(|| defaults.get(self.protocol))
            .ok_or(AcquisitionError::NoClient(self.protocol))?;
        let client = clients
            .iter()
            .find(|c| c.id == id)
            .ok_or(AcquisitionError::UnknownClient(id))?;
        if client.protocol != self.protocol {
            return Err(AcquisitionError::ProtocolMismatch {
                id,
                expected: self.protocol,
                actual: client.protocol,
            });
        }
        Ok(client)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Indexer {
    pub id: Uuid,
    pub name: String,
    pub protocol: IndexerProtocol,
    pub url: String,
    #[serde(default)]
    pub api_key: Option<SecretRef>,
    #[serde(default)]
    pub categories: Vec<u32>,
}

impl Indexer {
    pub fn validate(&self) -> Result<(), AcquisitionError> {
        non_empty(&self.name)?;
        match url::Url::parse(&self.url) {
            Ok(u) if matches!(u.scheme(), "http" | "https") => Ok(()),
            _ => Err(AcquisitionError::InvalidUrl(self.url.clone())),
        }
    }
}

fn non_empty(name: &str) -> Result<(), AcquisitionError> {
    if name.trim().is_empty() {
        return Err(AcquisitionError::EmptyName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(protocol: Protocol) -> DownloadClient {
        DownloadClient {
            id: Uuid::now_v7(),
            name: "dl".into(),
            protocol,
        }
    }

    fn folder(protocol: Protocol, client_id: Option<Uuid>) -> DropFolder {
        DropFolder {
            protocol,
            path: "/data/blackhole".into(),
            category: Some("comics".into()),
            client_id,
        }
    }

    #[test]
    fn client_requires_name() {
        let mut c = client(Protocol::Torrent);
        assert_eq!(c.validate(), Ok(()));
        c.name = " ".into();
        assert_eq!(c.validate(), Err(AcquisitionError::EmptyName));
    }

    #[test]
    fn select_prefers_explicit_then_default() {
        let t1 = client(Protocol::Torrent);
        let t2 = client(Protocol::Torrent);
        let clients = vec![t1.clone(), t2.clone()];
        let defaults = ProtocolDefaults {
            torrent: Some(t1.id),
            usenet: None,
        };
        assert_eq!(
            folder(Protocol::Torrent, Some(t2.id)).select(&clients, &defaults),
            Ok(&t2)
        );
        assert_eq!(
            folder(Protocol::Torrent, None).select(&clients, &defaults),
            Ok(&t1)
        );
        assert_eq!(
            folder(Protocol::Usenet, None).select(&clients, &defaults),
            Err(AcquisitionError::NoClient(Protocol::Usenet))
        );
    }

    #[test]
    fn select_rejects_unknown_and_mismatched() {
        let n = client(Protocol::Usenet);
        let clients = vec![n.clone()];
        let ghost = Uuid::now_v7();
        let defaults = ProtocolDefaults::default();
        assert_eq!(
            folder(Protocol::Torrent, Some(ghost)).select(&clients, &defaults),
            Err(AcquisitionError::UnknownClient(ghost))
        );
        assert_eq!(
            folder(Protocol::Torrent, Some(n.id)).select(&clients, &defaults),
            Err(AcquisitionError::ProtocolMismatch {
                id: n.id,
                expected: Protocol::Torrent,
                actual: Protocol::Usenet,
            })
        );
    }

    #[test]
    fn drop_folder_path_and_extension() {
        let mut f = folder(Protocol::Usenet, None);
        assert_eq!(f.validate(), Ok(()));
        assert!(f.accepts(Path::new("/data/blackhole/x.NZB")));
        assert!(!f.accepts(Path::new("/data/blackhole/x.torrent")));
        f.path = "blackhole".into();
        assert!(matches!(
            f.validate(),
            Err(AcquisitionError::RelativePath(_))
        ));
    }

    #[test]
    fn payload_protocol_and_validation() {
        let m = Payload::Torrent(TorrentPayload::Magnet("magnet:?xt=urn:btih:abc".into()));
        assert_eq!(m.protocol(), Protocol::Torrent);
        assert_eq!(m.validate(), Ok(()));
        let n = Payload::Nzb(NzbPayload(vec![]));
        assert_eq!(n.protocol(), Protocol::Usenet);
        assert_eq!(n.validate(), Err(AcquisitionError::EmptyPayload));
        assert_eq!(
            Payload::Torrent(TorrentPayload::Magnet("http://x".into())).validate(),
            Err(AcquisitionError::EmptyPayload)
        );
    }

    #[test]
    fn indexer_validation() {
        let mut i = Indexer {
            id: Uuid::now_v7(),
            name: "idx".into(),
            protocol: IndexerProtocol::Newznab,
            url: "https://10.0.0.6/api".into(),
            api_key: Some(SecretRef("onepassword://k".into())),
            categories: vec![7030],
        };
        assert_eq!(i.validate(), Ok(()));
        assert_eq!(i.protocol.protocol(), Protocol::Usenet);
        i.url = "ftp://10.0.0.6".into();
        assert!(matches!(i.validate(), Err(AcquisitionError::InvalidUrl(_))));
    }

    #[test]
    fn serde_round_trip() {
        let f = folder(Protocol::Torrent, Some(Uuid::now_v7()));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["protocol"], "torrent");
        assert_eq!(serde_json::from_value::<DropFolder>(v).unwrap(), f);
        assert!(schemars::schema_for!(Indexer).get("properties").is_some());
        assert!(schemars::schema_for!(Payload).get("oneOf").is_some());
    }
}
