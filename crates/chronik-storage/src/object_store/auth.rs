//! Authentication configuration for different storage backends.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Authentication configuration for different backends
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthConfig {
    /// No authentication required (local storage)
    None,
    
    /// S3 authentication
    S3(S3Credentials),
    
    /// Google Cloud Storage authentication
    Gcs(GcsCredentials),
    
    /// Azure authentication
    Azure(AzureCredentials),
}

/// S3 authentication methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum S3Credentials {
    /// Use AWS credentials from environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)
    FromEnvironment,
    
    /// Use AWS credentials from shared credentials file (~/.aws/credentials)
    FromSharedCredentials {
        profile: Option<String>,
    },
    
    /// Use IAM role credentials (for EC2 instances)
    FromInstanceMetadata,
    
    /// Use IAM role for service accounts (for EKS)
    FromServiceAccount {
        role_arn: String,
        session_name: Option<String>,
    },
    
    /// Use explicit access key credentials
    AccessKey {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    
    /// Use STS assume role
    AssumeRole {
        role_arn: String,
        session_name: String,
        external_id: Option<String>,
        duration_seconds: Option<u32>,
    },
    
    /// Use AWS SSO credentials
    Sso {
        start_url: String,
        region: String,
        account_id: String,
        role_name: String,
    },
}

/// Google Cloud Storage authentication methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum GcsCredentials {
    /// Use default Google Cloud credentials (gcloud, workload identity, etc.)
    Default,
    
    /// Use service account key file
    ServiceAccountKey {
        key_file: PathBuf,
    },
    
    /// Use service account key from environment variable (GOOGLE_APPLICATION_CREDENTIALS)
    FromEnvironment,
    
    /// Use workload identity (for GKE)
    WorkloadIdentity {
        service_account: String,
    },
    
    /// Use explicit service account credentials
    ServiceAccount {
        client_email: String,
        private_key: String,
        private_key_id: Option<String>,
    },
}

/// Azure authentication methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AzureCredentials {
    /// Use Azure default credential chain
    DefaultChain,
    
    /// Use connection string
    ConnectionString {
        connection_string: String,
    },
    
    /// Use storage account key
    AccountKey {
        account_name: String,
        account_key: String,
    },
    
    /// Use SAS token
    SasToken {
        account_name: String,
        sas_token: String,
    },
    
    /// Use Azure AD service principal
    ServicePrincipal {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
    
    /// Use managed identity (for Azure VMs/containers)
    ManagedIdentity {
        client_id: Option<String>,
    },
    
    /// Use Azure CLI credentials
    AzureCli,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig::None
    }
}

impl S3Credentials {
    /// Create access key credentials
    pub fn access_key(
        access_key_id: impl Into<String>, 
        secret_access_key: impl Into<String>
    ) -> Self {
        S3Credentials::AccessKey {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }
    
    /// Create access key credentials with session token
    pub fn access_key_with_token(
        access_key_id: impl Into<String>, 
        secret_access_key: impl Into<String>,
        session_token: impl Into<String>
    ) -> Self {
        S3Credentials::AccessKey {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: Some(session_token.into()),
        }
    }
    
    /// Create shared credentials with profile
    pub fn shared_credentials(profile: Option<String>) -> Self {
        S3Credentials::FromSharedCredentials { profile }
    }
    
    /// Create assume role credentials
    pub fn assume_role(
        role_arn: impl Into<String>,
        session_name: impl Into<String>
    ) -> Self {
        S3Credentials::AssumeRole {
            role_arn: role_arn.into(),
            session_name: session_name.into(),
            external_id: None,
            duration_seconds: None,
        }
    }
}

impl GcsCredentials {
    /// Create service account key credentials
    pub fn service_account_key(key_file: impl Into<PathBuf>) -> Self {
        GcsCredentials::ServiceAccountKey {
            key_file: key_file.into(),
        }
    }
    
    /// Create workload identity credentials
    pub fn workload_identity(service_account: impl Into<String>) -> Self {
        GcsCredentials::WorkloadIdentity {
            service_account: service_account.into(),
        }
    }
}

impl AzureCredentials {
    /// Create connection string credentials
    pub fn connection_string(connection_string: impl Into<String>) -> Self {
        AzureCredentials::ConnectionString {
            connection_string: connection_string.into(),
        }
    }
    
    /// Create account key credentials
    pub fn account_key(
        account_name: impl Into<String>,
        account_key: impl Into<String>
    ) -> Self {
        AzureCredentials::AccountKey {
            account_name: account_name.into(),
            account_key: account_key.into(),
        }
    }
    
    /// Create SAS token credentials
    pub fn sas_token(
        account_name: impl Into<String>,
        sas_token: impl Into<String>
    ) -> Self {
        AzureCredentials::SasToken {
            account_name: account_name.into(),
            sas_token: sas_token.into(),
        }
    }
    
    /// Create service principal credentials
    pub fn service_principal(
        tenant_id: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>
    ) -> Self {
        AzureCredentials::ServicePrincipal {
            tenant_id: tenant_id.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }
    
    /// Create managed identity credentials
    pub fn managed_identity(client_id: Option<String>) -> Self {
        AzureCredentials::ManagedIdentity { client_id }
    }
}

/// Trait for validating authentication configurations
pub trait AuthValidation {
    /// Validate the authentication configuration
    fn validate(&self) -> Result<(), String>;
    
    /// Check if authentication requires environment variables
    fn requires_environment(&self) -> bool;
    
