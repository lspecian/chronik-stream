# Contributing to Chronik Stream

Thank you for your interest in contributing to Chronik Stream! We welcome contributions from the community.

## How to Contribute

### Reporting Issues

If you find a bug or have a feature request, please open an issue on GitHub:
1. Check if the issue already exists
2. Create a new issue with a clear title and description
3. Include steps to reproduce (for bugs)
4. Add relevant labels

### Pull Requests

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Make your changes
4. Write or update tests
5. Ensure all tests pass (`cargo test --all`)
6. Format your code (`cargo fmt`)
7. Run clippy (`cargo clippy -- -D warnings`)
8. Commit with a clear message
9. Push to your fork
10. Open a pull request

### Development Setup

```bash
# Clone your fork
git clone git@github.com:YOUR_USERNAME/chronik-stream.git
cd chronik-stream

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build the project
cargo build

# Run tests
cargo test --all

# Start local development environment
docker-compose up -d
```

### Code Style

- Follow Rust naming conventions
- Use `cargo fmt` before committing
- Write clear, self-documenting code
- Add comments for complex logic
- Keep functions small and focused

### Testing

- Write unit tests for new functionality
- Add integration tests for API changes
- Ensure all tests pass before submitting PR
- Aim for >80% code coverage

### Documentation

- Update README.md if needed
- Add inline documentation for public APIs
- Update configuration examples
- Include examples in doc comments

### Commit Messages

Follow conventional commits:
- `feat:` New feature
- `fix:` Bug fix
- `docs:` Documentation changes
- `test:` Test additions/changes
- `refactor:` Code refactoring
- `perf:` Performance improvements
- `chore:` Maintenance tasks

Example: `feat: add consumer group coordinator`

### Review Process

1. Automated CI checks must pass
2. At least one maintainer review required
3. Address review feedback
4. Squash commits if requested
5. Maintainer will merge when ready

## Development Guidelines

### Architecture Decisions

- Discuss major changes in an issue first
- Follow existing patterns and conventions
- Maintain backward compatibility
- Consider performance implications

### Security

- Never commit secrets or credentials
- Report security issues privately to security@chronik-stream.io
- Follow secure coding practices
- Validate all inputs

## Community

- Be respectful and inclusive
- Follow our [Code of Conduct](CODE_OF_CONDUCT.md)
- Help others in issues and discussions
- Share your use cases and feedback

## License

By contributing, you agree that your contributions will be licensed under the MIT License.

## Questions?

Feel free to ask questions in:
- GitHub Discussions
- Discord: [Join our community](https://discord.gg/chronik-stream)
- Email: hello@chronik-stream.io

Thank you for contributing to Chronik Stream! 🚀