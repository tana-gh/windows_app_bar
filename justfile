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

example-basic:
    cargo run -p windows_app_bar_example_basic

example-bevy:
    cargo run -p windows_app_bar_example_bevy

check-all: format lint test
