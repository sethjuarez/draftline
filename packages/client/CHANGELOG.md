# Changelog

## [0.10.0](https://github.com/sethjuarez/draftline/compare/client-v0.9.0...client-v0.10.0) (2026-11-05)


### Features

* **workspace:** rebase local versions onto incoming ones ([#83](https://github.com/sethjuarez/draftline/issues/83)) ([e092d67](https://github.com/sethjuarez/draftline/commit/e092d6760aa62190ccc274bfe04d86fe70ccde2a))

## [0.9.0](https://github.com/sethjuarez/draftline/compare/client-v0.8.0...client-v0.9.0) (2026-11-05)


### Features

* **workspace:** clone without file contents through git (opt-in blobless clone) ([#79](https://github.com/sethjuarez/draftline/issues/79)) ([d010d31](https://github.com/sethjuarez/draftline/commit/d010d31b1a415c46ef973bfcb13f58f7d99adaec))
* **workspace:** create a project folder inside an existing repository ([#77](https://github.com/sethjuarez/draftline/issues/77)) ([f7fe72d](https://github.com/sethjuarez/draftline/commit/f7fe72dca7f4201dc34ab09f81e7c773e55df159))
* **workspace:** inspect a repository before adding a project ([#81](https://github.com/sethjuarez/draftline/issues/81)) ([ac021fd](https://github.com/sethjuarez/draftline/commit/ac021fda899c03f0d789bb3fe37bc2f2cc96b5de))
* **workspace:** open GitHub pull requests through an optional github feature ([#80](https://github.com/sethjuarez/draftline/issues/80)) ([327826f](https://github.com/sethjuarez/draftline/commit/327826f108f883ca5851f467f1f5ae13709cbfe9))
* **workspace:** record project id, base branch and commit template in the marker ([#75](https://github.com/sethjuarez/draftline/issues/75)) ([9485380](https://github.com/sethjuarez/draftline/commit/94853804d4191dd46ce9035016686392f17924de))
* **workspace:** report clone progress and clone a chosen branch ([#78](https://github.com/sethjuarez/draftline/issues/78)) ([b219312](https://github.com/sethjuarez/draftline/commit/b219312d827859da089e2d592a82923a53195adf))


### Bug Fixes

* **workspace:** clone a repository without claiming a project folder ([#82](https://github.com/sethjuarez/draftline/issues/82)) ([4d2afd1](https://github.com/sethjuarez/draftline/commit/4d2afd1b4935b784e54e6e2dadd1154805cb91d0))

## [0.8.0](https://github.com/sethjuarez/draftline/compare/client-v0.7.0...client-v0.8.0) (2026-11-05)


### Features

* **workspace:** read raw file bytes from a version ([#73](https://github.com/sethjuarez/draftline/issues/73)) ([d9b2e1c](https://github.com/sethjuarez/draftline/commit/d9b2e1c29fd5d76f9313f4ec441632d4a70c4258))

## [0.7.0](https://github.com/sethjuarez/draftline/compare/client-v0.6.0...client-v0.7.0) (2026-11-05)


### Features

* **workspace:** local variation merge and CutReady follow-ups ([#70](https://github.com/sethjuarez/draftline/issues/70)) ([077e42b](https://github.com/sethjuarez/draftline/commit/077e42b76a628dde2a58275072e7bf4948ddb3f0))
* **workspace:** pin clean incoming merges to the previewed heads ([#72](https://github.com/sethjuarez/draftline/issues/72)) ([77501f5](https://github.com/sethjuarez/draftline/commit/77501f52edd61da9d8edd9e5c1b18cfdb336ad85))

## [0.6.0](https://github.com/sethjuarez/draftline/compare/client-v0.5.0...client-v0.6.0) (2026-11-04)


### Features

* **workspace:** version comments, per-path history, and path restore ([#68](https://github.com/sethjuarez/draftline/issues/68)) ([ead6615](https://github.com/sethjuarez/draftline/commit/ead6615471cf35331ed1f9801547908778983029))

## [0.5.0](https://github.com/sethjuarez/draftline/compare/client-v0.4.1...client-v0.5.0) (2026-10-28)


### Features

* **workspace:** save, restore, and push Git LFS files through git-lfs ([#60](https://github.com/sethjuarez/draftline/issues/60)) ([038ee88](https://github.com/sethjuarez/draftline/commit/038ee881b55c0e0b7ec4f4c6ced715193073a99f))

## [0.4.1](https://github.com/sethjuarez/draftline/compare/client-v0.4.0...client-v0.4.1) (2026-10-26)


### Bug Fixes

* **workspace:** fix the CutReady 0.7.0 E2E findings ([#57](https://github.com/sethjuarez/draftline/issues/57)) ([fd726a2](https://github.com/sethjuarez/draftline/commit/fd726a2b08112f2842355fda4eac09cafbd4fd3f))

## [0.4.0](https://github.com/sethjuarez/draftline/compare/client-v0.3.0...client-v0.4.0) (2026-10-25)


### ⚠ BREAKING CHANGES

* **workspace:** SendRequest.expected_head and SendPreflight.head are replaced by SendRequest.expected and SendPreflight.token.

### Features

* **workspace:** manage one folder as a teammate in developer repositories ([#54](https://github.com/sethjuarez/draftline/issues/54)) ([b537879](https://github.com/sethjuarez/draftline/commit/b537879f814c07f9eb00b7ff882e7d1ddeb5544c))

## [0.3.0](https://github.com/sethjuarez/draftline/compare/client-v0.2.0...client-v0.3.0) (2026-10-24)


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

## [0.2.0](https://github.com/sethjuarez/draftline/compare/client-v0.1.15...client-v0.2.0) (2026-10-17)


### Features

* **merge:** add additive milestone merge publishing ([7c292a0](https://github.com/sethjuarez/draftline/commit/7c292a07dc4ca610b8b29036c4bd98ee998e6d85))

## [0.1.15](https://github.com/sethjuarez/draftline/compare/client-v0.1.14...client-v0.1.15) (2026-08-27)


### Bug Fixes

* restore main release lineage and package mirrors ([4b60088](https://github.com/sethjuarez/draftline/commit/4b6008826570b40af69707b1fa1c4e55fd62fd7f))
