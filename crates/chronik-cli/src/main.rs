//! `chronik` — the developer CLI for the Chronik Ontology SDK.
//!
//! This is the **authoring** half of the semantic-MCP SDK: declare a domain
//! (object types + link types) in a YAML/JSON schema, apply it to a broker, and
//! ingest facts. Agents then **consume** the domain over the MCP endpoint
//! `POST /ontology/v1/mcp` — no SDK code needed on the consumption side.
//!
//! Kept separate from `chronik-server` (the broker + ops CLI) on purpose: this is
//! a developer tool, not a way to run a server. The actual logic lives in the
//! `chronik-ontology` crate ([`chronik_ontology::schema`] +
//! [`chronik_ontology::apply`]); this binary is a thin CLI over it.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use chronik_ontology::publish_records;
use chronik_ontology::schema::{fact_record, parse_fact_line, parse_schema};

#[derive(Parser, Debug)]
#[command(name = "chronik", version, about = "Chronik developer CLI (Ontology SDK)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Ontology SDK — define a domain (object types + link types) and feed it data
    Ontology {
        #[command(subcommand)]
        action: OntologyAction,
    },
}

#[derive(Subcommand, Debug)]
enum OntologyAction {
    /// Validate a schema file offline (no broker needed) — good for CI
    Validate {
        /// Path to the schema file (e.g. my-domain.ontology.yaml)
        file: PathBuf,
    },

    /// Publish a schema's object types + link types to the broker
    Apply {
        /// Path to the schema file
        file: PathBuf,

        /// Kafka bootstrap servers
        #[arg(long, env = "CHRONIK_KAFKA", default_value = "localhost:9092")]
        brokers: String,

        /// Print the records that would be published, without producing
        #[arg(long)]
        dry_run: bool,
    },

    /// Ingest facts (JSONL: {subject,predicate,object,valid_from?,valid_to?})
    Ingest {
        /// Path to the facts JSONL file
        file: PathBuf,

        /// Namespace to ingest into (its tenant selects the topic)
        #[arg(long, env = "CHRONIK_ONTOLOGY_NAMESPACE")]
        namespace: String,

        /// Kafka bootstrap servers
        #[arg(long, env = "CHRONIK_KAFKA", default_value = "localhost:9092")]
        brokers: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Ontology { action } => run_ontology(action).await,
    }
}

async fn run_ontology(action: OntologyAction) -> Result<()> {
    match action {
        OntologyAction::Validate { file } => {
            let text = std::fs::read_to_string(&file)?;
            let schema = parse_schema(&text).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            schema.validate().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!(
                "OK: schema for namespace '{}' (tenant '{}') is valid — {} object type(s), {} link type(s).",
                schema.namespace,
                schema.tenant(),
                schema.object_types.len(),
                schema.link_types.len()
            );
            Ok(())
        }

        OntologyAction::Apply { file, brokers, dry_run } => {
            let text = std::fs::read_to_string(&file)?;
            let schema = parse_schema(&text).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            schema.validate().map_err(|e| anyhow::anyhow!(e.to_string()))?;

            let mut records = schema.object_type_records();
            records.extend(schema.link_type_records());

            if dry_run {
                println!("DRY RUN — {} record(s) that WOULD be published:", records.len());
                for r in &records {
                    println!("  {} key={} value={}", r.topic, r.key, r.value);
                }
                return Ok(());
            }

            publish_records(&brokers, &records).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!(
                "Applied: published {} object type(s) + {} link type(s) to tenant '{}' via {}.",
                schema.object_types.len(),
                schema.link_types.len(),
                schema.tenant(),
                brokers
            );
            println!("Agents can consume them via the MCP endpoint /ontology/v1/mcp (broker must run with CHRONIK_ONTOLOGY_ENABLED=true).");
            Ok(())
        }

        OntologyAction::Ingest { file, namespace, brokers } => {
            let tenant = namespace.split(':').next().unwrap_or(&namespace).to_string();
            let content = std::fs::read_to_string(&file)?;

            let mut records = Vec::new();
            for (i, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let fact = parse_fact_line(line)
                    .map_err(|e| anyhow::anyhow!("facts line {}: {}", i + 1, e.to_string()))?;
                records.push(fact_record(&namespace, &tenant, &fact, records.len() as i64));
            }
            let n = publish_records(&brokers, &records).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("Ingested {} fact(s) into mem.fact.{} (namespace '{}').", n, tenant, namespace);
            Ok(())
        }
    }
}
