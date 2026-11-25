# Privacy Policy — Taskbar Audio

**Effective date:** 2026-11-25

Taskbar Audio is a local desktop utility for Windows. Your privacy is simple: **we do not collect, store, transmit, or sell any personal data.**

## What the app accesses locally
- **Windows Core Audio (WASAPI):** reads your active audio output/input devices, master volume, mute state, and active application audio sessions strictly on your PC to display and control volume.
- **Local settings & diagnostics:** stores your preferences (such as language choice) locally on your device. If you choose **Copy diagnostics**, a local log file (`%LOCALAPPDATA%\taskbar-audio\diagnostics.log`) is copied to your clipboard only when you explicitly click that option. Nothing is sent automatically.

## Network access
- **Microsoft Store (MSIX) version:** makes **zero** network requests. Updates are delivered exclusively by the Microsoft Store.
- **Standalone (GitHub) version:** only connects to `api.github.com` / `github.com` over HTTPS when you explicitly click **Check for updates**, to check for a newer release and verify its SHA-256 checksum. No telemetry, analytics, or crash reports are ever sent.

## Contact
If you have any questions about this policy, please open an issue at:
https://github.com/vinny/windows-taskbar-audio/issues
