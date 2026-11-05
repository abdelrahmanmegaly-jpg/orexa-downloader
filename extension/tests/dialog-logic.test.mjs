// Run with:  node extension/tests/dialog-logic.test.mjs
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const L = require("../dialog-logic.js");

let n = 0;
const test = (name, fn) => {
  fn();
  n++;
  console.log("ok -", name);
};

test("sorts files into the same categories as the app", () => {
  assert.equal(L.categoryOf("disk.ISO").label, "Compressed");
  assert.equal(L.categoryOf("report.pdf").label, "Documents");
  assert.equal(L.categoryOf("song.mp3").label, "Music");
  assert.equal(L.categoryOf("setup.exe").label, "Programs");
  assert.equal(L.categoryOf("clip.mp4").label, "Video");
  assert.equal(L.categoryOf("data.bin").label, "Other");
  assert.equal(L.categoryOf("noext").icon, "file");
});
test("formats sizes like the app", () => {
  assert.equal(L.fmtSize(0), "");
  assert.equal(L.fmtSize(-1), "");
  assert.equal(L.fmtSize(512), "512 B");
  assert.equal(L.fmtSize(412 * 1024), "412 KB");
  assert.equal(L.fmtSize(10 * 1048576), "10.0 MB");
  assert.equal(L.fmtSize(5 * 1073741824), "5.0 GB");
});
test("formats video length", () => {
  assert.equal(L.fmtClock(75), "1:15");
  assert.equal(L.fmtClock(1123), "18:43");
  assert.equal(L.fmtClock(3725), "1:02:05");
});
test("finds a file name in a plain link", () => {
  assert.equal(L.nameFromUrl("https://example.com/files/My%20File.zip?x=1"), "My File.zip");
  assert.equal(L.nameFromUrl("https://example.com/"), "download");
});
test("finds the real file name in a signed link (like GitHub's)", () => {
  const u =
    "https://release-assets.githubusercontent.com/github-production-release-asset/2810292/06f2f8e4?sp=r&rscd=attachment%3B+filename%3Drufus-4.15.exe&rsct=application%2Foctet-stream";
  assert.equal(L.nameFromUrl(u), "rufus-4.15.exe");
  const u2 = "https://x.com/a?response-content-disposition=attachment%3B%20filename%3Drufus-4.15.exe";
  assert.equal(L.nameFromUrl(u2), "rufus-4.15.exe");
});
test("keeps Arabic names and removes characters Windows cannot use", () => {
  assert.equal(L.cleanName("محاضرة عن البرمجة.mp4"), "محاضرة عن البرمجة.mp4");
  assert.equal(L.cleanName('a:b*c?"d<e>f|g.txt'), "a_b_c__d_e_f_g.txt");
  assert.equal(L.cleanName("C:\\Users\\me\\x.zip"), "x.zip");
});
test("shows the site without www", () => {
  assert.equal(L.hostOf("https://www.youtube.com/watch?v=1"), "youtube.com");
  assert.equal(L.hostOf("nonsense"), "");
});

test("finds the YouTube video id in the usual links", () => {
  for (const u of [
    "https://www.youtube.com/watch?v=5M0xujy6aZM",
    "https://www.youtube.com/watch?list=PL1&v=5M0xujy6aZM&t=3",
    "https://youtu.be/5M0xujy6aZM?si=x",
    "https://www.youtube.com/shorts/5M0xujy6aZM",
  ]) assert.equal(L.youtubeId(u), "5M0xujy6aZM", u);
  assert.equal(L.youtubeId("https://www.youtube.com/feed/subscriptions"), "");
  assert.equal(L.youtubeId("not a link"), "");
});
test("ready-made quality buttons, and keeping the pick once the video is read", () => {
  const withF = L.defaultChoices(true);
  assert.deepEqual(withF.map((c) => c.label), ["1080p", "720p", "480p", "360p", "Audio only"]);
  assert.equal(L.defaultChoices(false)[0].label, "Best available");
  const real = [{ height: 1080, audio: false }, { height: 720, audio: false }, { height: 0, audio: true }];
  assert.equal(L.keepChoice({ height: 720, audio: false }, real, 0), 1);
  assert.equal(L.keepChoice({ height: 360, audio: false }, real, 0), 0, "falls back to the default");
  assert.equal(L.keepChoice({ height: 0, audio: true }, real, 0), 2);
});
test("stem of a file name", () => {
  assert.equal(L.stemOf("My Show. Part 2.mp4"), "My Show. Part 2");
  assert.equal(L.stemOf("noext"), "noext");
});

test("torrents: magnet names, type and detection", () => {
  assert.equal(L.nameFromUrl("magnet:?xt=urn:btih:abc&dn=Big+Buck%20Bunny&tr=x"), "Big Buck Bunny");
  assert.equal(L.nameFromUrl("magnet:?xt=urn:btih:abc"), "Torrent");
  assert.equal(L.categoryOf("ubuntu.torrent").label, "Torrent");
  assert.equal(L.isTorrent("magnet:?xt=urn:btih:abc", ""), true);
  assert.equal(L.isTorrent("https://x.org/dl/a.torrent?t=1", ""), true);
  assert.equal(L.isTorrent("https://x.org/get?id=1", "a.torrent"), true);
  assert.equal(L.isTorrent("https://x.org/a.zip", "a.zip"), false);
});

console.log(`\n${n} tests passed`);
