use clap::{Parser, Subcommand};
use kube::CustomResourceExt;

use chronik_operator::controllers::{
    autoscaler_controller, cluster_controller, standalone_controller, topic_controller,
    user_controller,
};
use chronik_operator::crds;
use chronik_operator::{leader_election, metrics, telemetry};

#[derive(Parser)]
#[command(name = "chronik-operator")]
#[command(about = "Kubernetes operator for Chronik Stream")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the operator and begin reconciling resources.
    Start {
        /// Log level: trace, debug, info, warn, error.
        #[arg(long, default_value = "info", env = "RUST_LOG")]
        log_level: String,

        /// Metrics server bind address.
        #[arg(long, default_value = "0.0.0.0:8080")]
        metrics_addr: String,

        /// Enable leader election for HA.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        leader_election: bool,
    },

    /// Generate CRD YAML to stdout (for kubectl apply -f -).
    CrdGen,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start {
            log_level,
            metrics_addr,
            leader_election: le_enabled,
        } => {
            telemetry::init(&log_level);
            tracing::info!("Starting chronik-operator");

            let client = kube::Client::try_default().await?;
            let version = client.apiserver_version().await?;
            tracing::info!(
                "Connected to Kubernetes {}.{}",
                version.major,
                version.minor
            );

            // Start metrics / health server
            tokio::spawn(metrics::serve(metrics_addr));

            // Leader election
            let leader_status = leader_election::LeaderStatus::new();

            if le_enabled {
                let config = leader_election::LeaderElectionConfig::default();
                tracing::info!(
                    holder = %config.holder_id,
                    lease = %config.lease_name,
                    "Starting leader election"
                );

                let le_status = leader_status.clone();
                let le_client = client.clone();
                tokio::spawn(async move {
                    leader_election::run(le_client, config, le_status).await;
                });

                // Wait until we become the leader before starting controllers.
                tracing::info!("Waiting for leadership...");
                while !leader_status.is_leader() {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                tracing::info!("Elected as leader, starting controllers");
            } else {
                tracing::info!("Leader election disabled, running as leader");
                leader_status.force_leader();
            }

            metrics::set_leader(true);

            // Start controllers
            tracing::info!("Starting standalone controller");
            let standalone_handle = tokio::spawn(standalone_controller::run(client.clone()));

            tracing::info!("Starting cluster controller");
            let cluster_handle = tokio::spawn(cluster_controller::run(client.clone()));

            tracing::info!("Starting topic controller");
            let topic_handle = tokio::spawn(topic_controller::run(client.clone()));

            tracing::info!("Starting user controller");
            let user_handle = tokio::spawn(user_controller::run(client.clone()));

            tracing::info!("Starting autoscaler controller");
            let autoscaler_handle = tokio::spawn(autoscaler_controller::run(client.clone()));

            // Monitor leadership in background (exit if lost).
            let le_status_monitor = leader_status.clone();
            let leadership_lost = async move {
                if !le_enabled {
                    // If LE disabled, never trigger.
                    std::future::pending::<()>().await;
                    return;
                }
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if !le_status_monitor.is_leader() {
                        return;
                    }
                }
            };

            // Wait for Ctrl-C, leadership loss, or controller exit
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutting down");
                }
                _ = leadership_lost => {
                    tracing::error!("Lost leadership, shutting down for restart");
                    metrics::set_leader(false);
                }
                res = standalone_handle => {
                    match res {
                        Ok(()) => tracing::info!("Standalone controller exited"),
                        Err(e) => tracing::error!("Standalone controller panicked: {e}"),
                    }
                }
                res = cluster_handle => {
                    match res {
                        Ok(()) => tracing::info!("Cluster controller exited"),
                        Err(e) => tracing::error!("Cluster controller panicked: {e}"),
                    }
                }
                res = topic_handle => {
                    match res {
                        Ok(()) => tracing::info!("Topic controller exited"),
                        Err(e) => tracing::error!("Topic controller panicked: {e}"),
                    }
                }
                res = user_handle => {
                    match res {
                        Ok(()) => tracing::info!("User controller exited"),
                        Err(e) => tracing::error!("User controller panicked: {e}"),
                    }
                }
                res = autoscaler_handle => {
                    match res {
                        Ok(()) => tracing::info!("AutoScaler controller exited"),
                        Err(e) => tracing::error!("AutoScaler controller panicked: {e}"),
                    }
                }
            }
            Ok(())
        }

        Commands::CrdGen => {
            // Print all CRD YAML definitions to stdout.
            let crds = [
                serde_yaml::to_string(&crds::ChronikStandalone::crd())?,
                serde_yaml::to_string(&crds::ChronikCluster::crd())?,
                serde_yaml::to_string(&crds::ChronikTopic::crd())?,
                serde_yaml::to_string(&crds::ChronikUser::crd())?,
                serde_yaml::to_string(&crds::ChronikAutoScaler::crd())?,
            ];

            for (i, crd_yaml) in crds.iter().enumerate() {
                if i > 0 {
                    println!("---");
                }
                print!("{crd_yaml}");
            }

            Ok(())
        }
    }
}
