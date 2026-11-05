# Orexa Downloader browser extension (Chrome, Edge, Brave)

Sends downloads to Orexa Downloader.

## Install (developer mode)

1. Install and open Orexa Downloader once (this registers the browser connector).
2. In Chrome open `chrome://extensions` (Edge: `edge://extensions`).
3. Turn on **Developer mode**.
4. Choose **Load unpacked** and pick this `extension` folder. After installing Orexa it is in
   `C:\Program Files\Orexa Downloader\extension` (Orexa > Settings > **Open folder** opens it).

The extension has a fixed ID (`eohagpikpfaoeikeifebabmmchkceeaf`), which is the only one the
connector accepts.

## What it does

- **Catches downloads.** When you click a file link and Chrome starts a download, a small
  **Download file info** window opens in the browser (the desktop app does not open). It shows the
  file name, type, size, the site, the folder it will be saved to (with a **Change** button) and
  three choices: **Download now**, **Add to queue**, **Cancel**. Closing the window counts as Cancel.
  Chrome's own copy is paused while you decide and removed afterwards. Orexa gets the link with its
  cookies and referrer, so logins keep working. If Orexa is unavailable, Chrome keeps the download,
  so nothing is lost. The window is always shown.
- **YouTube and Facebook videos are quick.** The window opens at once with a Download button that
  works right away; the real title, picture, qualities and sizes fill in as soon as they are known.
  Orexa starts reading the video the moment it begins to play (the answer is kept for 30 minutes), so
  the details are usually already there when you click.
- **Finds videos you play.** When you play a video or audio on a page, a **Download video** button appears
  on the player and the toolbar icon shows how many files were found. Nothing is listed or checked for
  videos that are only on the page: previews, muted background clips, clips under 15 seconds and live
  streams are ignored. The popup lists what is playing too. HLS (`.m3u8`) and DASH (`.mpd`) streams are
  found when they play (marked HLS or DASH, with the qualities they offer); copy-protected (DRM) streams
  are left out. The button is only for what you are playing now: playing something else replaces it, and
  when the video ends, is closed, or is paused and scrolled out of view, it is forgotten (nothing is
  collected while you browse). **Facebook** (reels, watch and video links) plays videos in small pieces, so for those the
  button downloads from the video's page: Orexa reads the video's real title and qualities and joins the
  pieces, using your Facebook login for videos that need one. Streams are saved as MP4 (Orexa uses its video tools for this). YouTube has its own
  button. Switch it all off with "Find videos on pages" in the popup.
- **Fast downloads.** Orexa fetches big files over several connections at once and can resume them
  after pausing or closing the app. Servers that do not allow this use one connection as before.
- **Torrents.** Clicking a magnet link opens the same window ("Download torrent"), and `.torrent` files
  you download are handled too. Orexa downloads them with its built-in torrent engine and lists them
  under **Torrents**. A finished torrent stops (Orexa does not keep seeding).
- **Right-click menu.** "Download link with Orexa", "Download video or audio with Orexa",
  and on YouTube "Download this video with Orexa".
- **YouTube button.** A "Download with Orexa" button on video pages; it opens the same window so
  you can pick the quality.
- **Popup.** Turn catching on or off, limit it by size, file type or site, see recent links.

## How it talks to Orexa

Chrome starts `orexa-downloader.exe` as a "native messaging host" (registered per user for
Chrome, Edge and Brave) and exchanges JSON over standard input/output. The connector only accepts
`http` and `https` links, only from the extension above, and passes them to the running app
through a folder (`%LOCALAPPDATA%\Orexa\inbox`). Cookies are used for the download but are
never written to disk.

## Tests

`node extension/tests/logic.test.mjs` checks which downloads are captured and the dialog flow.
`node extension/tests/dialog-logic.test.mjs` checks the file type, size and name helpers.
