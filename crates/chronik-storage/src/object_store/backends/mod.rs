//! Storage backend implementations for different cloud providers and local storage.

pub mod s3;
pub mod gcs;
pub mod azure;
pub mod local;

pub use s3::S3Backend;
pub use gcs::GcsBackend;
pub use azure::AzureBackend;
pub use local::LocalBackend;

/// OpenDAL root from optional path parts: `/a/b/`, or `/` with none.
pub(crate) fn opendal_root(parts: &[Option<&str>]) -> String {
    let joined: Vec<&str> = parts
        .iter()
        .flatten()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .collect();
    if joined.is_empty() {
        "/".to_string()
    } else {
        format!("/{}/", joined.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::opendal_root;

    #[test]
    fn root_joins_present_parts() {
        assert_eq!(opendal_root(&[]), "/");
        assert_eq!(opendal_root(&[None, Some("")]), "/");
        assert_eq!(opendal_root(&[Some("proj"), None]), "/proj/");
        assert_eq!(opendal_root(&[Some("proj"), Some("/chronik/")]), "/proj/chronik/");
        assert_eq!(opendal_root(&[None, Some("a/b")]), "/a/b/");
    }
}