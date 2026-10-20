# [2.5.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.4.0...v2.5.0) (2026-10-20)


### Features

* **routines:** accept OpenCode agent routines ([37b53d7](https://github.com/MSpiechowicz/harness-useful-memory/commit/37b53d7e04de459a5a97892be763d0752df27b7c))

# [2.4.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.3.0...v2.4.0) (2026-10-20)


### Features

* **cli:** read every repository, one by key, or the global scope ([22a4abb](https://github.com/MSpiechowicz/harness-useful-memory/commit/22a4abb61b8fdda22fa6e3813e46addebb8c4912))

# [2.3.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.2.0...v2.3.0) (2026-10-19)


### Features

* **routing:** remote+local notes and scripts, hide on this computer, push --json ([f37a30e](https://github.com/MSpiechowicz/harness-useful-memory/commit/f37a30e0e8eb7c48a4a36a1d8f9cd4c8ebcbf7c3))

# [2.2.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.1.0...v2.2.0) (2026-10-19)


### Bug Fixes

* **test:** a missing CA file path that is absolute on Windows too ([8aa3e96](https://github.com/MSpiechowicz/harness-useful-memory/commit/8aa3e965e983ecef8801f278a80396496067c8af))


### Features

* **team:** Amazon DocumentDB as a team database ([849b745](https://github.com/MSpiechowicz/harness-useful-memory/commit/849b745245b0acb711810fe97c943a0a652fc435))

# [2.1.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.0.0...v2.1.0) (2026-10-19)


### Features

* **routines:** share routines through the team database ([23a650d](https://github.com/MSpiechowicz/harness-useful-memory/commit/23a650d5782b2553a5927e2cbaed3f541c2946e0))

# [2.0.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v1.4.0...v2.0.0) (2026-10-18)


* feat(mcp)!: name the note tools after notes and entities ([1a80df5](https://github.com/MSpiechowicz/harness-useful-memory/commit/1a80df56f0a2629734b4b2483b59e49f1c488912))


### BREAKING CHANGES

* the old memory_* tool names are gone. Agents and allow-lists
that call them must use the new names.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>

# [1.4.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v1.3.0...v1.4.0) (2026-10-18)


### Bug Fixes

* **test:** expect the PostgreSQL schema 3 in the newer-database check ([5557f03](https://github.com/MSpiechowicz/harness-useful-memory/commit/5557f03ec7d7d0fb59f682b47b07a6c23926274b))


### Features

* **hooks:** share hooks with the team through the team database ([8047481](https://github.com/MSpiechowicz/harness-useful-memory/commit/8047481fc93fc15157372102b9c5597922456e04)), closes [MSpiechowicz/harness-useful-desktop#26](https://github.com/MSpiechowicz/harness-useful-desktop/issues/26)

# [1.3.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v1.2.0...v1.3.0) (2026-10-17)


### Features

* **cli:** filter notes and scripts by scope and review status together ([e7fad03](https://github.com/MSpiechowicz/harness-useful-memory/commit/e7fad0304370888f7baab62e55d78dd190fa8fd0))

# [1.2.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v1.1.0...v1.2.0) (2026-10-17)


### Bug Fixes

* **ci:** compare the run request folder resolved on macOS ([6237fdb](https://github.com/MSpiechowicz/harness-useful-memory/commit/6237fdb872ad265f9cea72a043c41589b165a511))
* **ci:** pass the macOS runner test and Windows clippy ([cb559df](https://github.com/MSpiechowicz/harness-useful-memory/commit/cb559dfc8fdfea7f87de2e8b8fc7f6838cb2169e))


### Features

* **scripts:** save, approve and run deterministic scripts ([01b8009](https://github.com/MSpiechowicz/harness-useful-memory/commit/01b80094e307b964e14586188d8e164b7c8c0dfc))

# [1.1.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v1.0.1...v1.1.0) (2026-10-16)


### Bug Fixes

* build on Windows and read the macOS config folder in the team settings test ([7364e0e](https://github.com/MSpiechowicz/harness-useful-memory/commit/7364e0e0d9928a8dbdefbec7598620756b054f76))
* **postgres:** retry a write the server ended before its commit (SQLSTATE 57P01-57P03) ([bf0e2fb](https://github.com/MSpiechowicz/harness-useful-memory/commit/bf0e2fb7771bbe1d321811d85f3d369b7bf352a4))


### Features

* `harness-memory check` and per-scope routing of team memory ([f3617a0](https://github.com/MSpiechowicz/harness-useful-memory/commit/f3617a09499765b761bafbe099753da507b442b8))
* team memory on PostgreSQL, libSQL (remote SQLite) or MongoDB ([38b1198](https://github.com/MSpiechowicz/harness-useful-memory/commit/38b11986f496f9203e79af074886541227dc3c07)), closes [MSpiechowicz/harness-useful-desktop#8](https://github.com/MSpiechowicz/harness-useful-desktop/issues/8)

## [1.0.1](https://github.com/MSpiechowicz/harness-useful-memory/compare/v1.0.0...v1.0.1) (2026-10-16)


### Bug Fixes

* **release:** compare the draft against the binary names, not the artifact listing ([db131ef](https://github.com/MSpiechowicz/harness-useful-memory/commit/db131efdbc899eecfd365fa85885950a7e424ab1))

# 1.0.0 (2026-10-16)


### Features

* standalone harness-memory MCP server, extracted from Harness Desktop ([2729301](https://github.com/MSpiechowicz/harness-useful-memory/commit/2729301665787b5024977fd2070fefb9bee77d89)), closes [MSpiechowicz/harness-useful-desktop#8](https://github.com/MSpiechowicz/harness-useful-desktop/issues/8)
