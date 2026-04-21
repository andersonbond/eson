# just dev  —  memory + agent (run desktop in another terminal)
dev:
    #!/usr/bin/env bash
    set -euo pipefail
    export ESON_WORKSPACE_ROOT="${ESON_WORKSPACE_ROOT:-$PWD/workspace}"
    cargo run -p eson-memory &
    sleep 1
    cargo run -p eson-agent

test:
    cargo test -p eson-agent -p eson-memory

test-workspace:
    #!/usr/bin/env bash
    set -euo pipefail
    cd apps/desktop && npm run build
    cd ../.. && cargo test --workspace

# macOS .app + .dmg (see build.md); unset CI if your env sets CI=1
build-desktop:
    #!/usr/bin/env bash
    set -euo pipefail
    cd apps/desktop && npm install && env -u CI npm run tauri build

# One-click installer: bundles eson-agent + eson-memory as Tauri sidecars
# plus persona/ + skills/ as resources. Output:
#   apps/desktop/src-tauri/target/release/bundle/dmg/Eson_<v>_<arch>.dmg
installer:
    ./scripts/build-installer.sh
