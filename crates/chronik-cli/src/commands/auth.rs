//! Authentication commands.

use crate::{client::AdminClient, output};
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};

/// Authentication commands
#[derive(Debug, clap::Parser)]
pub struct AuthCommand {
    #[command(subcommand)]
    command: AuthSubcommands,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommands {
    /// Login to get an access token
    Login {
        /// Username
        #[arg(short, long)]
        username: String,
        
        /// Password (will prompt if not provided)
        #[arg(short, long)]
        password: Option<String>,
    },
}

impl AuthCommand {
    pub async fn execute(
        &self,
        client: &AdminClient,
        _format: super::super::OutputFormat,
    ) -> Result<()> {
        match &self.command {
            AuthSubcommands::Login { username, password } => {
                let password = match password {
                    Some(p) => p.clone(),
                    None => {
                        // Prompt for password
                        rpassword::prompt_password("Password: ")?
                    }
                };
                
                let request = LoginRequest {
                    username: username.clone(),
                    password,
                };
                
                let response: LoginResponse = client
                    .post("/api/v1/auth/login", &request)
                    .await?;
                
                output::print_success("Login successful!");
                println!("\nAccess token: {}", response.access_token);
                println!("\nTo use this token, set the environment variable:");
                println!("  export CHRONIK_API_TOKEN=\"{}\"", response.access_token);
                println!("\nOr pass it with the -t flag:");
                println!("  chronik-ctl -t \"{}\" <command>", response.access_token);
            }
        }
        
        Ok(())
    }
}

// Request/Response types

#[derive(Debug, Serialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
    token_type: String,
    expires_in: i64,
}