    /// Get list of required environment variables
    fn required_env_vars(&self) -> Vec<&'static str>;
}

impl AuthValidation for S3Credentials {
    fn validate(&self) -> Result<(), String> {
        match self {
            S3Credentials::AccessKey { access_key_id, secret_access_key, .. } => {
                if access_key_id.is_empty() {
                    return Err("Access key ID cannot be empty".to_string());
                }
                if secret_access_key.is_empty() {
                    return Err("Secret access key cannot be empty".to_string());
                }
            }
            S3Credentials::AssumeRole { role_arn, session_name, .. } => {
                if role_arn.is_empty() {
                    return Err("Role ARN cannot be empty".to_string());
                }
                if session_name.is_empty() {
                    return Err("Session name cannot be empty".to_string());
                }
            }
            S3Credentials::FromServiceAccount { role_arn, .. } => {
                if role_arn.is_empty() {
                    return Err("Role ARN cannot be empty".to_string());
                }
            }
            S3Credentials::Sso { start_url, region, account_id, role_name } => {
                if start_url.is_empty() || region.is_empty() || account_id.is_empty() || role_name.is_empty() {
                    return Err("All SSO fields must be non-empty".to_string());
                }
            }
            _ => {}
        }
        Ok(())
    }
    
    fn requires_environment(&self) -> bool {
        matches!(self, S3Credentials::FromEnvironment)
    }
    
    fn required_env_vars(&self) -> Vec<&'static str> {
        match self {
            S3Credentials::FromEnvironment => vec!["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"],
            _ => vec![],
        }
    }
}

impl AuthValidation for GcsCredentials {
    fn validate(&self) -> Result<(), String> {
        match self {
            GcsCredentials::ServiceAccountKey { key_file } => {
                if !key_file.exists() {
                    return Err(format!("Service account key file not found: {:?}", key_file));
                }
            }
            GcsCredentials::WorkloadIdentity { service_account } => {
                if service_account.is_empty() {
                    return Err("Service account cannot be empty".to_string());
                }
            }
            GcsCredentials::ServiceAccount { client_email, private_key, .. } => {
                if client_email.is_empty() || private_key.is_empty() {
                    return Err("Client email and private key cannot be empty".to_string());
                }
            }
            _ => {}
        }
        Ok(())
    }
    
    fn requires_environment(&self) -> bool {
        matches!(self, GcsCredentials::FromEnvironment)
    }
    
    fn required_env_vars(&self) -> Vec<&'static str> {
        match self {
            GcsCredentials::FromEnvironment => vec!["GOOGLE_APPLICATION_CREDENTIALS"],
            _ => vec![],
        }
    }
}

impl AuthValidation for AzureCredentials {
    fn validate(&self) -> Result<(), String> {
        match self {
            AzureCredentials::ConnectionString { connection_string } => {
                if connection_string.is_empty() {
                    return Err("Connection string cannot be empty".to_string());
                }
            }
            AzureCredentials::AccountKey { account_name, account_key } => {
                if account_name.is_empty() || account_key.is_empty() {
                    return Err("Account name and account key cannot be empty".to_string());
                }
            }
            AzureCredentials::SasToken { account_name, sas_token } => {
                if account_name.is_empty() || sas_token.is_empty() {
                    return Err("Account name and SAS token cannot be empty".to_string());
                }
            }
            AzureCredentials::ServicePrincipal { tenant_id, client_id, client_secret } => {
                if tenant_id.is_empty() || client_id.is_empty() || client_secret.is_empty() {
                    return Err("Tenant ID, client ID, and client secret cannot be empty".to_string());
                }
            }
            _ => {}
        }
        Ok(())
    }
    
    fn requires_environment(&self) -> bool {
        false // Azure default chain handles environment automatically
    }
    
    fn required_env_vars(&self) -> Vec<&'static str> {
        vec![]
    }
}

impl AuthValidation for AuthConfig {
    fn validate(&self) -> Result<(), String> {
        match self {
            AuthConfig::None => Ok(()),
            AuthConfig::S3(creds) => creds.validate(),
            AuthConfig::Gcs(creds) => creds.validate(),
            AuthConfig::Azure(creds) => creds.validate(),
        }
    }
    
    fn requires_environment(&self) -> bool {
        match self {
            AuthConfig::None => false,
            AuthConfig::S3(creds) => creds.requires_environment(),
            AuthConfig::Gcs(creds) => creds.requires_environment(),
            AuthConfig::Azure(creds) => creds.requires_environment(),
        }
    }
    
    fn required_env_vars(&self) -> Vec<&'static str> {
        match self {
            AuthConfig::None => vec![],
            AuthConfig::S3(creds) => creds.required_env_vars(),
            AuthConfig::Gcs(creds) => creds.required_env_vars(),
            AuthConfig::Azure(creds) => creds.required_env_vars(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_s3_credentials_validation() {
        let valid_creds = S3Credentials::access_key("key", "secret");
        assert!(valid_creds.validate().is_ok());
        
        let invalid_creds = S3Credentials::AccessKey {
            access_key_id: "".to_string(),
            secret_access_key: "secret".to_string(),
            session_token: None,
        };
        assert!(invalid_creds.validate().is_err());
    }

    #[test]
    fn test_environment_requirements() {
        let env_creds = S3Credentials::FromEnvironment;
        assert!(env_creds.requires_environment());
        assert_eq!(env_creds.required_env_vars(), vec!["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]);
        
        let key_creds = S3Credentials::access_key("key", "secret");
        assert!(!key_creds.requires_environment());
        assert!(key_creds.required_env_vars().is_empty());
    }

    #[test]
    fn test_auth_config_validation() {
        let config = AuthConfig::S3(S3Credentials::access_key("key", "secret"));
        assert!(config.validate().is_ok());
        
        let invalid_config = AuthConfig::S3(S3Credentials::AccessKey {
            access_key_id: "".to_string(),
            secret_access_key: "".to_string(),
            session_token: None,
        });
        assert!(invalid_config.validate().is_err());
    }
}