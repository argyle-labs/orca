use anyhow::Result;
use brain_utils::db::{self, DockerRuntimeRow};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DockerAction {
    /// List all registered Docker runtimes
    List,
    /// Add a Docker runtime to brain.db
    Add {
        /// Runtime name (e.g. colima, docker-desktop, dockge, portainer)
        name: String,
        /// Unix socket path (e.g. ~/.colima/default/docker.sock)
        #[arg(long)]
        socket: Option<String>,
        /// Full DOCKER_HOST URL for TCP remotes (e.g. tcp://remote:2376)
        #[arg(long)]
        host: Option<String>,
        /// HTTP URL for web-based orchestrators (e.g. https://dockge.internal)
        #[arg(long)]
        url: Option<String>,
    },
    /// Remove a Docker runtime from brain.db
    Remove {
        name: String,
    },
}

pub fn cmd_docker(action: DockerAction) -> Result<()> {
    match action {
        DockerAction::List => {
            let conn = db::open_default()?;
            let rts = db::list_docker_runtimes(&conn)?;
            if rts.is_empty() {
                println!("No Docker runtimes registered. Use `brain docker add` to add one.");
            } else {
                println!("Docker runtimes:");
                for r in &rts {
                    let target = r.docker_host()
                        .or_else(|| r.url.clone())
                        .unwrap_or_else(|| "(no connection)".to_string());
                    let flag = if r.enabled { " [enabled]" } else { " [disabled]" };
                    println!("  {}{} → {}", r.name, flag, target);
                }
            }
            Ok(())
        }
        DockerAction::Add { name, socket, host, url } => {
            if socket.is_none() && host.is_none() && url.is_none() {
                anyhow::bail!("provide --socket <path>, --host <url>, or --url <http-url>");
            }
            let row = DockerRuntimeRow {
                name: name.clone(),
                socket_path: socket,
                host,
                url,
                enabled: true,
            };
            let conn = db::open_default()?;
            db::upsert_docker_runtime(&conn, &row)?;
            println!("added docker runtime '{name}' to brain.db");
            Ok(())
        }
        DockerAction::Remove { name } => {
            let conn = db::open_default()?;
            if db::remove_docker_runtime(&conn, &name)? {
                println!("removed '{name}'");
            } else {
                println!("'{name}' not found in brain.db");
            }
            Ok(())
        }
    }
}
