# deskdrop

Copy on one computer, paste on the other: text, rich text, images, and **files/folders**
between macOS and Windows. A companion to [Deskflow](https://github.com/deskflow/deskflow), which
shares the keyboard and mouse but can't move files.

- **Mac → Windows:** ⌘C in Finder, move to Windows, Ctrl+V in Explorer.
- **Windows → Mac:** Ctrl+C in Explorer, move to the Mac, ⌘V in Finder.

## How it works

- A small agent runs on each machine. The Mac listens on TCP 24802 and Windows dials in.
  Traffic is encrypted and authenticated with Noise `NNpsk0` using a shared pairing secret.
- When you copy files, the agent sends the file list right away, and the other side starts
  downloading into a cache (`~/Library/Caches/deskdrop` or `%LOCALAPPDATA%\deskdrop\cache`).
- The other side's clipboard is filled **lazily**: delayed-render `CF_HDROP` on Windows and an
  `NSPasteboardItem` data provider on macOS. If you paste before the download is done, the paste
  waits for it to finish and then copies the files out of the cache.
- Copies over `max_transfer_gb` (16 GB by default, set in `config.json`) are not offered.
  `.DS_Store`, `Thumbs.db`, and `desktop.ini` are skipped. File modification times are kept.

**Turn off Deskflow's own clipboard sharing.** deskdrop replaces it. If both run, Deskflow
overwrites the file clipboard with plain text whenever the cursor crosses screens.

## Setup

```sh
cargo build --release                                  # macOS binary
cargo build --release --target x86_64-pc-windows-gnu   # Windows .exe (brew install mingw-w64)

./target/release/deskdrop init-server --install        # on the Mac; prints the Windows command
deskdrop.exe init-client HOST:24802 SECRET --install   # on Windows
```

`install` registers a LaunchAgent (`com.deskdrop.agent`) on macOS and an HKCU `Run` entry on
Windows, then starts the agent. `uninstall` removes them. Logs are written to `deskdrop.log` in
`~/Library/Application Support/deskdrop` or `%LOCALAPPDATA%\deskdrop`.

## Testing without the other machine

`cargo build --features devpeer` adds `deskdrop devpeer HOST:PORT SECRET`, a headless peer that
reads `text …` / `files PATH…` lines from stdin. `examples/wintest.rs` is a Windows clipboard
probe (`paste`, `copy PATH…`, `text`, `dib`). Together they let you test the Windows agent
under Wine.
