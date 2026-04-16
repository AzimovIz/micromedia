# MicroMedia

A portable media manager for organizing, tagging, and viewing images and videos. Built as a single self-contained binary — just drop it in a folder with your media and go.

## Features

- **Gallery view** — thumbnail grid with adjustable columns
- **Image viewer** — zoom, pan, fit-to-width
- **Video player** — integrated playback via libmpv (dynamically loaded, no system install required)
  - Play/pause, seek, volume, +/-10s skip
  - Fullscreen mode with auto-hiding overlay controls
  - Set any video frame as custom thumbnail
  - Read-ahead buffering for smooth playback on slow storage
- **Tag system** — create tags, assign to files, filter by tags (AND/OR)
- **Search & filter** — search by filename, filter by file extension
- **Sorting** — by name, size, type, or date added
- **Background scanning** — media library is indexed in the background, UI is available instantly
- **Portable** — all data (database, thumbnails, config) stored next to the binary in `appdata/`
- **Cross-compatible** — automatic GPU renderer fallback (Vulkan → OpenGL → glow)

## Project Structure

```
micromedia          # the binary
media/              # put your media files here
appdata/
  micromedia.db     # SQLite database
  thumbnails/       # generated thumbnails
  libs/             # place libmpv.so / mpv-2.dll here (optional)
  config.toml       # auto-generated config
```

## Building

```bash
# Standard build
cargo build --release

# Portable Linux build (compatible with glibc 2.17+)
cargo zigbuild --target x86_64-unknown-linux-gnu.2.17 --release
```

Requires: Rust toolchain. For zigbuild: `zig` + `cargo install cargo-zigbuild`.

## Video Playback

Video playback requires libmpv. Place the library in `appdata/libs/`:

- **Linux**: `libmpv.so`
- **Windows**: `mpv-2.dll`

Thumbnail generation for videos requires `ffmpeg` in PATH or in `appdata/libs/`.

The application works without libmpv — you just won't be able to play videos.

## Built with Claude

This project was almost entirely written by [Claude](https://claude.ai) (Anthropic's AI assistant) via Claude Code, in collaboration with a human developer who provided direction, testing, and feedback. From architecture to implementation — the Rust code, egui UI, libmpv integration, SQLite schema, background scanning, and renderer fallback chain were all authored by Claude.
