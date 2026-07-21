# ghlens

Interactive TUI for a GitHub repo's event history. Rust/ratatui port of the
`ghevents` (`ghe`) zsh function. Same data: merges `gh api repos/{repo}/activity`
(branch ref-changes + SHAs) with `/events` (PRs, issues, stars, forks, releases,
comments), one glyph + color per event kind. Adds a per-day activity sparkline
over the (filtered) history and a live substring filter.

## Requirements

- Rust + cargo (build only)
- [`gh`](https://cli.github.com) on PATH and authenticated (`gh auth login`). ghlens
  uses your existing `gh` auth for both github.com and GitHub Enterprise. No token wiring.

## Build

```sh
cargo build --release        # -> target/release/ghlens
cargo test --release         # render + filter smoke tests
```

## Install

### Homebrew

Builds from source (needs Rust) and pulls in `gh` as a runtime dependency.
Until a tagged release exists, install the HEAD formula straight from the repo:

```sh
brew install --HEAD https://raw.githubusercontent.com/dmissoh/ghlens/main/Formula/ghlens.rb
```

For the nicer `brew install dmissoh/ghlens/ghlens`, publish `Formula/ghlens.rb`
to a tap repo named `dmissoh/homebrew-ghlens`, then `brew tap dmissoh/ghlens &&
brew install --HEAD ghlens` (drop `--HEAD` once the formula has a tagged `url` + `sha256`).

### From source

```sh
cargo install --path .       # copies to ~/.cargo/bin/ghlens
```

`~/.cargo/bin` is on PATH by default, so `ghlens` then works from any folder. It is a
standalone native binary (not a symlink into this repo), so it keeps working even if
you move or clean the source dir. Re-run after changing the code to refresh the copy.

## Run

Args mirror `ghevents`: `[filter] [repo]`, repo defaults to the current dir.

```sh
ghlens                                   # current repo, no filter
ghlens feature/foo-230626                # current repo, initial filter (branch/actor/detail/number)
ghlens feature/foo cli/cli               # explicit repo as 2nd positional
ghlens -R cli/cli                        # explicit repo, no filter
ghlens -R myorg/app --host ghe.example.com   # GitHub Enterprise
```

Piped (not a terminal) it prints the filtered rows instead of the TUI, so
`ghlens push cli/cli | grep …` / `| fzf` work.

## Keys

### Keyboard

| key | action |
|-----|--------|
| type | filter the active column (see below) |
| Tab / Shift-Tab | switch the active filter column (All → Date → Type → Actor → Detail) |
| ↑ ↓ / PgUp PgDn / Home End | navigate |
| Enter | on a push (▸) row: expand/collapse its commit subjects (the old `-c`) |
| Ctrl-C | copy the selected line to the clipboard (tab-separated; a `↳` row copies the commit) |
| Ctrl-O | open the selected row on GitHub: a `↳` commit row opens that exact commit, a PR/issue row opens it by number, a push/branch row opens the branch (`gh browse`) |
| Esc | clear all filters, or quit when none are set |
| Ctrl-Q | quit |

### Mouse

| action | does |
|--------|------|
| click a row | select it |
| click the `▸`/`▾` arrow | expand/collapse that push |
| double-click a row | same as Enter (expand/collapse a push) |
| click a column header | focus that column's filter |
| drag a `│` column separator | resize that column (Detail flexes to fill) |
| click a sparkline bar | jump to that day's first row |
| scroll wheel | move the selection |

### Activity chart

One bar per day, oldest left to newest right. The block title states the scale
(a full-height bar = the peak day's event count); the axis below the bars labels
the start, middle, and end dates. The selected row's day is highlighted yellow,
and clicking a bar jumps the list to that day.

### Filtering

Each column has its own filter; a row shows only if it matches **all** non-empty
filters. `All` matches the combined haystack (branch, actor, detail, PR head branch,
issue/PR number). The active column is highlighted in the header and named in the
footer, e.g. `filter[Actor]:`. The CLI `filter` arg seeds the `All` filter.

Push rows are marked `▸` (collapsed) / `▾` (expanded); click the arrow (or press
Enter / double-click) to expand. Expanding fetches that push's commits via one
`compare/<before>...<after>` call and splices them in as `↳` rows. That same call
yields the commit count, shown as a dim `(N commits)` decorator on the push row
which then sticks even when collapsed. (GitHub's feeds don't carry the count, so it
only appears once a push has been expanded.) The default view already merges
PRs/issues/stars/etc (the old `-a`), so expanding a push gives the full `gheac` view.

## Glyphs

`↑` push · `✚` create · `✖` delete · `⚡` force-push · `⇄` merge/PR · `✎` comment
· `◆` issue · `★` star · `⑂` fork · `⚑` release

## Not (yet) ported

- Live refresh (`r`). Currently fetches once at startup.

## Releasing

Maintainers: see [RELEASING.md](RELEASING.md) for how to cut a new version and
publish it to the Homebrew tap.
