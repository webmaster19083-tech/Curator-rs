# Curator

A small, self-hosted, site-agnostic front-end for [gallery-dl](https://github.com/mikf/gallery-dl).

Paste in creator URLs from any site gallery-dl supports (DeviantArt, Pixiv,
Twitter/X, ArtStation, Instagram, Reddit, Imgur, hundreds more), Curator
downloads their content locally and gives you a fast browser UI to view it —
by creator, by group, or everything shuffled together — with ratings, tags,
a slideshow, and a few other ways to browse (portrait wall, mobile feed).
An optional local NudeNet classifier can suggest the 1-3 exposure scale; a
separate P-HAR worker can suggest Fast for qualifying short videos. Cum stays
human-confirmed. See `--docs` for setup if you want local auto-rating.
"Live browse" streams a URL straight from gallery-dl's listing output
without downloading anything, for previewing before you commit disk space —
merged in from the separate Contact Sheet project, so it's all one server
now. Real sources get the same treatment automatically: a newly-added
source shows up looking fully downloaded right away — streamed live until
the real download catches up, then swapped to the real file in place, so
any rating/tag you set early sticks. A dismissible sidebar reminder nudges
you to export your source list every so often, since that's the one thing
here that's genuinely hard to recreate if lost.

Curator is a Tauri desktop application backed by one local Rust/Axum + SQLite
service. The same service remains available in a normal browser as a fallback;
everything runs on your machine and nothing is uploaded anywhere.

## Requirements

- Windows, macOS, or Linux
- [gallery-dl](https://github.com/mikf/gallery-dl) itself, available on your
  `PATH` (`pip install gallery-dl`, or see gallery-dl's own install docs) —
  Curator is a front-end for it, not a replacement for it
- Optional: [ffmpeg](https://ffmpeg.org/) on your `PATH` — gallery-dl uses it
  for some sites' video handling

Curator itself needs no Python, no pip, and no separate install step — it's
one binary.

## Setup & run

**Windows:** download the Curator installer (`.msi` or NSIS `.exe`) from the
[Releases page], install it, then open **Curator** from the Start menu. The
desktop app starts its backend and library automatically; no browser, terminal,
or working-directory setup is needed.

**macOS / Linux:** no pre-built binary is published yet — build from source
with [Rust](https://rustup.rs) installed:

```bash
git clone <this repo> curator
cd curator
cargo build --release
./target/release/curator
```

The browser fallback is served at **http://127.0.0.1:42168** while Curator is
running. Closing the desktop window can leave Curator running in the system
tray, so its local browser fallback and downloads continue until you choose
**Quit Curator**.

**First launch:** Curator opens a short local setup wizard instead of the
normal browser UI — it checks for `gallery-dl` (and optionally `ffmpeg`),
lets you confirm or change where your data lives, and sets a few download
and appearance defaults. Nothing you enter leaves your machine, and you're
never asked for a password or site cookies there. Once you finish it (or
choose "Advanced / Skip Setup"), Curator won't show it again — reopen it
any time from **Settings → Run Setup Again**, which only lets you review or
change things; it never touches your downloads, database, or other
settings. Upgrading an existing installation never re-triggers the wizard.
If `gallery-dl` isn't installed yet, the wizard tells you and lets you
either install it and retest, or point Curator at wherever it lives.

Your data (downloads, database, settings, log) lives outside the app's own
folder — in `~/Curator` by default, separate from the code/binary — so
upgrading Curator later is just "replace the binary (and `static/`) and run
it again."

## Everything else

This file stays short on purpose. For the full reference — Tailscale-only
remote access, moving your data directory, groups & tags, ratings, themes,
speeding up downloads, gallery-dl login/cookies, troubleshooting, all of
it — run:

```bash
curator --docs
```

or `curator --help` for just the command-line flags. `--docs` prints a lot
of text; pipe it through a pager if you like (`| more` on Windows, `| less`
on Linux/macOS).

## A quick note on use

Curator downloads whatever you point it at. Only add creators/galleries
you have the right to access, and be mindful of each site's terms of
service and rate limits — gallery-dl is a general-purpose tool, and how you
use it is on you.

[Releases page]: ../../releases
