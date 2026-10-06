# Contributing to Remote Desk

Thank you for your interest in contributing! This document provides guidelines and instructions for contributing to Remote Desk.

## Code of Conduct

Please read and follow our [Code of Conduct](CODE_OF_CONDUCT.md).

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/yourusername/remote-desk.git`
3. Create a feature branch: `git checkout -b feature/your-feature-name`
4. Make your changes
5. Test your changes with `cargo test`
6. Commit your changes with clear, descriptive messages
7. Push to your fork and open a Pull Request

## Development Setup

### Requirements
- Rust 1.88 or later
- PipeWire
- KDE Plasma 6 on Wayland
- Clang (for PipeWire bindings)

### Building
```sh
cargo build --release
```

### Testing
```sh
cargo test
RUST_LOG=remote_desk=debug cargo run --release -- serve
```

## Reporting Bugs

Before creating a bug report, please check the existing issues. When creating a bug report, include:

- A clear title and description
- Steps to reproduce the issue
- Expected behavior
- Actual behavior
- Your system configuration (KDE Plasma version, Wayland, etc.)
- Relevant logs or error messages

## Suggesting Enhancements

Enhancement suggestions are welcome! Please provide:

- A clear title and description
- The motivation and use case
- Possible implementation approaches (if you have ideas)

## Pull Request Process

1. Update documentation and README if needed
2. Add tests for new functionality
3. Ensure all tests pass: `cargo test`
4. Keep commits clean and well-organized
5. Write clear, descriptive commit messages
6. Reference any related issues in the PR description

## Code Style

- Follow Rust conventions and idioms
- Use `cargo fmt` for formatting
- Use `cargo clippy` to catch common mistakes
- Keep functions focused and readable

## License

By contributing to Remote Desk, you agree that your contributions will be licensed under its MIT License.

## Questions?

Feel free to open an issue or discussion for questions about contributing.
