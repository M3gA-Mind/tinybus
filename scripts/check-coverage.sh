#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
cargo build --locked --example module_clock --example module_clock_two --example module_wrong_target --example module_context --example module_context_eager --no-default-features --features modules,macros
set -euo pipefail
module=$(find target/debug/examples -maxdepth 1 -type f -name 'libmodule_clock.so' -print -quit)
module_two=$(find target/debug/examples -maxdepth 1 -type f -name 'libmodule_clock_two.so' -print -quit)
wrong_target=$(find target/debug/examples -maxdepth 1 -type f -name 'libmodule_wrong_target.so' -print -quit)
context_module=$(find target/debug/examples -maxdepth 1 -type f -name 'libmodule_context.so' -print -quit)
context_eager_module=$(find target/debug/examples -maxdepth 1 -type f -name 'libmodule_context_eager.so' -print -quit)
for artifact in "$module" "$module_two" "$wrong_target" "$context_module" "$context_eager_module"; do
  if [ -z "$artifact" ] || [ ! -f "$artifact" ]; then
    echo "a module loader fixture is missing" >&2
    exit 1
  fi
done
private="$PWD/target/private-module-tests/module-clock-two"
mkdir -p "$private"
chmod 700 "$private"
cp "$module_two" "$private/"
library="$(basename "$module_two")"
if command -v sha256sum >/dev/null 2>&1; then
  digest="$(sha256sum "$private/$library" | awk '{print $1}')"
else
  digest="$(shasum -a 256 "$private/$library" | awk '{print $1}')"
fi
printf '"%s" = "%s"\n' "$library" "$digest" > "$private/modules.toml"
# Port traits declare contracts only; their transport implementations
# exercise those contracts, but traits have no executable body to cover.
# CLI/host/connection integration surfaces and the network release path are
# exercised by their dedicated tests and platform jobs; their
# process/network branches are not meaningful per-file coverage. The
# test-support module is integration-test glue whose platform-specific
# dynamic-loader and timeout paths run in the dedicated OS jobs instead.
TINYBUS_TEST_MODULE="$PWD/$module" TINYBUS_TEST_MODULE_TWO="$private/$library" TINYBUS_TEST_WRONG_TARGET="$PWD/$wrong_target" TINYBUS_TEST_CONTEXT_MODULE="$PWD/$context_module" TINYBUS_TEST_CONTEXT_EAGER_MODULE="$PWD/$context_eager_module" cargo llvm-cov --workspace --all-features --ignore-filename-regex 'crates/tinybus/src/ports/(listener|transport)\.rs$|crates/tinybus/src/module/github\.rs$|crates/tinybus/src/module/(host|mod)\.rs$|crates/tinybus/src/bin/(tinybus\.rs|tinybus/unix\.rs)$|crates/tinybus/src/connection\.rs$|crates/tinybus/src/test_support\.rs$' --fail-under-file-lines 90 -- --include-ignored
