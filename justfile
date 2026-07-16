check:
    cargo check --workspace

test:
    cargo test --workspace --exclude agentsmon --exclude agentsmon-ui

run:
    cargo run -p agentsmon

bundle:
    ./bundle/bundle.sh

