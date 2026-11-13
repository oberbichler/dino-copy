# Contributing

## Repository layout

| Path | What |
| --- | --- |
| `core/` | `dino-copy-core`, the engine: walk, plan, copy, delete. |
| `src/`, `tests/` | The command line tool and its integration tests. |
| `app/` | DinoCopy: the window (`index.html`, `src/`, TypeScript with Vite) and the Tauri backend (`src-tauri/`, its own Cargo workspace and lockfile). |
| `app/src/i18n/` | Translations. English defines the keys; the type check fails if a language misses one. |
| `app/tests/` | UI tests of the window in every state, language and colour scheme. |
| `app/icon/` | The icon's source (`icon.svg`, rendered by `render.mjs`). |
| `app/packaging/` | The Homebrew cask, filled in by the release. |

## Building and testing

Command line tool and engine (Rust 1.85 or newer):

```sh
cargo test --workspace   # unit, integration and property-based tests
cargo clippy --workspace --all-targets
```

The app (Node.js 22 and Rust 1.88 or newer):

```sh
cd app
npm install
npm run tauri dev        # the app, reloading on changes
npm run typecheck
npm run test:ui          # UI tests in WebKit (once: npx playwright install webkit)
cd src-tauri && cargo test
```

The UI tests replace the backend with a stand-in (`app/tests/tauri.ts`) and
check each state's behaviour and that no text is cut off. With
`SCREENSHOTS=<dir>` they also save a screenshot of every state.

The core guarantees are checked by property-based tests (`core/src/walk_props.rs`):
random source and target trees are mirrored and compared against an
independent model, and the source is swapped for an arbitrary other tree
between the two passes to check that nothing unplanned is ever deleted. Raise
`PROPTEST_CASES` for a deeper search.

## Releasing

The command line tool and the app share one version (`Cargo.toml`,
`core/Cargo.toml`, `app/src-tauri/Cargo.toml`, `app/package.json`; a test
checks that they match). Pushing a tag `vX.Y.Z` runs `release.yml`:
[dist](https://github.com/axodotdev/cargo-dist) builds the command line tool
and updates its formula in
[oberbichler/homebrew-tap](https://github.com/oberbichler/homebrew-tap);
`build-app.yml` builds `DinoCopy.dmg` (universal, signed ad hoc) for the same
GitHub release, and `publish-app.yml` updates the `dinocopy` cask. `release.yml`
is generated: change `dist-workspace.toml` and run `dist generate`.
