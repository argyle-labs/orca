use anyhow::Result;
use brain_utils::db::{self, SchemaDbRow};
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum SchemaAction {
    /// List all registered schema databases
    List,
    /// Add a schema database to brain.db
    Add {
        /// Display name
        name: String,
        #[arg(long)]
        database: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        password: String,
        /// Docker container name (use instead of host/port for local containers)
        #[arg(long)]
        container: Option<String>,
        /// Host for TCP connection
        #[arg(long)]
        host: Option<String>,
        /// Port for TCP connection (default: 3306)
        #[arg(long)]
        port: Option<u16>,
        /// Path to JSON domains file
        #[arg(long)]
        domains_file: Option<String>,
    },
    /// Remove a schema database from brain.db
    Remove {
        name: String,
    },
}

pub fn cmd_schema(action: SchemaAction) -> Result<()> {
    match action {
        SchemaAction::List => {
            let conn = db::open_default()?;
            let dbs = db::list_schema_databases(&conn)?;
            if dbs.is_empty() {
                println!("No schema databases registered. Use `brain schema add` to add one.");
            } else {
                println!("Schema databases:");
                for d in &dbs {
                    let conn_info = match (&d.container, &d.host) {
                        (Some(c), _) => format!("container:{c}"),
                        (None, Some(h)) => format!("{h}:{}", d.port.unwrap_or(3306)),
                        _ => "unknown".to_string(),
                    };
                    println!("  {} → {} @ {}", d.name, d.database, conn_info);
                }
            }
            Ok(())
        }
        SchemaAction::Add {
            name, database, user, password, container, host, port, domains_file,
        } => {
            let row = SchemaDbRow {
                name: name.clone(),
                host,
                port,
                user,
                password,
                database,
                container,
                domains_file,
                enabled: true,
            };
            let conn = db::open_default()?;
            db::upsert_schema_database(&conn, &row)?;
            println!("added schema database '{name}' to brain.db");
            Ok(())
        }
        SchemaAction::Remove { name } => {
            let conn = db::open_default()?;
            if db::remove_schema_database(&conn, &name)? {
                println!("removed '{name}'");
            } else {
                println!("'{name}' not found in brain.db");
            }
            Ok(())
        }
    }
}
