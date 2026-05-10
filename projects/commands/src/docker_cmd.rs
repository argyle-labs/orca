use anyhow::Result;
use clap::Subcommand;
use db::docker_runtimes::RuntimeRow;

#[derive(Subcommand, Debug)]
pub enum DockerAction {
    /// List all registered Docker runtimes
    List,
    /// Add a Docker runtime to orca.db
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
    /// Remove a Docker runtime from orca.db
    Remove { name: String },
}

pub fn cmd_docker(action: DockerAction) -> Result<()> {
    match action {
        DockerAction::List => {
            let conn = db::open_default()?;
            let rts = db::docker_runtimes::list(&conn)?;
            if rts.is_empty() {
                println!("No Docker runtimes registered. Use `orca docker add` to add one.");
            } else {
                println!("Docker runtimes:");
                for r in &rts {
                    let target = r
                        .docker_host()
                        .or_else(|| r.url.clone())
                        .unwrap_or_else(|| "(no connection)".to_string());
                    let flag = if r.enabled {
                        " [enabled]"
                    } else {
                        " [disabled]"
                    };
                    println!("  {}{} → {}", r.name, flag, target);
                }
            }
            Ok(())
        }
        DockerAction::Add {
            name,
            socket,
            host,
            url,
        } => {
            if socket.is_none() && host.is_none() && url.is_none() {
                anyhow::bail!("provide --socket <path>, --host <url>, or --url <http-url>");
            }
            let row = RuntimeRow {
                name: name.clone(),
                socket_path: socket,
                host,
                url,
                enabled: true,
            };
            let conn = db::open_default()?;
            db::docker_runtimes::upsert(&conn, &row)?;
            println!("added docker runtime '{name}' to orca.db");
            Ok(())
        }
        DockerAction::Remove { name } => {
            let conn = db::open_default()?;
            if db::docker_runtimes::remove(&conn, &name)? {
                println!("removed '{name}'");
            } else {
                println!("'{name}' not found in orca.db");
            }
            Ok(())
        }
    }
}
