# Lists all available commands.
default:
    just --list

# Install tools required by other recipes.
install-tools:
    rustup toolchain add nightly
    cargo +stable install cargo-hack --locked
    cargo +stable install cargo-minimal-versions --locked
    cargo +stable install cargo-msrv --locked

# Check if the current dependency version bounds are sufficient.
minimal-versions:
    cargo minimal-versions check --workspace --direct

# Find the minimum supported rust version.
msrv:
    cargo msrv find

# Lint the code.
clippy:
    cargo clippy --all --all-features -- -W clippy::pedantic

# Update all deps; sort all Cargo.toml deps; format all code.
tidy:
    cargo update --workspace
    cargo sort --workspace
    cargo fmt --all

# Run the full non-mutating validation suite.
verify:
    cargo fmt --all -- --check
    cargo check --all-targets --all-features
    cargo check --lib --no-default-features
    cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic
    cargo test --all --all-features
    cargo test --doc --no-default-features
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --no-default-features
