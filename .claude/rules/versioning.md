# Development versioning and upstream-tree management

How the gem's version number, the sibling `../pdf_oxide` checkout, and
"exception builds" (patched upstream) relate. Complements
`maintenance.md` (what to port) and `extensions.md` (Ext surface). Where
this file and the "Versioning" section of `extensions.md` disagree, this
file wins: that section describes **pinned mode** only.

## Two modes

The gem is always in exactly one of these modes. The mode is readable from
the version number alone.

| | **Tracking mode** (current) | **Pinned mode** (release-bound) |
|---|---|---|
| Purpose | absorb upstream `main` as fast as possible while private | reproducible builds; prerequisite for any publish |
| Sibling tree | follows upstream `main` (`git pull` allowed) | detached at a **release tag** (`vX.Y.Z`), moved only by a re-sync |
| Gem version | **`0.1.X`** — the `0.1` prefix means "not tied to any upstream release"; bump `X` per change | **`X.Y.Z.N`** = upstream release + build (see `extensions.md`) |
| `PARITY[:commit]` | the upstream commit the port was last reconciled against (may be an unreleased `main` commit) | the release tag's commit |
| crates.io switch | impossible (no published crate matches) | `pdf_oxide = "=X.Y.Z"` |

Rule: **never mix the schemes.** A `0.1.X` version says "floating"; a
four-segment version says "this exact upstream release". Switching modes
is a deliberate step (checklist at the end), not a side effect of a sync.

## Tracking mode rules (now)

- `git pull` the sibling tree from the **real upstream**, not from a fork's
  stale `main`: the checkout's `origin` is our own fork
  (`metheglin/pdf_oxide`), so an `upstream` remote
  (`yfedoseev/pdf_oxide`) is needed for "latest".
- After every pull that changes what we build against, run the two
  detectors in `maintenance.md` (CSV regenerate + python.rs diff) and update
  the Baseline table with the new commit. The Baseline's *version* field
  reads `X.Y.Z+main@<short-commit>` in this mode, e.g. `0.3.77+main@f5e5131`,
  to make the "unreleased" part explicit.
- `PdfDioxide::UPSTREAM_VERSION` (from `pdf_oxide::VERSION`) is NOT a
  drift detector here — upstream bumps `Cargo.toml` only at release time,
  so `main` commits all report the same version. Until the commit guard
  below exists, the Baseline table is the only record of what was built.
- Gem version `0.1.X`: bump `X` whenever the built artifact changes
  meaningfully (Ext addition, upstream pull that changes behaviour, wrapper
  fix). No CHANGELOG obligation per bump in this mode; keep CHANGELOG for
  Ext additions and exception builds only.

## Pinned mode rules (before publishing)

- Two worktrees, so a pull can never move the build:
  ```
  ../pdf_oxide         path dependency; detached HEAD at vX.Y.Z; moved only by a re-sync
  ../pdf_oxide-main    `git worktree add ../pdf_oxide-main main`; the only place `git pull` runs
  ```
- Re-sync targets are **release tags only**. `main` is for previewing what
  is coming, never for building — the eventual `=X.Y.Z` crates.io pin must
  exist.
- Version `X.Y.Z.N` per `extensions.md` (`N` resets to 0 on re-sync).

## Drift guard (planned, not implemented)

`Cargo.lock` does not record a path dependency's commit, so nothing today
detects that the sibling tree moved. Planned: `build.rs` runs
`git -C <path-dep> rev-parse HEAD` and embeds it as
`PdfDioxide::UPSTREAM_COMMIT`; `version_info` reports it; `rake compile`
warns (pinned mode: fails) when it differs from `PARITY[:commit]`. The
guard becomes unnecessary once the dependency is a crates.io pin.

## Exception builds (patched upstream) — the only allowed deviation

Sometimes an upstream bug must be worked around before the fix ships
(precedent: issue #1309, Form XObject `/Resources` reference not resolved
by the renderer). This is a deviation of the **dependency source**, not of
this gem's code, and it is contained by making the build describe itself:

| Where | Value | Why |
|---|---|---|
| fork branch (own fork) | `pdf_dioxide/<base>-fixNNNN`, cut from **exactly the base the gem claims** (the tag in pinned mode; `PARITY[:commit]` in tracking mode) — never from a newer `main` | "base + this one patch", nothing else |
| fork `Cargo.toml` version | `X.Y.Z+fixNNNN` — semver **build metadata**, not a `-` prerelease: workspace members (`pdf_oxide_cli`) require `version = "X.Y.Z"`, which a prerelease does not satisfy but `+meta` does | `UPSTREAM_VERSION` then reports `X.Y.Z+fixNNNN` with zero gem code |
| the patch itself | **opt-in**: a flag on a public options struct (e.g. `RenderOptions.resolve_form_resources`), default off | parity methods keep byte-identical behaviour; only an Ext `experimental_*` method turns the flag on |
| gem version | tracking mode: next `0.1.X`; pinned mode: **prerelease** `X.Y.Z.N.fixNNNN` (RubyGems treats any letter as prerelease → sorts below `X.Y.Z.N`, never auto-selected by Bundler) | the artifact itself says "not a normal release" |
| record | **CHANGELOG entry only** — fork branch, rev, issue link, the Ext method that exposes it | one place; no rule document is touched |
| expiry | dropped at the next sync: the sync always starts from an unpatched base. If upstream still hasn't merged, the exception is **re-declared explicitly** (new branch off the new base, new CHANGELOG entry) — never carried over silently | prevents the exception from becoming the norm |

When the upstream fix lands, the `experimental_*` method graduates per
`extensions.md` (shim → warn → remove).

## Mode transition: tracking → pinned (checklist)

```text
[ ] pick the release tag to pin (normally the latest vX.Y.Z)
[ ] `git checkout vX.Y.Z` in ../pdf_oxide (detached); create ../pdf_oxide-main worktree
[ ] run the maintenance.md re-sync against that tag; update Baseline (version = X.Y.Z, commit = tag commit)
[ ] set VERSION = "X.Y.Z.0"; PARITY = tag; CHANGELOG entry "switched to pinned mode"
[ ] re-cut any live exception branch from the tag (or drop it if merged upstream)
[ ] optionally switch Cargo to `pdf_oxide = "=X.Y.Z"` and confirm `rake install` still works
```
