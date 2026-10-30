#!/usr/bin/env bash
set -Eeuo pipefail
umask 077
: "${SOURCE_SHA:?}"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]
[[ "$(git rev-parse HEAD^)" == "$SOURCE_SHA" ]]
[[ ! -e probe-artifact && ! -L probe-artifact ]]
mkdir -m 700 probe-artifact
gcc --version > probe-artifact/compiler.txt
gcc -std=c11 -O2 -g -Wall -Wextra -Werror -fPIC -shared \
  .github/proof/turn-io-probe.c -o probe-artifact/turn-io-probe.so
gcc -std=c11 -O2 -g -Wall -Wextra -Werror -pthread \
  .github/proof/probe-selftest.c -o probe-artifact/probe-selftest
probe-artifact/probe-selftest "$PWD/probe-artifact/turn-io-probe.so" > probe-artifact/selftest.txt
grep -Fx 'PASS marker exit=86' probe-artifact/selftest.txt
grep -Fx 'PASS malformed exit=87' probe-artifact/selftest.txt
printf '{"source_sha":"%s","helper_sha":"%s","purpose":"test-only native I/O boundary probe"}\n' \
  "$SOURCE_SHA" "$(git rev-parse HEAD)" > probe-artifact/build.json
(cd probe-artifact && sha256sum turn-io-probe.so probe-selftest compiler.txt selftest.txt build.json > SHA256SUMS)
