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

Pushing a tag that starts with `v` builds a Windows x86_64 binary and uploads it
to a GitHub Release.

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin main --tags
```
