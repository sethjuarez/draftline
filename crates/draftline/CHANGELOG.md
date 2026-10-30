# Changelog

## [0.10.2](https://github.com/sethjuarez/draftline/compare/draftline-v0.10.1...draftline-v0.10.2) (2026-10-30)


### Bug Fixes

* **workspace:** wait out a running operation instead of reporting recovery_required ([#66](https://github.com/sethjuarez/draftline/issues/66)) ([66b9fd0](https://github.com/sethjuarez/draftline/commit/66b9fd0ff8c1db7056acc923043b6145ef1d78bb))

## [0.10.1](https://github.com/sethjuarez/draftline/compare/draftline-v0.10.0...draftline-v0.10.1) (2026-10-29)


### Bug Fixes

* **workspace:** offer the folder a developer moved the root's content into ([#64](https://github.com/sethjuarez/draftline/issues/64)) ([62bb5f5](https://github.com/sethjuarez/draftline/commit/62bb5f5e3aa80b37c0250febaed6d5a1b903584b))

## [0.10.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.9.0...draftline-v0.10.0) (2026-10-28)


### Features

* add crash points for recovery testing and timeouts for Git LFS ([#62](https://github.com/sethjuarez/draftline/issues/62)) ([20e4860](https://github.com/sethjuarez/draftline/commit/20e4860fb0ef8005682b2e9d20ceaba921539033))

## [0.9.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.8.0...draftline-v0.9.0) (2026-10-28)


### Features

* **workspace:** save, restore, and push Git LFS files through git-lfs ([#60](https://github.com/sethjuarez/draftline/issues/60)) ([038ee88](https://github.com/sethjuarez/draftline/commit/038ee881b55c0e0b7ec4f4c6ced715193073a99f))

## [0.8.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.7.0...draftline-v0.8.0) (2026-10-26)


### ⚠ BREAKING CHANGES

* **workspace:** match DraftlineError with a wildcard arm.

### Features

* **workspace:** mark DraftlineError non_exhaustive ([#59](https://github.com/sethjuarez/draftline/issues/59)) ([4dbdf8d](https://github.com/sethjuarez/draftline/commit/4dbdf8de02a3bc985b2d5b74bfe1d8dee2093053))


### Bug Fixes

* **workspace:** fix the CutReady 0.7.0 E2E findings ([#57](https://github.com/sethjuarez/draftline/issues/57)) ([fd726a2](https://github.com/sethjuarez/draftline/commit/fd726a2b08112f2842355fda4eac09cafbd4fd3f))

## [0.7.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.6.0...draftline-v0.7.0) (2026-10-25)


### ⚠ BREAKING CHANGES

* **workspace:** SendRequest.expected_head and SendPreflight.head are replaced by SendRequest.expected and SendPreflight.token.

### Features

* **workspace:** manage one folder as a teammate in developer repositories ([#54](https://github.com/sethjuarez/draftline/issues/54)) ([b537879](https://github.com/sethjuarez/draftline/commit/b537879f814c07f9eb00b7ff882e7d1ddeb5544c))

## [0.6.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.5.0...draftline-v0.6.0) (2026-10-24)


### ⚠ BREAKING CHANGES

* **workspace:** remove features that rewrite shared history
* **workspace:** upgrade git2 to 0.21 and send only to bases the remote has
* **workspace:** hold one Draftline workspace per checkout

### Features

* **tauri:** apply a variation namespace to contract commands ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** add optional owned worktrees for separate copies ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** expose the developer's Git identity ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** hold one Draftline workspace per checkout ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** limit history to the project folder and filter variations by namespace ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** merge updates without touching outside code and bring in the latest base ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** remove features that rewrite shared history ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** scope workspaces to a project folder inside an existing repo ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** send pushes the current branch and opens a pull request ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** tie recovery records to their project ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))


### Bug Fixes

* **workspace:** fully roll back or finish interrupted switches and targeted restores ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** keep namespaced workspaces off developer branches ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** make saves atomic and refuse while Git holds the index ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** never consume the developer's index or overwrite local edits ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** never write LFS pointers over real files ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** upgrade git2 to 0.21 and send only to bases the remote has ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))
* **workspace:** wait out Git's locks and make late locks recoverable ([b2bd27e](https://github.com/sethjuarez/draftline/commit/b2bd27e52e7c36310871afba73f75418afbd8a9a))

## [0.5.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.4.0...draftline-v0.5.0) (2026-10-17)


### Features

* **merge:** add additive milestone merge publishing ([7c292a0](https://github.com/sethjuarez/draftline/commit/7c292a07dc4ca610b8b29036c4bd98ee998e6d85))

## [0.4.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.3.0...draftline-v0.4.0) (2026-10-17)


### Features

* **workspace:** honor contributor profiles in every commit-creating operation ([#45](https://github.com/sethjuarez/draftline/issues/45)) ([e41294b](https://github.com/sethjuarez/draftline/commit/e41294b401ee454f14e2332cc11d46b628f137d8))

## [0.3.0](https://github.com/sethjuarez/draftline/compare/draftline-v0.2.17...draftline-v0.3.0) (2026-08-27)


### Features

* **merge:** let hosts inject semantic merge resolvers ([3715c08](https://github.com/sethjuarez/draftline/commit/3715c0895e0135ca4c285e21312b39cf355fb7f7))
* **merge:** let hosts inject semantic merge resolvers ([998f73c](https://github.com/sethjuarez/draftline/commit/998f73cf06680ed8165f0d843b86edb48657dea7))
* UI primitives and graph APIs for Tauri host apps (v0.1.5) ([cb67896](https://github.com/sethjuarez/draftline/commit/cb6789666acc2bcfb81785faf6b3efb46b9dc9d0))
* UI primitives and graph APIs for Tauri host apps (v0.1.5) ([c7db721](https://github.com/sethjuarez/draftline/commit/c7db7215b44de3982342ca73e90362a91a74eb94))


### Bug Fixes

* address rubber duck review findings ([295da6b](https://github.com/sethjuarez/draftline/commit/295da6bcad6b84e5c61e3593e4dd094bf4e16c30))
* enforce full 40-char hex in VersionId::from_canonical_string (v0.1.7) ([3bfa1ab](https://github.com/sethjuarez/draftline/commit/3bfa1abbc9b715af1d0f698dbcc90c084f6fba4b))
* enforce full 40-char hex in VersionId::from_canonical_string (v0.1.7) ([7368efc](https://github.com/sethjuarez/draftline/commit/7368efc8d156b42e5e204cb7b8a45fa1a74bd187))
* propagate cleanup remote credentials ([c500c71](https://github.com/sethjuarez/draftline/commit/c500c71446752b1cda06bf77c3d2bbb842cca943))
* restore main release lineage and package mirrors ([4b60088](https://github.com/sethjuarez/draftline/commit/4b6008826570b40af69707b1fa1c4e55fd62fd7f))
* restore pending cleanup compatibility ([c7e6de3](https://github.com/sethjuarez/draftline/commit/c7e6de36cefe7697e61b256afa90aab9ac7c734c))
