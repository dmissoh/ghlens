# Releasing ghlens

How to cut a new ghlens release so `brew install dmissoh/ghlens/ghlens` (and
`brew upgrade ghlens`) serve the new version.

## The model: one version, three places that must agree

A release lines the same version number up in three spots:

1. **`Cargo.toml` `version`** (this repo) - compiled into the binary; this is
   what `ghlens --version` prints.
2. **A git tag `vX.Y.Z`** on `dmissoh/ghlens` - GitHub auto-generates a source
   tarball at `https://github.com/dmissoh/ghlens/archive/refs/tags/vX.Y.Z.tar.gz`.
3. **The tap formula's `url` + `sha256`** (in `dmissoh/homebrew-ghlens`) - points
   Homebrew at that tarball and verifies its checksum.

You edit #1. The tap's `release.sh` does #2 and #3.

## Two repos

| repo | local path | role |
|------|-----------|------|
| `dmissoh/ghlens` (public) | `~/dev/workspace/private/ghlens` | the code; where you tag releases |
| `dmissoh/homebrew-ghlens` (public) | `~/dev/workspace/private/homebrew-ghlens` | the Homebrew tap: `Formula/ghlens.rb` + `release.sh` |

Both must stay **public** for anonymous `brew install` to work.

## One-time prerequisites

- `gh` authenticated (`gh auth status`).
- Rust toolchain (`cargo`), for local build/test.
- The tap cloned at `~/dev/workspace/private/homebrew-ghlens` (already set up).

## Release steps

Example below cuts `0.2.0`. Substitute your version everywhere.

### 1. Bump the version and sanity-check (this repo)

Edit `Cargo.toml`:

```toml
version = "0.2.0"
```

Then build and test:

```sh
cd ~/dev/workspace/private/ghlens
cargo test --release
cargo build --release
./target/release/ghlens --version   # should already print the new number
```

**Check:** `--version` prints `ghlens 0.2.0`, tests pass.

### 2. Commit and push main (this repo)

```sh
git add Cargo.toml Cargo.lock src/     # include any code changes in the release
git commit -m "feat: <what changed>; bump to 0.2.0"
git push origin main
```

**Check:** `git status` reads "up to date with origin/main".

> Push main BEFORE the next step. `release.sh` tags whatever is at the tip of
> `main` on GitHub. If you skip this, the tag captures the *old* code and the
> built binary reports the wrong version.

### 3. Cut the release and sync the tap (tap repo)

```sh
cd ~/dev/workspace/private/homebrew-ghlens
./release.sh 0.2.0
```

**Check:** it ends with `done -> ...` and no errors. The tap now has a commit
`ghlens 0.2.0` with the formula pointing at `v0.2.0`.

### 4. Update your machine and verify

```sh
brew update                          # pulls the new formula into your local tap
brew upgrade ghlens                  # (or: brew install dmissoh/ghlens/ghlens if not installed)
rehash                               # zsh: refresh cached command locations
ghlens --version                     # -> ghlens 0.2.0
```

**Check:** `ghlens --version` prints the new number.

## What `release.sh <version>` does

1. Creates release/tag `vX.Y.Z` on `dmissoh/ghlens` from the current `main`
   (`gh release create ... --target main --generate-notes`), unless it already
   exists.
2. Downloads the release source tarball and computes its `sha256`.
3. Rewrites `Formula/ghlens.rb`'s `url` and `sha256` in place (macOS/BSD `sed`).
4. Commits (`ghlens X.Y.Z`) and pushes the tap.

## Gotchas

- **`upgrade` vs `install`.** `brew upgrade` only acts on an already-installed
  formula. If ghlens is not installed, use `brew install dmissoh/ghlens/ghlens`.
- **Stale local tap.** Homebrew reads the formula from its local clone at
  `/opt/homebrew/Library/Taps/dmissoh/homebrew-ghlens`. `release.sh` pushes to
  the remote; run `brew update` to pull it locally before upgrading.
- **`command not found: ghlens` right after install (zsh).** The shell cached
  the old location. Run `rehash` or open a new terminal.
- **Version mismatch.** `Cargo.toml` `version` must equal the release version, or
  `--version` will disagree with the tag. Bump it in step 1.
- **Linux maintainers.** `release.sh` uses BSD `sed -i ''`. On GNU sed use
  `sed -i` (no empty-string argument).

## Rollback a bad release

```sh
# delete the GitHub release + tag
gh release delete vX.Y.Z --repo dmissoh/ghlens --yes
git push origin :refs/tags/vX.Y.Z

# revert the tap formula to the previous release
cd ~/dev/workspace/private/homebrew-ghlens
git revert HEAD        # or reset to the prior "ghlens ..." commit, then push
git push origin main
```

Then re-cut the release once the code is fixed.
