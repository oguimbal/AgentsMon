check:
    cargo check --workspace

test:
    cargo test --workspace --exclude agentsmon --exclude agentsmon-ui

run:
    cargo run -p agentsmon

bundle:
    ./bundle/bundle.sh

dmg:
    ./bundle/dmg.sh

# Bump the version, commit, tag and push; CI builds, signs and publishes.
release bump="patch":
    ./scripts/release.sh {{bump}}
