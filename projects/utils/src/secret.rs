use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, Serializer};
use zeroize::Zeroize;

#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn parse(s: impl Into<String>) -> Result<Self, String> {
        let s = s.into();
        if is_canonical(&s) {
            Ok(Self(s))
        } else {
            Err(
                "secret_ref must be <scheme>://... or a dotted secret name like proxmox.lab.token"
                    .into(),
            )
        }
    }

    pub fn from_stored(s: impl Into<String>) -> Result<Self, String> {
        let s = s.into();
        if is_canonical(&s) || is_legacy(&s) {
            Ok(Self(s))
        } else {
            Err("stored secret_ref is empty or malformed".into())
        }
    }

    pub fn is_canonical(&self) -> bool {
        is_canonical(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_stored(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

fn is_name_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_canonical(s: &str) -> bool {
    let scheme_ref = s.split_once("://").is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && !rest.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && !rest.chars().any(char::is_whitespace)
            && !rest.split(['/', '\\']).any(|seg| seg == "..")
    });
    scheme_ref || (s.split('.').count() >= 2 && s.split('.').all(is_name_segment))
}

fn is_legacy(s: &str) -> bool {
    is_name_segment(s)
        || s.strip_prefix("secret:")
            .is_some_and(|rest| is_canonical(rest) || is_name_segment(rest))
}

pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn expose_secret(&self) -> &T {
        &self.0
    }

    pub fn serialize_exposed<S: Serializer>(secret: &Self, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
    {
        secret.0.serialize(s)
    }
}

impl<T: Zeroize> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T: Zeroize + Clone> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<'de, T: Zeroize + Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::ops::Deref;

    static_assertions::assert_not_impl_any!(
        Secret<String>: Deref,
        AsRef<str>,
        AsRef<OsStr>,
        AsRef<[u8]>,
        Into<String>,
        Serialize,
        fmt::Display
    );

    #[test]
    fn formatting_redacts() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(s.expose_secret(), "hunter2");
    }

    #[test]
    fn serialize_requires_opt_in() {
        #[derive(Serialize)]
        struct Wire {
            #[serde(serialize_with = "Secret::serialize_exposed")]
            v: Secret<String>,
        }
        let w = Wire {
            v: Secret::new("x".into()),
        };
        assert_eq!(serde_json::to_string(&w).unwrap(), r#"{"v":"x"}"#);
    }

    #[test]
    fn secret_ref_validates() {
        assert!(SecretRef::parse("onepassword://vault/item").is_ok());
        assert!(SecretRef::parse("proxmox.lab.token").is_ok());
        assert!(SecretRef::parse("plain").is_err());
        assert!(SecretRef::parse("secret:a.b").is_err());
        assert_eq!(
            serde_json::to_string(&SecretRef::parse("a.b").unwrap()).unwrap(),
            r#""a.b""#
        );
    }

    #[test]
    fn stored_legacy_refs_still_load() {
        for legacy in ["github_token", "secret:cred-ref", "secret:a.b"] {
            let r: SecretRef = serde_json::from_value(serde_json::json!(legacy)).unwrap();
            assert!(!r.is_canonical());
        }
        assert!(SecretRef::from_stored("a.b").unwrap().is_canonical());
        for bad in ["", " ", "a b", "secret:"] {
            assert!(serde_json::from_value::<SecretRef>(serde_json::json!(bad)).is_err());
        }
    }

    #[test]
    fn traversal_after_scheme_is_rejected() {
        for bad in ["x://../../etc", "file://a/../b", "x://..", "x://a\\..\\b"] {
            assert!(SecretRef::parse(bad).is_err(), "{bad}");
            assert!(SecretRef::from_stored(bad).is_err(), "{bad}");
        }
        assert!(SecretRef::parse("x://a..b/c").is_ok());
    }
}
