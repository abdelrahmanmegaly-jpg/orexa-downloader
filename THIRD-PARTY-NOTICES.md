# Third-party notices

Orexa Downloader is built on, or works together with, the following open-source projects. Each is
used under its own license.

## Used in the app

| Project | Used for | License |
|---|---|---|
| [egui / eframe](https://github.com/emilk/egui) | The app's user interface | MIT or Apache-2.0 |
| [librqbit](https://github.com/ikatson/rqbit) | The torrent engine | Apache-2.0 |
| [reqwest](https://github.com/seanmonstar/reqwest), [tokio](https://tokio.rs) | Networking and async runtime | MIT or Apache-2.0 |
| [resvg](https://github.com/RazrFalcon/resvg) | Drawing the logo sharply | MPL-2.0 |
| [tray-icon](https://github.com/tauri-apps/tray-icon), [tauri-winrt-notification](https://github.com/tauri-apps/winrt-notification) | Tray icon, Windows notifications | MIT or Apache-2.0 |
| [arboard](https://crates.io/crates/arboard), [rfd](https://crates.io/crates/rfd) | Clipboard, folder pickers | MIT or Apache-2.0 |
| Other Rust crates | See `app/Cargo.toml` and `app/Cargo.lock` | MIT / Apache-2.0 / similar |

## Fetched at run time (not included in this repository or the installer)

| Project | Used for | License |
|---|---|---|
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | Reading and downloading videos | Unlicense |
| [FFmpeg](https://ffmpeg.org) (builds from [yt-dlp/FFmpeg-Builds](https://github.com/yt-dlp/FFmpeg-Builds)) | Joining video and audio | GPL / LGPL |

These tools are downloaded from their official release pages onto the user's own computer the first
time they are needed, and are checked against the published checksums when those are available.

## Fonts

[Plus Jakarta Sans](https://github.com/tokotype/PlusJakartaSans), by Tokotype, under the
[SIL Open Font License 1.1](https://openfontlicense.org). Included in `app/assets/`, `extension/fonts/`
and `website/assets/fonts/`.

## Icons

Interface icons are from [Lucide](https://lucide.dev) (ISC license).
