//! `orca profile` subcommands — manage per-user profiles + sharing.
//!
//! Profiles are first-class shareable objects (see `project_profile_path.md`).
//! Filesystem content lives at `~/.orca/profiles/<id>/`; metadata, ACL, and
//! credentials live in `orca.db` (encrypted at rest).
//!
//! Multi-user auth is not wired in yet — every command operates as the implicit
//! `LOCAL_USER` per `orca_utils::config::LOCAL_USER`. When real identity lands, replace the
//! `user_id()` helper below.

use crate::profile::{ProfileManager, Role};
use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use colored::Colorize;
use orca_utils::config::{Config, LOCAL_USER};
use rusqlite::Connection;

#[derive(Subcommand)]
pub enum ProfileAction {
    /// List all profiles you can access (owned + shared).
    List,

    /// Show details of a profile (defaults to the active profile).
    Show {
        /// Profile id or name. Omit to show the active profile.
        spec: Option<String>,
    },

    /// Show the currently active profile.
    Current,

    /// Create a new profile owned by the current user.
    Create {
        /// Profile name (must be unique among your owned profiles).
        name: String,
        /// Optional human description.
        #[arg(long)]
        description: Option<String>,
    },

    /// Delete a profile (only the owner can delete).
    Delete {
        /// Profile id or name.
        spec: String,
        /// Skip confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Set the active profile for the current user.
    Use {
        /// Profile id or name.
        spec: String,
    },

    /// Share a profile with another user.
    Share {
        /// Profile id or name.
        spec: String,
        /// User id to share with.
        user: String,
        /// Role to grant: `viewer` (read-only) or `collaborator` (edit).
        role: String,
    },

    /// Remove a share.
    Unshare {
        /// Profile id or name.
        spec: String,
        /// User id to revoke.
        user: String,
    },

    /// List sharees on a profile (owner only).
    Shares {
        /// Profile id or name.
        spec: String,
    },
}

pub fn cmd_profile(config: &Config, action: ProfileAction) -> Result<()> {
    let conn = db::open(&config.db_path).context("open orca.db")?;
    let mgr = ProfileManager::from_config(config);
    let me = user_id();

    match action {
        ProfileAction::List => list(&conn, &mgr, &me),
        ProfileAction::Show { spec } => show(&conn, &mgr, &me, spec.as_deref()),
        ProfileAction::Current => show_current(&conn, &mgr, &me),
        ProfileAction::Create { name, description } => {
            create(&conn, &mgr, &me, &name, description.as_deref())
        }
        ProfileAction::Delete { spec, yes } => delete(&conn, &mgr, &me, &spec, yes),
        ProfileAction::Use { spec } => use_profile(&conn, &mgr, &me, &spec),
        ProfileAction::Share { spec, user, role } => share(&conn, &mgr, &me, &spec, &user, &role),
        ProfileAction::Unshare { spec, user } => unshare(&conn, &mgr, &me, &spec, &user),
        ProfileAction::Shares { spec } => shares(&conn, &mgr, &me, &spec),
    }
}

/// Identity of the requesting user. Single-user installs always return
/// `LOCAL_USER`; replace this when auth-bound CLI sessions exist.
fn user_id() -> String {
    LOCAL_USER.to_string()
}

fn list(conn: &Connection, mgr: &ProfileManager, me: &str) -> Result<()> {
    let profiles = mgr.list_for_user(conn, me)?;
    if profiles.is_empty() {
        println!(
            "{}",
            "(no profiles — run `orca profile create <name>`)".dimmed()
        );
        return Ok(());
    }
    let active = db::profiles::get_active(conn, me).ok().flatten();
    println!("{}", "Profiles:".green());
    for p in profiles {
        let marker = if Some(&p.id) == active.as_ref() {
            "*"
        } else {
            " "
        };
        let owner_tag = if p.owner_user_id == me {
            "owner".cyan()
        } else {
            format!("shared by {}", p.owner_user_id).dimmed()
        };
        println!(
            "  {} {}  {}  {}",
            marker.green(),
            format!("{:<20}", p.name).cyan(),
            owner_tag,
            p.id.dimmed(),
        );
    }
    Ok(())
}

fn show(conn: &Connection, mgr: &ProfileManager, me: &str, spec: Option<&str>) -> Result<()> {
    let profile = match spec {
        Some(s) => mgr
            .resolve_spec(conn, me, s)?
            .ok_or_else(|| anyhow!("profile not found: {s}"))?,
        None => mgr
            .resolve_active(conn, me)?
            .ok_or_else(|| anyhow!("no active profile (run `orca profile use <spec>`)"))?,
    };

    println!("{}  {}", "Name:".green(), profile.name);
    println!("{}    {}", "Id:".green(), profile.id.dimmed());
    println!("{} {}", "Owner:".green(), profile.owner_user_id);
    if let Some(desc) = &profile.description {
        println!("{}  {}", "Desc:".green(), desc);
    }
    println!("{}  {}", "Path:".green(), profile.root.display());
    let access = mgr.access(conn, &profile.id, me)?;
    println!("{}  {:?}", "Access:".green(), access);
    Ok(())
}

fn show_current(conn: &Connection, mgr: &ProfileManager, me: &str) -> Result<()> {
    match mgr.resolve_active(conn, me)? {
        Some(p) => {
            println!("{} {}  {}", "Active:".green(), p.name.cyan(), p.id.dimmed());
            Ok(())
        }
        None => {
            println!("{}", "(no active profile)".dimmed());
            Ok(())
        }
    }
}

fn create(
    conn: &Connection,
    mgr: &ProfileManager,
    me: &str,
    name: &str,
    description: Option<&str>,
) -> Result<()> {
    let p = mgr.create(conn, me, name, description)?;
    println!(
        "{} {}  {}",
        "Created:".green(),
        p.name.cyan(),
        p.id.dimmed()
    );
    println!("{} {}", "Path:".green(), p.root.display());
    Ok(())
}

fn delete(conn: &Connection, mgr: &ProfileManager, me: &str, spec: &str, yes: bool) -> Result<()> {
    let p = mgr
        .resolve_spec(conn, me, spec)?
        .ok_or_else(|| anyhow!("profile not found: {spec}"))?;

    if !yes {
        eprintln!(
            "About to delete profile {} ({}). This removes filesystem content + DB metadata.",
            p.name.cyan(),
            p.id.dimmed()
        );
        eprint!("Continue? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }

    mgr.delete(conn, &p.id, me)?;
    println!("{} {}", "Deleted:".green(), p.name);
    Ok(())
}

fn use_profile(conn: &Connection, mgr: &ProfileManager, me: &str, spec: &str) -> Result<()> {
    let p = mgr
        .resolve_spec(conn, me, spec)?
        .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
    mgr.set_active(conn, me, &p.id)?;
    println!("{} {}  {}", "Active:".green(), p.name.cyan(), p.id.dimmed());
    Ok(())
}

fn share(
    conn: &Connection,
    mgr: &ProfileManager,
    me: &str,
    spec: &str,
    with_user: &str,
    role: &str,
) -> Result<()> {
    let p = mgr
        .resolve_spec(conn, me, spec)?
        .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
    let role_enum = Role::parse(role)
        .ok_or_else(|| anyhow!("invalid role: {role} (expected 'viewer' or 'collaborator')"))?;
    mgr.share(conn, &p.id, me, with_user, role_enum)?;
    println!(
        "{} shared {} ({}) with {} as {}",
        "ok:".green(),
        p.name.cyan(),
        p.id.dimmed(),
        with_user,
        role.cyan()
    );
    Ok(())
}

fn unshare(
    conn: &Connection,
    mgr: &ProfileManager,
    me: &str,
    spec: &str,
    with_user: &str,
) -> Result<()> {
    let p = mgr
        .resolve_spec(conn, me, spec)?
        .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
    let removed = mgr.unshare(conn, &p.id, me, with_user)?;
    if removed {
        println!("{} removed share for {}", "ok:".green(), with_user);
    } else {
        println!("{} no share existed for {}", "noop:".dimmed(), with_user);
    }
    Ok(())
}

fn shares(conn: &Connection, mgr: &ProfileManager, me: &str, spec: &str) -> Result<()> {
    let p = mgr
        .resolve_spec(conn, me, spec)?
        .ok_or_else(|| anyhow!("profile not found: {spec}"))?;
    let shares = mgr.list_shares(conn, &p.id, me)?;
    if shares.is_empty() {
        println!("{}", "(no shares)".dimmed());
        return Ok(());
    }
    println!("{}", "Shares:".green());
    for (user, role) in shares {
        println!(
            "  {}  {}",
            format!("{:<24}", user).cyan(),
            role.as_str().dimmed()
        );
    }
    Ok(())
}
