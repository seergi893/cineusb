# CineUSB build maintenance

CI must execute USB tests successfully before it builds or publishes a package. A runner
that cannot install test prerequisites fails; a warning and skipped tests are not a pass.
Architecture checks, third-party notices, cache regressions and updater tests run before
compilation. The actual IPK still passes the existing ELF/link-receipt/package checks.

## Cache boundaries

Cargo registry sources and compiled registry dependencies are keyed by host/target,
architecture, toolchain, Cargo manifests and lockfile. The existing cargo-seed scrubber
removes every cached path-package output before building. Application code is always
rebuilt; cached application binaries can never satisfy Make by timestamp.

Native caches are exact matches only, keyed by stable/feed configuration, NDK manifest,
all recipes, patches, Makefile and compiler configuration. A complete byte manifest and
existing ELF link receipts are checked before Make sentinels are refreshed. The cache
contains library prefixes/staged libraries, never application objects or binaries.
The NDK is initialized at the same absolute path on every runner, and the build
configuration stamp is initialized before restoring libraries. Changed recipes invalidate
the cache instead of silently reusing different flags. Corruption fails closed.

## Parallelism and build scope

The two outer Make jobs and each native sub-build share an explicit runner-sized budget.
Cargo recipes are marked recursive so GNU Make passes valid jobserver descriptors.
Python/CMake native subprocesses clear inherited Make flags and use the explicit budget.
CineUSB packages select the existing feed-only configuration. Host USB tests disable
unrelated diagnostic defaults. Both changes use supported feature combinations.

Plex, session, media, appkit and screens are still shared dependencies: the local player
uses their playback session, metadata/track contracts, dialogs, and application routing.
Removing those crates requires extracting those shared contracts and a separate app shell;
merely hiding dependencies in Cargo would break the player. No claim is made that these
shared crates have been removed. The unused URI backend is excluded from the shipped build.

## Updates

HarfBuzz is updated to 15.1.0 and Sentry Native to 0.17.6. The existing Sentry patch applies
strictly without changing its webOS compatibility workarounds. Patch files use LF on Windows
as well as Linux. The webOS NDK and Rust pin are retained for a controlled comparison of this
build change; compiler updates remain separate reviewed Dependabot changes with regenerated
standard-library notices. No dependency update is merged automatically.

The scheduled native updater opens draft PRs, preserves existing/declined proposals and
explicitly dispatches CineUSB CI (bot pushes do not start workflows). The Dependabot notices
workflow executes scripts only from the trusted base; PR manifests/locks are read as data.
Only official numbered Rust toolchains may supply standard-library notices. Unclassified
license inputs fail for human review. The native dependency graph is submitted on main changes.

## Build benchmark

Measurements use GitHub-hosted Linux x64 tests and ARM64 NDK builds, stable release
configuration. Native cold/warm results must identify the exact commit/run and cache hits.
Tests finish much faster than compilation; these numbers are build latency, not TV performance.

| Run | Host job | ARM/package job | End-to-end | Notes |
| --- | --- | --- | --- | --- |
| Baseline c6fa23e, Actions 44311762932 | 3m38s | 5m58s | 9m44s | NDK cache only; both playback backends |
| Updated cold | pending | pending | pending | Fresh dependency/cache keys |
| Updated warm | pending | pending | pending | Same committed tree, restored verified caches |

A runner comparison includes machine/network variation; no speedup is claimed until both
updated runs complete. Physical LG playback/remote verification is a separate device check.
