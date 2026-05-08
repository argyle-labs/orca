use anyhow::Result;
use clap::Subcommand;
use db::{self, SchemaDbRow};

#[derive(Subcommand, Debug)]
pub enum SchemaAction {
    /// List all registered schema databases
    List,
    /// Add a schema database to orca.db
    Add {
        /// Display name
        name: String,
        #[arg(long)]
        database: String,
        /// Driver: mysql (default), postgres, sqlite
        #[arg(long, default_value = "mysql")]
        driver: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password: Option<String>,
        /// Docker container name (use instead of host/port for local containers)
        #[arg(long)]
        container: Option<String>,
        /// Host for TCP connection
        #[arg(long)]
        host: Option<String>,
        /// Port for TCP connection (default: 3306 mysql, 5432 postgres)
        #[arg(long)]
        port: Option<u16>,
        /// Path to JSON domains file
        #[arg(long)]
        domains_file: Option<String>,
    },
    /// Remove a schema database from orca.db
    Remove { name: String },
}

pub fn cmd_schema(action: SchemaAction) -> Result<()> {
    match action {
        SchemaAction::List => {
            let conn = db::open_default()?;
            let dbs = db::list_schema_databases(&conn)?;
            if dbs.is_empty() {
                println!("No schema databases registered. Use `orca schema add` to add one.");
            } else {
                println!("Schema databases:");
                for d in &dbs {
                    let conn_info = match (&d.driver[..], &d.container, &d.host) {
                        ("sqlite", _, _) => d.database.clone(),
                        (_, Some(c), _) => format!("container:{c}"),
                        (_, None, Some(h)) => {
                            let default_port = if d.driver == "postgres" { 5432 } else { 3306 };
                            format!("{h}:{}", d.port.unwrap_or(default_port))
                        }
                        _ => "unknown".to_string(),
                    };
                    println!(
                        "  {} [{}] → {} @ {}",
                        d.name, d.driver, d.database, conn_info
                    );
                }
            }
            Ok(())
        }
        SchemaAction::Add {
            name,
            database,
            driver,
            user,
            password,
            container,
            host,
            port,
            domains_file,
        } => {
            let row = SchemaDbRow {
                name: name.clone(),
                driver: driver.clone(),
                host,
                port,
                user: user.unwrap_or_default(),
                password: password.unwrap_or_default(),
                database,
                container,
                domains_file,
                enabled: true,
            };
            let conn = db::open_default()?;
            db::upsert_schema_database(&conn, &row)?;
            println!("added schema database '{name}' [{driver}] to orca.db");
            Ok(())
        }
        SchemaAction::Remove { name } => {
            let conn = db::open_default()?;
            if db::remove_schema_database(&conn, &name)? {
                println!("removed '{name}'");
            } else {
                println!("'{name}' not found in orca.db");
            }
            Ok(())
        }
    }
}
