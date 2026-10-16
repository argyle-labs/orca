use crate::SecretRef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Torrent,
    Usenet,
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
    #[error("{0} is empty")]
    Empty(&'static str),
    #[error("invalid url {0:?}")]
    InvalidUrl(String),
    #[error("{0:?} requires a secret")]
    MissingSecret(Protocol),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientConnection {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub secret: Option<SecretRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DownloadClient {
    pub id: Uuid,
    pub name: String,
    pub protocol: Protocol,
    /// Opaque id the provider plugin declares for itself.
    pub provider: String,
    pub connection: ClientConnection,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub save_path: Option<String>,
}

impl DownloadClient {
    pub fn validate(&self) -> Result<(), AcquisitionError> {
        non_empty("name", &self.name)?;
        non_empty("provider", &self.provider)?;
        check_url(&self.connection.url)?;
        if self.protocol == Protocol::Usenet && self.connection.secret.is_none() {
            return Err(AcquisitionError::MissingSecret(self.protocol));
        }
        Ok(())
    }

    pub fn resolve(self, secret: Option<String>) -> ResolvedDownloadClient {
        ResolvedDownloadClient {
            id: self.id,
            name: self.name,
            protocol: self.protocol,
            provider: self.provider,
            connection: ResolvedConnection {
                url: self.connection.url,
                username: self.connection.username,
                secret,
            },
            category: self.category,
            save_path: self.save_path,
        }
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
    /// Opaque id the provider plugin declares for itself.
    pub provider: String,
}

impl Indexer {
    pub fn validate(&self) -> Result<(), AcquisitionError> {
        non_empty("name", &self.name)?;
        non_empty("provider", &self.provider)?;
        check_url(&self.url)?;
        if self.protocol == IndexerProtocol::Newznab && self.api_key.is_none() {
            return Err(AcquisitionError::MissingSecret(self.protocol.protocol()));
        }
        Ok(())
    }

    pub fn resolve(self, api_key: Option<String>) -> ResolvedIndexer {
        ResolvedIndexer {
            id: self.id,
            name: self.name,
            protocol: self.protocol,
            url: self.url,
            api_key,
            categories: self.categories,
            provider: self.provider,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedConnection {
    pub url: String,
    pub username: Option<String>,
    pub secret: Option<String>,
}

impl fmt::Debug for ResolvedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedConnection")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("secret", &redact(&self.secret))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedDownloadClient {
    pub id: Uuid,
    pub name: String,
    pub protocol: Protocol,
    pub provider: String,
    pub connection: ResolvedConnection,
    pub category: Option<String>,
    pub save_path: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedIndexer {
    pub id: Uuid,
    pub name: String,
    pub protocol: IndexerProtocol,
    pub url: String,
    pub api_key: Option<String>,
    pub categories: Vec<u32>,
    pub provider: String,
}

impl fmt::Debug for ResolvedIndexer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedIndexer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("protocol", &self.protocol)
            .field("url", &self.url)
            .field("api_key", &redact(&self.api_key))
            .field("categories", &self.categories)
            .field("provider", &self.provider)
            .finish()
    }
}

fn redact(secret: &Option<String>) -> Option<&'static str> {
    secret.as_ref().map(|_| "<redacted>")
}

fn non_empty(field: &'static str, value: &str) -> Result<(), AcquisitionError> {
    if value.trim().is_empty() {
        return Err(AcquisitionError::Empty(field));
    }
    Ok(())
}

fn check_url(url: &str) -> Result<(), AcquisitionError> {
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => Ok(()),
        _ => Err(AcquisitionError::InvalidUrl(url.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(protocol: Protocol, secret: bool) -> DownloadClient {
        DownloadClient {
            id: Uuid::now_v7(),
            name: "dl".into(),
            protocol,
            provider: "some-plugin".into(),
            connection: ClientConnection {
                url: "http://10.0.0.5:8080".into(),
                username: Some("orca".into()),
                secret: secret.then(|| SecretRef("onepassword://x".into())),
            },
            category: Some("comics".into()),
            save_path: None,
        }
    }

    fn indexer(protocol: IndexerProtocol, api_key: bool) -> Indexer {
        Indexer {
            id: Uuid::now_v7(),
            name: "idx".into(),
            protocol,
            url: "https://10.0.0.6/api".into(),
            api_key: api_key.then(|| SecretRef("onepassword://k".into())),
            categories: vec![7030],
            provider: "some-plugin".into(),
        }
    }

    #[test]
    fn torrent_client_secret_optional() {
        assert_eq!(client(Protocol::Torrent, false).validate(), Ok(()));
    }

    #[test]
    fn usenet_client_requires_secret() {
        assert_eq!(
            client(Protocol::Usenet, false).validate(),
            Err(AcquisitionError::MissingSecret(Protocol::Usenet))
        );
        assert_eq!(client(Protocol::Usenet, true).validate(), Ok(()));
    }

    #[test]
    fn client_rejects_bad_fields() {
        let mut c = client(Protocol::Torrent, false);
        c.connection.url = "not a url".into();
        assert!(matches!(c.validate(), Err(AcquisitionError::InvalidUrl(_))));
        c.connection.url = "ftp://10.0.0.5".into();
        assert!(matches!(c.validate(), Err(AcquisitionError::InvalidUrl(_))));
        c.connection.url = "http://10.0.0.5".into();
        c.provider = "".into();
        assert_eq!(c.validate(), Err(AcquisitionError::Empty("provider")));
        c.name = " ".into();
        assert_eq!(c.validate(), Err(AcquisitionError::Empty("name")));
    }

    #[test]
    fn indexer_validation() {
        assert_eq!(indexer(IndexerProtocol::Torznab, false).validate(), Ok(()));
        assert_eq!(
            indexer(IndexerProtocol::Newznab, false).validate(),
            Err(AcquisitionError::MissingSecret(Protocol::Usenet))
        );
        let mut i = indexer(IndexerProtocol::Newznab, true);
        assert_eq!(i.validate(), Ok(()));
        i.url = "nope".into();
        assert!(matches!(i.validate(), Err(AcquisitionError::InvalidUrl(_))));
    }

    #[test]
    fn resolved_debug_redacts_secrets() {
        let c = client(Protocol::Usenet, true).resolve(Some("hunter2".into()));
        assert_eq!(c.connection.secret.as_deref(), Some("hunter2"));
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("hunter2") && dbg.contains("<redacted>"));
        let i = indexer(IndexerProtocol::Newznab, true).resolve(Some("k3y".into()));
        let dbg = format!("{i:?}");
        assert!(!dbg.contains("k3y") && dbg.contains("<redacted>"));
    }

    #[test]
    fn serde_round_trip() {
        let c = client(Protocol::Torrent, true);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["protocol"], "torrent");
        assert_eq!(serde_json::from_value::<DownloadClient>(v).unwrap(), c);
        assert!(schemars::schema_for!(Indexer).get("properties").is_some());
    }
}
