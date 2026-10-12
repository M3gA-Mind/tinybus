#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
cargo build --locked --example module_clock --example module_clock_two --example module_wrong_target --example module_context --example module_context_eager --no-default-features --features modules,macros
cargo test --locked --no-default-features --features modules,macros,test-support test_support::tests
case "${RUNNER_OS:-$(uname -s)}" in
  Windows|MINGW*|MSYS*|CYGWIN*) pwsh -NoProfile -File scripts/test-e2e.ps1 ;;
  *)
set -euo pipefail
module=$(find target/debug/examples -maxdepth 1 -type f \( -name 'libmodule_clock.so' -o -name 'libmodule_clock.dylib' \) -print -quit)
module_two=$(find target/debug/examples -maxdepth 1 -type f \( -name 'libmodule_clock_two.so' -o -name 'libmodule_clock_two.dylib' \) -print -quit)
wrong_target=$(find target/debug/examples -maxdepth 1 -type f \( -name 'libmodule_wrong_target.so' -o -name 'libmodule_wrong_target.dylib' \) -print -quit)
context_module=$(find target/debug/examples -maxdepth 1 -type f \( -name 'libmodule_context.so' -o -name 'libmodule_context.dylib' \) -print -quit)
context_eager_module=$(find target/debug/examples -maxdepth 1 -type f \( -name 'libmodule_context_eager.so' -o -name 'libmodule_context_eager.dylib' \) -print -quit)
for artifact in "$module" "$module_two" "$wrong_target" "$context_module" "$context_eager_module"; do
  if [ -z "$artifact" ] || [ ! -f "$artifact" ]; then
    echo "a module example artifact is missing; the loader tests would not exercise it" >&2
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
TINYBUS_TEST_MODULE="$PWD/$module" TINYBUS_TEST_MODULE_TWO="$private/$library" TINYBUS_TEST_WRONG_TARGET="$PWD/$wrong_target" TINYBUS_TEST_CONTEXT_MODULE="$PWD/$context_module" TINYBUS_TEST_CONTEXT_EAGER_MODULE="$PWD/$context_eager_module" cargo test --locked --no-default-features --features modules,macros,test-support -- --ignored

cargo test --locked --no-default-features --features modules,macros admission_tests -- --include-ignored
TINYBUS_TEST_UMASK=1 cargo test --locked --no-default-features --features modules,macros permissive_umask -- --test-threads=1
    ;;
esac
