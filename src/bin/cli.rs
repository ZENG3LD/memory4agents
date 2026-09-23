use clap::{Parser, Subcommand};
use memory4agent::db::reader;
use memory4agent::search::trigram::fuzzy_search;
use rusqlite::Connection;
use std::path::PathBuf;
use std::process;

#[derive(Parser)]
#[command(name = "memory4agent", about = "Code intelligence CLI — reads SQLite index directly")]
struct Cli {
    /// Path to SQLite database
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Workspace ID
    #[arg(long, global = true, default_value = "nemo")]
    workspace: String,

    /// Output as JSON instead of human-readable text
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fuzzy symbol search by name
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// List all symbols defined in a file
    Symbols { file: String },
    /// Who calls a given symbol name
    Callers { symbol_name: String },
    /// File dependencies (what this file imports)
    Deps { file: String },
    /// What files depend on this file (reverse deps)
    Impact { file: String },
    /// Most depended-upon files
    Hotspots {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Crate dependency graph
    Crates,
    /// Which crate owns a symbol
    Resolve { symbol_name: String },
}

fn resolve_db_path(flag: Option<PathBuf>) -> PathBuf {
    if let Some(p) = flag {
        return p;
    }
    // `MEMORY4AGENT_DATA_DIR` is the current name; `DIG2MEMORY_DATA_DIR` is
    // kept as a fallback so a shell/agent config written before this rename
    // still resolves to the same on-disk directory (which itself is not
    // renamed — see plan piece 0.1).
    if let Ok(dir) = std::env::var("MEMORY4AGENT_DATA_DIR") {
        return PathBuf::from(dir).join("index.db");
    }
    if let Ok(dir) = std::env::var("DIG2MEMORY_DATA_DIR") {
        return PathBuf::from(dir).join("index.db");
    }
    PathBuf::from("./data/index.db")
}

fn open_db(path: &PathBuf) -> Connection {
    Connection::open(path).unwrap_or_else(|e| {
        eprintln!("error: cannot open database at {}: {}", path.display(), e);
        process::exit(1);
    })
}

fn run(cli: &Cli, conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let ws = cli.workspace.as_str();

    match &cli.command {
        Command::Search { query, limit } => {
            let hits = fuzzy_search(conn, Some(ws), query, *limit, 0.3)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else {
                for h in &hits {
                    let s = &h.symbol;
                    println!(
                        "{}:{}\t{}\t{}\t(score: {:.2})",
                        s.file_path, s.line, s.kind, s.name, h.score
                    );
                }
            }
        }

        Command::Symbols { file } => {
            let symbols = reader::get_symbols_in_file(conn, ws, file)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&symbols)?);
            } else {
                for s in &symbols {
                    let parent = s
                        .parent_name
                        .as_deref()
                        .map(|p| format!("\t[{p}]"))
                        .unwrap_or_default();
                    println!(
                        "{}:{}\t{}\t{}\t{}{}",
                        s.line, s.col, s.kind, s.visibility, s.name, parent
                    );
                }
            }
        }

        Command::Callers { symbol_name } => {
            let callers = reader::get_callers_of(conn, symbol_name, Some(ws))?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&callers)?);
            } else {
                for s in &callers {
                    println!("{}:{}\t{}\t{}", s.file_path, s.line, s.kind, s.name);
                }
            }
        }

        Command::Deps { file } => {
            let edges = reader::get_deps_of_file(conn, ws, file)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&edges)?);
            } else {
                for e in &edges {
                    println!("{}\t-> {}", e.kind, e.to_file);
                }
            }
        }

        Command::Impact { file } => {
            let dependents = reader::get_reverse_file_deps(conn, ws, file)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&dependents)?);
            } else {
                for path in &dependents {
                    println!("{path}");
                }
            }
        }

        Command::Hotspots { limit } => {
            let spots = reader::get_hotspots(conn, Some(ws), *limit)?;
            if cli.json {
                // Serialize as array of {file, count} objects.
                let json_vals: Vec<serde_json::Value> = spots
                    .iter()
                    .map(|(f, c)| serde_json::json!({"file": f, "count": c}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_vals)?);
            } else {
                for (file, count) in &spots {
                    println!("{count}\t{file}");
                }
            }
        }

        Command::Crates => {
            let nodes = reader::list_crate_nodes(conn, Some(ws))?;
            let edges = reader::list_crate_deps(conn, Some(ws))?;
            if cli.json {
                let out = serde_json::json!({"nodes": nodes, "edges": edges});
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("# crates");
                for n in &nodes {
                    let member = if n.is_workspace_member { " [member]" } else { "" };
                    println!("  {} v{}{}", n.name, n.version, member);
                }
                println!("# edges");
                for e in &edges {
                    println!("  {} -> {}", e.from_crate, e.to_crate);
                }
            }
        }

        Command::Resolve { symbol_name } => {
            let results = reader::resolve_symbol_to_crate(conn, ws, symbol_name)?;
            if cli.json {
                let json_vals: Vec<serde_json::Value> = results
                    .iter()
                    .map(|(s, c)| {
                        serde_json::json!({
                            "symbol": s,
                            "crate": c.name,
                            "file": s.file_path,
                            "line": s.line,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_vals)?);
            } else {
                for (s, c) in &results {
                    println!("{}\t{}:{}\t{}", s.name, s.file_path, s.line, c.name);
                }
            }
        }
    }

    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let db_path = resolve_db_path(cli.db.clone());
    let conn = open_db(&db_path);

    if let Err(e) = run(&cli, &conn) {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
