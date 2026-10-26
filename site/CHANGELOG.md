# Changelog

## [0.3.1](https://github.com/sethjuarez/draftline/compare/site-v0.3.0...site-v0.3.1) (2026-10-26)


### Bug Fixes

* **workspace:** fix the CutReady 0.7.0 E2E findings ([#57](https://github.com/sethjuarez/draftline/issues/57)) ([fd726a2](https://github.com/sethjuarez/draftline/commit/fd726a2b08112f2842355fda4eac09cafbd4fd3f))

## [0.3.0](https://github.com/sethjuarez/draftline/compare/site-v0.2.0...site-v0.3.0) (2026-10-25)


### ⚠ BREAKING CHANGES

* **workspace:** SendRequest.expected_head and SendPreflight.head are replaced by SendRequest.expected and SendPreflight.token.

### Features

* **workspace:** manage one folder as a teammate in developer repositories ([#54](https://github.com/sethjuarez/draftline/issues/54)) ([b537879](https://github.com/sethjuarez/draftline/commit/b537879f814c07f9eb00b7ff882e7d1ddeb5544c))

## [0.2.0](https://github.com/sethjuarez/draftline/compare/site-v0.1.0...site-v0.2.0) (2026-10-24)


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
