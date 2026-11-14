# Changelog

## 0.2.1 (extension 0.2.1)

The extension's version number moves to 0.2.1 to match the app; the extension itself has no changes in this release.

### Added
- **Change link.** An unfinished download can continue from another address, like a mirror or a fresh link from the website. Press **Change** next to the link in the download's details and paste the new one. Orexa checks it is the same file, then continues from what is already saved.
- **Same file, new link.** When you add a file you already have from another link, Orexa tells you. An unfinished copy can be continued with **Continue it** instead of starting over. A download from your browser continues the unfinished copy by itself.
- **Check file.** Compare a finished file with the checksum on its download page (SHA-256, SHA-512, SHA-1 or MD5) to be sure it is complete and unchanged. Orexa also looks for the checksum on the server by itself.
- **Speed limit.** In Settings, keep all downloads together under a speed you choose, in KB/s or MB/s. It applies to files, YouTube and torrents.
- **Scheduled downloads.** In Settings, queued downloads wait for a start time, for example at night, and can stop at a set time. Orexa has to be running; the tray is enough.
- **Leftovers.** Removing an unfinished download can move its partial file to the Recycle Bin. In Settings, **Look for leftovers** finds partial files of downloads that are no longer in your list.

### Improved
- **Downloads survive a server going down.** When a server stops answering, the download shows **Server not answering** with a countdown and keeps trying for about ten minutes before it gives up. Before, it failed after a few seconds or waited forever.
- **Connections on each row.** A running download shows how many connections it uses.
- **The download window fits what it shows.** It grows and shrinks with its content and stays on screen, so its buttons are never hidden. When the main window is small, the details scroll.
- **Settings text wraps beside its buttons** instead of running under them.

### Fixed
- A download that continued with one connection could end up damaged when the server sent the whole file again instead of the rest.
- A download whose connection was cut before the end could be marked as finished.

### Removed
- Nothing in this release.

## 0.2.0

The browser extension is unchanged (still 0.1.7); this update is for the app.

### Added
- **YouTube playlists.** Paste a playlist link, or a video link that includes `&list=`, into **Add URL**. Orexa lists every video with its length. Tick the ones you want, or use **All** / **None**, then pick a quality: **Best (up to 1080p)**, **720p**, **480p** or **Audio only**. Each video downloads on its own, a few at a time, into a folder named after the playlist. File names are numbered (`001 - …`) so they stay in playlist order. Private and deleted videos are skipped.
- **Only this video.** If you open a single video that belongs to a playlist, one click downloads just that video.

### Improved
- **YouTube downloads retry by themselves.** When YouTube sends an empty or broken stream ("The downloaded file is empty"), Orexa tries again up to three times before showing an error.

### Fixed
- Nothing else in this release.

### Removed
- Nothing in this release.

## 0.1.9 (extension 0.1.7)

### Added
- **Install for "Only me" or "All users".** The installer now asks. **Only me** needs no administrator rights, so Orexa installs on computers without admin access. **All users** installs in Program Files and asks Windows for permission.
- **Browser fallback.** When a site only lets the browser download a file, Orexa hands it back and your browser finishes it.

### Improved
- **Fewer failed downloads on file hosts.** Sites that allow only one or two connections at a time (like pixeldrain) now work. Orexa uses fewer connections and waits when the site asks, instead of failing after six interrupted parts.
- **Fewer "403 Forbidden" errors.** Orexa asks for files the way a browser does. If a site still refuses, it retries with different settings before giving up.
- **Pictures stay in the browser.** PNG, JPG and other images download in the browser as usual. You can turn on **Also catch images** in the extension's options.
- **Clearer error messages** when a site refuses a download, limits downloads, or the file is gone.

### Fixed
- Multi-connection downloads failed with "The connection kept failing" on sites that limit connections.

### Removed
- Nothing in this release.

## 0.1.8

The browser extension is unchanged (still 0.1.6); this update is for the app and installer.

### Added
- **Updates inside the app.** The version number now shows at the bottom of the sidebar, under Settings, with **Up to date** or a blue **Update** badge. Click it to see what is new in the latest release and press **Update now**: Orexa downloads the installer from GitHub, checks it, and updates itself.

### Improved
- After a download finishes, the download window now **closes itself** when you click **Open file** or **Show in folder**.
- When a download finishes, its window **comes to the front** even if you are working in another app.

### Fixed
- Nothing in this release.

### Removed
- Nothing in this release.

## 0.1.7

The browser extension is unchanged (still 0.1.6); this update is for the app and installer.

### Added
- **Chrome Web Store extension support.** The app now works with the extension installed from the Chrome Web Store, and the installer offers it to Chrome, Brave and Edge: the next time you open the browser it asks whether to turn the extension on.
- The installer's last page tells you to expect that prompt.

### Improved
- **The "How to add the extension" guide** now follows what the browser really shows, with screenshots: the Action required button (Chrome and Brave) or the three-dots menu (Edge), then Enable extension or Turn on extension, then pinning it. It also has a button that opens the extension's Chrome Web Store page.
- Settings → Chrome extension now opens the Chrome Web Store.
- Uninstalling removes the extension offer from your browsers.

### Fixed
- Nothing in this release.

### Removed
- Nothing in this release.

## 0.1.6

### Added
- **Arabic language** with a full right-to-left layout and the thmanyah typeface. A language menu sits above Settings in the sidebar, and the app starts in Arabic on first run if Windows is set to Arabic.
- **Download details window** (the info button on any row, or double-click):
  - It shows a progress bar with the percentage, the link with a Copy button, state, file size, how much is downloaded, speed and time left.
  - **See details** reveals a speed graph and an activity log with connections and retries.
- **Download pop-up from the browser:**
  - Choosing **Download now** opens a small Orexa-styled window in the centre of the screen, without opening the main app.
  - It has Pause/Resume, Show in folder, minimize, and **Cancel**, which stops the download so you can resume it later from the app.
  - The tray menu has **Show the current download** to bring it back.
- **Notification buttons:** Open file and Show in folder on the "Download complete" notification.
- **First-run welcome** that offers the extension guide and a language choice.
- **Multi-select:** Ctrl+click and Shift+click, with Pause, Resume and Remove for several downloads at once.
- **Keyboard shortcuts:** Ctrl+N, Ctrl+V, Ctrl+F, Ctrl+A, Space, Enter, Delete, Esc, arrow keys and F1.
- **When all downloads finish:** open the folder, or put the PC to sleep after a 30-second countdown.
- **Copy diagnostics** and **Report a problem** in Settings. Diagnostics contain no links or file names.
- **Torrent control:**
  - Choose files before starting, and change them while a torrent runs.
  - A torrent details window shows peers, speeds and files.
  - Optionally keep sharing finished torrents, with a Stop button.
  - Drop a `.torrent` file on the window to add it.
- **Smarter saving:**
  - Duplicate file names get a number instead of overwriting.
  - A free-space check before starting, and a warning for links already in your list.
  - Optional sorting into Video, Music, Documents and so on, in the app and in the browser extension's window.
- **Windows integration:** open magnet links and `.torrent` files with Orexa, with a shortcut to Windows' Default apps, and a Start with Windows toggle.

### Improved
- The extension's download window now follows your type-folder setting.
- Settings has new grouped cards for the options above.
- Torrent rows are labelled "Torrent" and show sharing state and upload speed.

### Fixed
- Unticked torrent files no longer leave large empty placeholder files behind.
- Multi-part downloads restore their progress correctly after restarting the app.

### Removed
- Nothing in this release.

## 0.1.5

First public release.
