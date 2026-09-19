set dotenv-load := false
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

clean:
    cargo clean

format:
    cargo fmt --all

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

check-all: format lint test
