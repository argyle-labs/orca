//! ConfigSource — the git-repo ⇆ orca config-store reconcile domain.
//!
//! A meerkat-style git checkout (`config/<host>/*.toml`) is the declared
//! source of truth; the per-host config-row store (`config_*` tools, backed by
//! `db::config_store`) is the live target. ConfigSource reconciles between the
//! two.
//!
//! Tonight's slice is READ-ONLY: parse the checkout, validate every row against
//! its live noun schema (Draft 2020-12), and produce a dry-run diff. No live
//! mutation, no apply, no PR-writeback — those verbs are stubbed.
//!
//! - [`reconcile`] — pure, daemon-free logic (parse / validate / diff). Unit
//!   tested in isolation.
//! - [`tools`] — the `#[orca_tool]` verbs that wire the pure logic to the live
//!   daemon (config-schema registry, config store, unit catalog).

pub mod reconcile;
pub mod tools;
