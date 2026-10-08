# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/phoxal/cargo/compare/cargo-phoxal-v0.4.0...cargo-phoxal-v0.4.1) - 2026-10-08

### Added

- report command progress without obscuring output

### Other

- include explicit native setup before simulation ([#11](https://github.com/phoxal/cargo/pull/11))
- name published source-reference support

## [0.4.0](https://github.com/phoxal/cargo/compare/cargo-phoxal-v0.3.0...cargo-phoxal-v0.4.0) - 2026-10-07

### Added

- [**breaking**] resolve nested robot documents and named sources

### Other

- make native and process fixtures explicit ([#6](https://github.com/phoxal/cargo/pull/6))

## [0.3.0](https://github.com/phoxal/cargo/compare/cargo-phoxal-v0.2.0...cargo-phoxal-v0.3.0) - 2026-10-07

### Added

- [**breaking**] deliver layered robot authoring and compiled bindings

## [0.2.0](https://github.com/phoxal/cargo/compare/cargo-phoxal-v0.1.1...cargo-phoxal-v0.2.0) - 2026-10-06

### Added

- [**breaking**] simplify robot builds simulation and scenario commands

### Other

- isolate warning fixture from parent CI rustflags

## [0.1.1](https://github.com/phoxal/cargo/compare/cargo-phoxal-v0.1.0...cargo-phoxal-v0.1.1) - 2026-10-04

### Fixed

- *(cli)* reject incomplete simulation shutdown reports

### Other

- *(release)* update package versions ([#1](https://github.com/phoxal/cargo/pull/1))

## [0.1.0](https://github.com/phoxal/cargo/releases/tag/cargo-phoxal-v0.1.0) - 2026-10-04

### Added

- *(cli)* use crates.io sources and delegate native simulation
- extract the standalone Phoxal developer tool
