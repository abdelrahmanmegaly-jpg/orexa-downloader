# Changelog

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
