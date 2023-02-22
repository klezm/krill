FROM mcr.microsoft.com/devcontainers/rust:0-1-bullseye

RUN sudo apt update && \
    sudo apt install -y \
        ca-certificates curl gnupg lsb-release \
        curl wget git gcc rsync build-essential \
        pkg-config libssl-dev \
        bat mc expect colorized-logs

RUN rustup install nightly

RUN rustup component add rust-src rust-analyzer clippy rustfmt

RUN cargo +nightly install cargo-fuzz cargo-test-fuzz afl
