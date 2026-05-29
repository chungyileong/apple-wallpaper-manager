# awm

Rust rewrite of the Apple wallpaper downloader with a terminal UI, live download progress bars, and category/subcategory navigation.

## Features

- Browse wallpaper categories and subcategories in a collapsible TUI
- Select nothing by default, then expand categories and choose subcategories to download
- Download multiple assets concurrently with a default of 4 workers
- Show live per-file progress for each active download
- Show an overall queue progress bar
- Save downloads to the same wallpaper storage location the original app uses

## Requirements

- macOS with access to the Apple wallpaper manifest
- Rust toolchain

## Usage

Run it with:

```bash
cargo run
```

You can also choose a destination folder:

```bash
cargo run -- \
  --output ~/Library/Application\ Support/com.apple.wallpaper/aerials/videos
```

You can change the worker count with:

```bash
cargo run -- --threads 6
```

## Controls

- `↑` / `↓` move through the category tree
- `→` expand a category
- `←` collapse a category
- `space` toggle the current category or subcategory
- `a` select all subcategories
- `c` clear selection
- `x` remove the currently selected item from disk
- `d` or `Enter` start downloading
- `q` quit

## Notes

- Downloads are written through temporary `.part` files and renamed into place when complete.
- The default download destination matches the original app's wallpaper storage folder.
- The default download worker count is 4.
- Existing files are skipped when they already match the remote size.
- Downloaded assets are marked in the tree view with `[x]`.
- If the server reports a content length, each active download row shows byte-based completion. If it does not, the app still tracks the transfer and shows the byte count.
