# deskdrop

Copy on one computer, paste on the other: **files, folders, text, rich text, and images**
between macOS and Windows.

Built as a companion to [Deskflow](https://github.com/deskflow/deskflow) (and Synergy, Barrier,
Input Leap), which share one keyboard and mouse across machines but can't move files. It also
works fine without them.

- **Mac → Windows:** ⌘C in Finder, move to Windows, Ctrl+V in Explorer.
- **Windows → Mac:** Ctrl+C in Explorer, move to the Mac, ⌘V in Finder.

No cloud, no account. Traffic stays on your LAN, encrypted end to end.

## Install

Grab the binaries from the [latest release](https://github.com/dunstorm/deskdrop/releases/latest).
One machine is the **server**; the other connects to it. It doesn't matter which, but the Mac is
the simpler choice.

**On the Mac (server):**

```sh
curl -fsSL https://github.com/dunstorm/deskdrop/releases/latest/download/deskdrop-macos-universal.tar.gz | tar xz
./deskdrop init-server --install
```

This prints a pairing command with your Mac's address and a fresh secret. `--install` registers a
LaunchAgent, so deskdrop starts at login, and starts it now.

**On Windows (client),** in PowerShell:

```powershell
iwr https://github.com/dunstorm/deskdrop/releases/latest/download/deskdrop-windows-x64.exe -OutFile deskdrop.exe
.\deskdrop.exe init-client "192.168.1.20:24802,my-mac.local:24802" <secret> --install
```

Paste in the command the Mac printed. Use `deskdrop-windows-arm64.exe` on ARM PCs.
`--install` copies the program to `%LOCALAPPDATA%\deskdrop`, adds it to startup, and starts it.

That's it. Copy something and paste on the other side.

> [!IMPORTANT]
> If you use Deskflow, Synergy, or Barrier, **turn off its clipboard sharing.** deskdrop does
> everything those tools' clipboard sharing did. If both run, they overwrite each other's
> clipboard, and Deskflow replaces the copied files with plain text when the cursor crosses
> screens.

### Uninstall

```sh
deskdrop uninstall        # or deskdrop.exe uninstall on Windows
```

## How it works

- A small agent (a few MB, no runtime dependencies) runs on each machine. The server listens on TCP
  **24802**, and the client dials it, trying each address in turn and reconnecting on its own.
- When you copy files, only the file list goes to the other machine. Nothing is downloaded
  until you **paste**. On Windows the files go on the clipboard as *virtual files*
  (`FileGroupDescriptorW` + `FileContents`, as Remote Desktop does), so Explorer can list them
  without fetching anything. Pasting streams them in under Explorer's own copy dialog. On macOS
  an `NSPasteboardItem` data provider does the same for Finder. Data lands in a cache
  (`~/Library/Caches/deskdrop` or `%LOCALAPPDATA%\deskdrop\cache`) on the way. A newer copy
  cancels an unfinished download.
- Folders (including empty ones), Unicode names, and modification times are kept.
  `.DS_Store`, `Thumbs.db`, and `desktop.ini` are skipped.
- Pastes that take more than a moment show a **progress panel** with speed and time left and a
  **Cancel** button. On the Mac it floats in the top-right corner, in both directions. On Windows
  it sits above the taskbar clock while sending; when receiving, Explorer's own copy dialog
  does that job, and its Cancel stops the transfer too.
- Text is converted between `\n` and `\r\n`. Images travel as PNG, with `CF_DIB` added on
  Windows for older apps. Rich text travels as HTML.

### Security

The connection uses the [Noise](https://noiseprotocol.org) pattern `NNpsk0` (X25519,
ChaCha20-Poly1305, BLAKE2s), keyed by the pairing secret. Without the secret a peer can't
complete the handshake, read traffic, or inject clipboard data. Anyone who has the secret can
read your clipboard, so treat it like a password. Run `init-server` on a fresh config to make a
new one. Incoming file paths are sanitized and confined to the cache directory.

## Configuration

`config.json` lives in `~/Library/Application Support/deskdrop/` or `%LOCALAPPDATA%\deskdrop\`:

```json
{
  "secret": "…",
  "listen": "0.0.0.0:24802",
  "connect": [],
  "max_transfer_gb": 16.0
}
```

A server sets `listen`; a client sets `connect` (a list of `host:port`). Copies larger than
`max_transfer_gb` aren't offered, to protect disk space. Restart the agent after editing
(`deskdrop install` again does this).

Logs are written to `deskdrop.log` in the same folder.

## Troubleshooting

- **Nothing pastes.** Check the log on both sides for `connected to …`. If the client logs
  `handshake failed`, the secrets don't match.
- **The client can't reach the server.** With the macOS firewall on, allow incoming connections
  for `deskdrop`. With Windows as the server, allow TCP 24802 in Windows Defender Firewall.
- **Big copies paste slowly.** The paste waits for the download to finish. Transfers run at LAN
  speed; the progress panel shows how long is left, and **Cancel** stops it.
- **"Windows protected your PC" or a Gatekeeper warning.** Release binaries aren't code-signed
  yet. Downloading with `iwr` or `curl` as shown above avoids the warning. Otherwise choose
  *More info → Run anyway*, or run `xattr -d com.apple.quarantine deskdrop`.

## Limitations

- macOS and Windows only, one pair of machines at a time.
- Copy and paste only. Dragging across the screen edge isn't supported.
- On Windows, remote files paste into Explorer, the desktop, and file dialogs. Apps that only
  accept real file paths (some upload boxes) don't see them; paste into a folder first.
- Clipboard managers that read every copy (such as Ditto or Maccy) make files download as
  soon as they're copied, because they ask for the data the way a paste does.

## Building from source

```sh
cargo build --release                                   # native
cargo build --release --target x86_64-pc-windows-gnu    # Windows from macOS (brew install mingw-w64)
```

For testing without a second machine, `cargo build --features devpeer` adds
`deskdrop devpeer HOST:PORT SECRET`. This is a headless peer that reads `text …` and
`files PATH…` lines from stdin. `examples/wintest.rs` is a Windows clipboard probe; together
they let you exercise the Windows agent under Wine.

## License

MIT
