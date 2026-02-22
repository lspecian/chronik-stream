mod consumer;
mod ingestor;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "chronik-perf", about = "Chronik performance test applications")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the HTTP ingestor (receives HTTP, produces to Kafka)
    Ingestor {
        /// Kafka bootstrap servers
        #[arg(long, env = "KAFKA_BOOTSTRAP_SERVERS", default_value = "localhost:9092")]
        bootstrap_servers: String,

        /// Default topic to produce to
        #[arg(long, env = "KAFKA_TOPIC", default_value = "perf-test")]
        topic: String,

        /// HTTP listen port
        #[arg(long, env = "LISTEN_PORT", default_value = "8080")]
        port: u16,
    },
    /// Start the Kafka consumer with metrics endpoint
    Consumer {
        /// Kafka bootstrap servers
        #[arg(long, env = "KAFKA_BOOTSTRAP_SERVERS", default_value = "localhost:9092")]
        bootstrap_servers: String,

        /// Topic to consume from
        #[arg(long, env = "KAFKA_TOPIC", default_value = "perf-test")]
        topic: String,

        /// Consumer group ID
        #[arg(long, env = "CONSUMER_GROUP", default_value = "perf-consumer")]
        group: String,

        /// HTTP listen port for metrics
        #[arg(long, env = "LISTEN_PORT", default_value = "8081")]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Ingestor {
            bootstrap_servers,
            topic,
            port,
        } => ingestor::run(bootstrap_servers, topic, port).await,
        Commands::Consumer {
            bootstrap_servers,
            topic,
            group,
            port,
        } => consumer::run(bootstrap_servers, topic, group, port).await,
    }
}
