//! Auth domain — credentials, PKI, secrets. Each module contains the tool
//! defs + the service trait its bodies dispatch through.

pub mod auth;
#[cfg(feature = "native")]
pub mod oauth;
pub mod pki;
pub mod secrets;

#[cfg(feature = "native")]
pub mod loopback_token;
#[cfg(feature = "native")]
pub mod password;
#[cfg(feature = "native")]
pub mod throttle;
