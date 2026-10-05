# Releasing

[`.github/workflows/release.yml`](../.github/workflows/release.yml) builds rtp-audio for Linux
and Windows whenever a GitHub release is published, runs the tests, and attaches:

- `rtp-audio-<tag>-x86_64-unknown-linux-gnu.tar.gz` (binary + README)
- `rtp-audio-<tag>-x86_64-pc-windows-msvc.zip`
- a `.sha256` checksum file for each

## Make a release

1. Bump `version` in `Cargo.toml`, then commit and push.
2. Write the release notes in a file, e.g. `notes.md`: what changed, and which download is for
   whom. GitHub's `--generate-notes` only lists merged pull requests, so with direct commits
   it adds nothing but a changelog link.
3. Publish the release:
   ```bash
   gh release create v0.2.0 --title "v0.2.0" --notes-file notes.md
   ```
   Or use **Releases → Draft a new release** on GitHub. A draft doesn't start the build;
   publishing it does. To fix the notes later: `gh release edit v0.2.0 --notes-file notes.md`.
4. Watch the build with `gh run watch`. The files appear on the release after a few minutes.

## Test build without a release

On GitHub open **Actions → Release → Run workflow**. The archives appear under **Artifacts** on
that run's page; nothing is attached to any release.

## If a build fails

Fix it, push, then use **Re-run jobs** on the failed run. Files already attached are replaced.

If GitHub retires the `ubuntu-22.04` runner, change it to `ubuntu-24.04` in the workflow (the
Linux binary then needs a newer glibc).
