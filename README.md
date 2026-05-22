# mcdougs

Small Rust TUI for browsing live McDougall auction lots.

It filters to Saskatoon by default, hides expired lots unless you ask otherwise, and lets you click a lot row to open it in your browser.

## Run

```bash
cargo run
```

## Plain output

```bash
cargo run -- --plain
```

## Release

Pushing a tag that starts with `v` builds direct Windows and Linux x86_64
binaries and uploads them to a GitHub Release.

```bash
git tag -a v0.1.1 -m "Release v0.1.1"
git push origin main --follow-tags
```

Release assets are named like:

```text
mcdougs-v0.1.1-windows-x86_64.exe
mcdougs-v0.1.1-linux-x86_64
mcdougs-v0.1.1-linux-x86_64.tar.gz
```
