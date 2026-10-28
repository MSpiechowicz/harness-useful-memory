# [2.10.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.9.2...v2.10.0) (2026-10-28)


### Features

* **usefulness:** note_feedback tool and per-note usefulness counters ([4f79edb](https://github.com/MSpiechowicz/harness-useful-memory/commit/4f79edbc7f9decdd75d0e510d501c572f0d511a3))

## [2.9.2](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.9.1...v2.9.2) (2026-10-26)


### Bug Fixes

* **guards:** a call that names its project is judged by that project, not the server's folder ([37a60c9](https://github.com/MSpiechowicz/harness-useful-memory/commit/37a60c99996eb3535ae409bb8183a60e9dda0cef))

## [2.9.1](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.9.0...v2.9.1) (2026-10-26)


### Bug Fixes

* **offline:** share the unreachable team database cooldown between processes ([6fdce90](https://github.com/MSpiechowicz/harness-useful-memory/commit/6fdce90237ab12f97dfc67aa674f5762883885fc))

# [2.9.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.8.0...v2.9.0) (2026-10-26)


### Features

* **offline:** serve remote scopes from this computer while the team database is unreachable ([c2cc18b](https://github.com/MSpiechowicz/harness-useful-memory/commit/c2cc18bdfbde2bdac55c18b174a415ffb2f422d3))

# [2.8.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.7.0...v2.8.0) (2026-10-23)


### Bug Fixes

* **embedder:** listen on macOS and build tests on Windows ([7a5f43c](https://github.com/MSpiechowicz/harness-useful-memory/commit/7a5f43c4d9e6ca74b560642051b866a6dc30bff0))
* **embedder:** never chmod a socket folder by path, exact abandoned-call count ([d2d7d6d](https://github.com/MSpiechowicz/harness-useful-memory/commit/d2d7d6dfe2821530f51ddd421539f374cb9e7019))
* **embedder:** open the lock file read-write so Windows can lock it ([dfdf27a](https://github.com/MSpiechowicz/harness-useful-memory/commit/dfdf27a31780c6de82ec494643891588749582f2))
* **embedder:** review round 1 repairs ([19a8d16](https://github.com/MSpiechowicz/harness-useful-memory/commit/19a8d165e15a57e315c976b6a882a4d68de0d943))
* **embedder:** security review repairs ([96e053a](https://github.com/MSpiechowicz/harness-useful-memory/commit/96e053ad98c059f9279380aab78d4bef11294465))
* **mcp:** note_search asks for specific keywords, not an empty query ([b93001c](https://github.com/MSpiechowicz/harness-useful-memory/commit/b93001c2892aa74b0c8c0f077069c37855d40613))


### Features

* **embedder:** shared local embedder with vector cache and socket ([20c9840](https://github.com/MSpiechowicz/harness-useful-memory/commit/20c9840332f88718a58518d8ac1b7452db72adda))
* **notes:** English notes setting, refusal of non-English agent saves and lang filter ([a30529c](https://github.com/MSpiechowicz/harness-useful-memory/commit/a30529c3b7c0d75926fe11921998c7e8919ead55))
* **search:** hybrid memory search, shared embedder and English notes ([0cf13eb](https://github.com/MSpiechowicz/harness-useful-memory/commit/0cf13eba5beb0c423f893871e13ec9e688448a27))
* **search:** hybrid note_search with the shared embedder ([edc0b0d](https://github.com/MSpiechowicz/harness-useful-memory/commit/edc0b0d7660c6f05cf45ac9f6fe5af1a24828b97))
* **search:** stemmed keyword search, one ranking for repository and global notes ([cea196d](https://github.com/MSpiechowicz/harness-useful-memory/commit/cea196d340ccd9e7eb77329651bb7b03949f7ffa))
* **store:** additive search extensions, note language and store settings ([c0a7e07](https://github.com/MSpiechowicz/harness-useful-memory/commit/c0a7e0772ec54edb269246806380388a6397f8c8))
* **store:** additive search extensions, note language and store settings ([7a3750f](https://github.com/MSpiechowicz/harness-useful-memory/commit/7a3750fdbffea59c2c245112252a22e6a9c15df7))

# [2.7.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.6.1...v2.7.0) (2026-10-21)


### Features

* **guards:** save guards and held proposals for Harness Desktop ([#39](https://github.com/MSpiechowicz/harness-useful-memory/issues/39)) ([20daaf7](https://github.com/MSpiechowicz/harness-useful-memory/commit/20daaf7a0e722565ec91d2ff0b9bdc2e6c19c45a))

## [2.6.1](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.6.0...v2.6.1) (2026-10-21)


### Bug Fixes

* **mcp:** require project_path on saves and make repository errors say how to fix the call ([cad8b90](https://github.com/MSpiechowicz/harness-useful-memory/commit/cad8b900a8b1c1c14c6f19b9951120eb6bc37d4d))

# [2.6.0](https://github.com/MSpiechowicz/harness-useful-memory/compare/v2.5.0...v2.6.0) (2026-10-21)


### Bug Fixes

* **deps:** update rustls-webpki to 0.103.16 and webpki-roots to 1.0.10 for RUSTSEC-2026-0381 and RUSTSEC-2026-0382 ([6044e02](https://github.com/MSpiechowicz/harness-useful-memory/commit/6044e0242013c53c5389327fdfe418e4774738d5))


### Features

* **cli:** filter notes and scripts lists by place and count each place ([67d918a](https://github.com/MSpiechowicz/harness-useful-memory/commit/67d918a6c3c38773d1215a965757b3a8df3b9b26))

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
