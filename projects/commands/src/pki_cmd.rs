//! `orca pki` subcommand — manage the orca CA and plugin certificates.

use anyhow::Result;
use clap::Subcommand;
use config::{APP_PKI_DIR, APP_STATE_DIR};
use orca_sdk::pki::{self, Capability};

fn default_pki_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(APP_STATE_DIR)
        .join(APP_PKI_DIR)
}

#[derive(Subcommand)]
pub enum PkiAction {
    /// Initialize the orca CA and server cert (safe to re-run; skips if CA exists)
    Init,

    /// Issue a cert for a plugin
    Issue {
        /// Plugin ID (must match the plugin's declared id)
        plugin_id: String,
        /// Capability class: general (default) or sensitive
        #[arg(long, default_value = "general")]
        capability: String,
    },

    /// List all issued plugin certs
    List,
}

pub fn cmd_pki(action: PkiAction) -> Result<()> {
    let pki_dir = default_pki_dir();

    match action {
        PkiAction::Init => {
            pki::init(&pki_dir)?;
            println!("PKI initialized at {}", pki_dir.display());
            println!("  CA cert:     {}", pki::ca_cert_path(&pki_dir).display());
            println!(
                "  Server cert: {}",
                pki::server_cert_path(&pki_dir).display()
            );
        }

        PkiAction::Issue {
            plugin_id,
            capability,
        } => {
            let cap: Capability = capability.parse()?;
            let bundle = pki::issue(&pki_dir, &plugin_id, cap)?;
            println!("issued cert for '{plugin_id}' (capability: {cap})");
            println!(
                "  cert: {}",
                pki::plugin_cert_path(&pki_dir, &plugin_id).display()
            );
            println!(
                "  key:  {}",
                pki::plugin_key_path(&pki_dir, &plugin_id).display()
            );
            // Print the bundle so it can be piped to the plugin's own PKI dir.
            println!("\n# CA cert (add to plugin's trust store)");
            println!("{}", bundle.ca_cert_pem);
            println!("# Plugin cert");
            println!("{}", bundle.cert_pem);
        }

        PkiAction::List => {
            let ids = pki::list_plugins(&pki_dir);
            if ids.is_empty() {
                println!("no plugin certs issued — use `orca pki issue <plugin-id>`");
            } else {
                println!("{:<30} CERT PATH", "PLUGIN ID");
                println!("{}", "-".repeat(70));
                for id in &ids {
                    println!(
                        "{:<30} {}",
                        id,
                        pki::plugin_cert_path(&pki_dir, id).display()
                    );
                }
            }
        }
    }

    Ok(())
}
