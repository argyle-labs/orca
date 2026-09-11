//! `hosts` — the reachable / capable / healthy view of any addressable endpoint.
//!
//! A "host" is not just a machine: it is any addressable endpoint orca tracks —
//! a machine, a service (docker/vm/lxc), or an exposed API. This crate owns the
//! CRUD for a host's capabilities and status. The CREATE TABLE schema stays in
//! `db` (host_status schema in db::metrics).
//!
//! NOTE: host *addressing* (db::host_addressing) is still in `db` for now — it
//! is entangled with the pod peer-address storage (db::pod) and moves here with
//! the pod->systems dissolution.
pub mod host_capabilities;
pub mod host_status;
