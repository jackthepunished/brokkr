# Changelog

All notable changes to Brokkr will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `brokk run -c '<command>'` subcommand — spawns a local shell process,
  captures and prints stdout/stderr, propagates exit code.
- `brokk version` subcommand, with git SHA + rustc + target triple embedded
  at build time.

### Changed
- MSRV bumped from 1.78 → 1.85 during bootstrap (transitive deps require
  edition 2024).
