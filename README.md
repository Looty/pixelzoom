# Pixel Zoom

A lightweight Windows screen zoom that feels like macOS Cmd+scroll Zoom. Hold a modifier key, scroll, and the screen magnifies smoothly. It runs from the system tray and costs almost nothing when idle.

## Install

1. Download the latest `pixelzoom-<version>-windows-x64.zip` from the repository's Releases page.
2. Unzip it anywhere, for example `%LOCALAPPDATA%\Programs\PixelZoom`.
3. Run `pixelzoom.exe`. A tray icon appears.
4. Optional: open Settings from the tray icon and enable "Start with Windows".

To uninstall: turn off "Start with Windows" in Settings, choose Exit from the tray menu, then delete the folder you unzipped and `%APPDATA%\pixelzoom`.

### Windows SmartScreen

The executable is not code-signed, so Windows may show "Windows protected your PC". Click "More info", then "Run anyway". To check the download is intact, compare its SHA-256 with the one shown on the release page:

```
Get-FileHash .\pixelzoom-<version>-windows-x64.zip -Algorithm SHA256
```

## Usage

- Shortcut + scroll zooms in and out. By default, Ctrl + scroll up zooms in.
- Right-click the tray icon for Settings or Exit. Double-click opens Settings.
- Ctrl+Alt+= and Ctrl+Alt+- zoom from the keyboard.
- Ctrl+Alt+Q quits immediately (emergency exit).

## Settings

- Enable or disable Pixel Zoom.
- Start with Windows.
- Shortcut recorder: click a key box or Record, hold a key and scroll up or down, then release. The direction you record zooms in; the other zooms out. A key is required; Clear wipes it and listens again.
- Screen picker, shown when several monitors are connected.
- Settings are stored in `%APPDATA%\pixelzoom\config.ini`.

## How it works

The screen is captured with desktop duplication and scaled on the GPU with Lanczos3 filtering and ClearType-aware adaptive sharpening, so text stays crisp. HDR content is passed through unchanged. When you are not zooming, the idle cost is effectively zero.

## Known limitations

- While enabled, the chosen modifier + scroll is swallowed in other apps, which breaks their own Ctrl+scroll zoom (browsers, editors). Pick another modifier or disable Pixel Zoom.
- ClearType sharpening assumes RGB-order sub-pixel text rendering, the Windows default.
- Protected content and exclusive-fullscreen applications cannot be captured.
- The executable is unsigned (see SmartScreen above).

## Build from source

Requires Rust, the MSVC Build Tools, and a Windows SDK.

```
cargo build --release
```

The executable is `target\release\pixelzoom.exe`. Run it from a terminal to see log output.

## License

MIT. See [LICENSE](LICENSE).
