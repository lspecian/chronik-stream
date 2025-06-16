//! HTTP client for Admin API.

use anyhow::Result;
use reqwest::{Client, RequestBuilder};
use serde::{de::DeserializeOwned, Serialize};

/// Admin API client
pub struct AdminClient {
    client: Client,
    base_url: String,
    token: Option<String>,
}

impl AdminClient {
    /// Create new admin client
    pub fn new(base_url: &str, token: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        })
    }
    
    /// Add auth header if token is set
    fn add_auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }
    
    /// GET request
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.client.get(&url);
        let request = self.add_auth(request);
        
        let response = request.send().await?;
        
        if !response.status().is_success() {
            let error = response.text().await?;
            anyhow::bail!("Request failed: {}", error);
        }
        
        Ok(response.json().await?)
    }
    
    /// POST request
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.client.post(&url).json(body);
        let request = self.add_auth(request);
        
        let response = request.send().await?;
        
        if !response.status().is_success() {
            let error = response.text().await?;
            anyhow::bail!("Request failed: {}", error);
        }
        
        Ok(response.json().await?)
    }
    
    /// PUT request
    pub async fn put<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.client.put(&url).json(body);
        let request = self.add_auth(request);
        
        let response = request.send().await?;
        
        if !response.status().is_success() {
            let error = response.text().await?;
            anyhow::bail!("Request failed: {}", error);
        }
        
        Ok(())
    }
    
    /// DELETE request
    pub async fn delete(&self, path: &str) -> Result<()> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.client.delete(&url);
        let request = self.add_auth(request);
        
        let response = request.send().await?;
        
        if !response.status().is_success() {
            let error = response.text().await?;
            anyhow::bail!("Request failed: {}", error);
        }
        
        Ok(())
    }
}