# Contributing to Chronik Stream

Thank you for your interest in contributing to Chronik Stream! This document provides guidelines for contributing to the project.

## Code of Conduct

By participating in this project, you agree to abide by our Code of Conduct. We expect all contributors to be respectful, inclusive, and professional.

## How to Contribute

### Reporting Issues

- Check if the issue already exists in the [issue tracker](https://github.com/chronik-stream/chronik-stream/issues)
- Provide a clear description of the problem
- Include steps to reproduce the issue
- Share relevant logs, error messages, and system information

### Suggesting Features

- Open a discussion in [GitHub Discussions](https://github.com/chronik-stream/chronik-stream/discussions)
- Describe the use case and benefits
- Consider the impact on existing users
- Be open to feedback and alternative approaches

### Submitting Code

1. **Fork the repository** and create a feature branch
2. **Write your code** following our coding standards
3. **Add tests** for new functionality
4. **Update documentation** as needed
5. **Submit a pull request** with a clear description

## Development Setup

### Prerequisites

- Rust 1.70+
- PostgreSQL 14+
- Docker & Docker Compose
- Git

### Building from Source

```bash
# Clone your fork
git clone https://github.com/YOUR_USERNAME/chronik-stream.git
cd chronik-stream

# Add upstream remote
git remote add upstream https://github.com/chronik-stream/chronik-stream.git

# Build the project
cargo build --workspace

# Run tests
cargo test --workspace

# Run lints
cargo clippy --workspace --all-targets
```

### Running Tests

```bash
# Unit tests
cargo test --workspace

# Integration tests
cargo test --workspace --test '*' -- --test-threads=1

# Benchmarks
cargo bench

# Specific crate tests
cargo test -p chronik-protocol
```

## Coding Standards

### Rust Style Guide

- Follow the official [Rust Style Guide](https://github.com/rust-lang/style-team/blob/master/guide/guide.md)
- Use `rustfmt` to format code: `cargo fmt --all`
- Use `clippy` for linting: `cargo clippy --all-targets --all-features`
- Write documentation for public APIs
- Add unit tests for new functionality

### Code Organization

- Keep modules focused and cohesive
- Use meaningful names for functions and variables
- Avoid deeply nested code structures
- Prefer composition over inheritance

### Error Handling

- Use `Result<T, E>` for fallible operations
- Create specific error types for each crate
- Provide context in error messages
- Avoid panics in library code

### Documentation

- Document all public APIs with `///` comments
- Include examples in documentation
- Keep README files up to date
- Add inline comments for complex logic

## Pull Request Process

### Before Submitting

1. **Rebase on main**: Keep your branch up to date
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. **Run tests**: Ensure all tests pass
   ```bash
   cargo test --workspace
   ```

3. **Check formatting**: Run formatters and linters
   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   ```

4. **Update CHANGELOG**: Add your changes to the unreleased section

### PR Guidelines

- **Title**: Use a clear, descriptive title
- **Description**: Explain what, why, and how
- **Size**: Keep PRs focused and reasonably sized
- **Tests**: Include tests for new functionality
- **Documentation**: Update relevant documentation
- **Breaking Changes**: Clearly mark any breaking changes

### Review Process

1. CI must pass all checks
2. At least one maintainer approval required
3. Address all review feedback
4. Maintainer will merge when ready

## Testing Guidelines

### Unit Tests

- Test individual functions and methods
- Use descriptive test names
- Cover edge cases and error conditions
- Keep tests focused and independent

Example:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_creation() {
        let segment = Segment::new("test-topic", 0);
        assert_eq!(segment.topic(), "test-topic");
        assert_eq!(segment.partition(), 0);
    }
}
```

### Integration Tests

- Test interactions between components
- Use real dependencies when possible
- Clean up resources after tests
- Document test prerequisites

### Benchmarks

- Add benchmarks for performance-critical code
- Use meaningful benchmark names
- Compare against baseline when optimizing
- Document benchmark methodology

## Documentation

### Code Documentation

```rust
/// Processes a batch of messages for indexing.
///
/// # Arguments
///
/// * `messages` - The messages to process
/// * `topic` - The topic name
///
/// # Returns
///
/// Returns `Ok(())` on success, or an error if processing fails.
///
/// # Examples
///
/// ```
/// let messages = vec![Message::new("hello")];
/// process_batch(&messages, "my-topic")?;
/// ```
pub fn process_batch(messages: &[Message], topic: &str) -> Result<()> {
    // Implementation
}
```

### API Documentation

- Document all REST endpoints
- Include request/response examples
- Specify error conditions
- Note authentication requirements

## Release Process

1. **Version Bump**: Update version in `Cargo.toml`
2. **Changelog**: Update CHANGELOG.md
3. **Tag Release**: Create git tag
4. **Build Artifacts**: Build release binaries
5. **Publish**: Push to crates.io and Docker Hub

## Community

### Getting Help

- GitHub Discussions for questions
- Issue tracker for bugs
- Discord/Slack for real-time chat

### Maintainers

- @maintainer1
- @maintainer2
- @maintainer3

### License

By contributing to Chronik Stream, you agree that your contributions will be licensed under the Apache License 2.0.

## Recognition

Contributors will be recognized in:
- The CONTRIBUTORS file
- Release notes
- Project documentation

Thank you for contributing to Chronik Stream!