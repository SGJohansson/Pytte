# Pytte

A multi-tab plaintext editor for Windows where two things come before everything else: **speed** and **never losing a single keystroke**. Open a 1.5 GB file and the tab is ready before you have let go of the mouse. Kill the process, pull the plug or let Windows Update restart the machine, and every tab comes back exactly where you left it, unsaved work included.

I built Pytte because Linux is well served by fast editors and Windows mostly is not. The rule from day one: the UI thread never waits for the disk, and nothing that slows down the core gets in until the core is proven.

> **Status:** early prototype (v0.2). Windows 10/11 only. The interface is in Swedish.

## Features

- **Instant open, whatever the size** — files are memory-mapped, not read. Opening runs on a background thread; the tab appears at once and fills in when ready, and costs the same for 2 KB and 2 GB.
- **Line numbers without the wait** — a line index is counted on its own thread, 64 KiB at a time, and published lock-free; the first MiB is counted before the tab is shown, so ordinary files have exact numbers on the first frame. A gigabyte is indexed in a fraction of a second; until then the gutter shows `~` and the status bar the progress. The UI thread never scans the file, and the gutter only ever grows, so the layout never jumps.
- **Search** — `Ctrl+F` searches as you type, on a worker thread that can be cancelled, with matches streamed back and highlighted on screen. SIMD substring search; case-insensitive mode folds ASCII and Latin-1 (`åäö`, `é`, `ü`). Matches that straddle edits are found too.
- **Line operations** — duplicate, move and delete whole lines; each is a single undo step.
- **Status bar** — saved/changed state, line and column, line count, selection size, line endings and file size. Segments shorten or drop out in order of importance rather than overlap; nothing is ever drawn on top of text.
- **Crash-proof sessions** — every edit is appended to a per-tab write-ahead log on its own thread, written through to disk, with a CRC on every record. Close the window and nothing is asked: tabs, cursor positions and unsaved text are restored on the next start. A torn last record after a power cut is detected and cut off.
- **Safe saves** — written to a temporary file, flushed, then swapped in with `ReplaceFileW`, so the file on disk is always either the old or the new version, never half of each.
- **Tabs** — open as many as you like; switching is O(1) and only the visible lines are ever drawn. Opening a file that is already open just switches to it.
- **UTF-8** — `åäö`, `€`, `§` and emoji type and render correctly; the cursor and `Backspace` move one whole character at a time, so a character can never be split. Invalid bytes are shown as `·` and left untouched. CRLF and LF are detected per file and kept.
- **Selection, clipboard, undo** — `Shift` with any movement key, mouse drag, `Ctrl+A`; cut, copy and paste; undo and redo grouped by word. Undo keeps references to the removed text rather than copies, so undoing a `Ctrl+A`, `Delete` on a huge file costs next to nothing.
- **One instance owns the session** — running `pytte.exe file.txt` while Pytte is open hands the file to the running window as a new tab.
- **VoidFlow look** — after [voidflow.tech](https://voidflow.tech/): near-black background, phosphor-green cursor and active tab, a crimson edge, scanlines on the tab bar only, JetBrains Mono (falls back to Consolas).

## Keyboard shortcuts

**Files and tabs**

| Shortcut | Action |
|---|---|
| `Ctrl+O` | Open (several files at once) |
| `Ctrl+S` | Save (asks for a name if the tab is untitled) |
| `Ctrl+Shift+S` | Save as |
| `Ctrl+N` | New tab |
| `Ctrl+W` / `Ctrl+F4` / middle-click a tab | Close tab (asks only if there are unsaved changes) |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| Close the window | Nothing is asked; the session is restored on the next start |

**Moving** (add `Shift` to select)

| Shortcut | Action |
|---|---|
| `←` `→` / `Ctrl+←` `Ctrl+→` | Character / word |
| `↑` `↓` · `PgUp` `PgDn` | Line · page |
| `Home` `End` / `Ctrl+Home` `Ctrl+End` | Start/end of line / of file |
| Click · drag · `Shift`+click | Place the cursor · select · extend the selection |
| Mouse wheel | Scroll |
| `Ctrl+A` · `Esc` | Select all · clear the selection |
| `Ctrl+G` | Go to line |

**Editing**

| Shortcut | Action |
|---|---|
| `Backspace` / `Delete` | Delete a character (or the selection) |
| `Ctrl+Backspace` / `Ctrl+Delete` | Delete a word |
| `Ctrl+C` (`Ctrl+Insert`) · `Ctrl+X` · `Ctrl+V` | Copy · cut · paste |
| `Ctrl+Z` · `Ctrl+Y` (`Ctrl+Shift+Z`) | Undo · redo |
| `Ctrl+D` | Duplicate line(s) |
| `Alt+↑` / `Alt+↓` | Move line(s) up / down |
| `Ctrl+Shift+K` | Delete line(s) |

**Search**

| Shortcut | Action |
|---|---|
| `Ctrl+F` | Open the search bar (a short selection is filled in) |
| `Enter` / `F3` · `Shift+Enter` / `Shift+F3` | Next · previous match, wrapping around |
| `Alt+C` | Toggle case sensitivity |
| `Esc` | Close the search bar |

## Installation

### Download

Pre-built binaries are published on the [Releases](https://github.com/SGJohansson/Pytte/releases) page. Download `pytte.exe` and run it: no installer, no dependencies.

### Build from source

Requires the Rust toolchain and the MSVC build tools:

```powershell
winget install Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

git clone https://github.com/SGJohansson/Pytte.git
cd Pytte
cargo build --release
.\target\release\pytte.exe
```

## Data and durability

The session lives locally in `%LOCALAPPDATA%\Pytte\session\`. Nothing is sent anywhere.

| File | Contents |
|---|---|
| `tab_<id>.wal` | Write-ahead log per tab: the base file's identity, then every insert and delete since the last save |
| `session.txt` | Tab order, active tab, cursor and scroll positions |
| `tab_<id>.wal.orphan-<time>` | Unsaved edits that could not be replayed because the file was changed outside Pytte (see below) |

How it avoids losing data:

- Edits are queued to a dedicated writer thread and written with `FILE_FLAG_WRITE_THROUGH`; batching keeps it to one write per tab per wake-up.
- On start, each log is replayed on top of its base file, but only if the file's size and modification time still match. If the file changed outside Pytte, the log is moved aside as an `.orphan` file instead of being applied to the wrong text, and the status bar says so.
- While a file is open it is mapped without write sharing: other programs can read it but not overwrite it underneath you. A file that another program is still writing (a live log, say) is opened as a snapshot of its size at that moment instead.

## Design

| Part | Choice |
|---|---|
| Language | Rust, with Microsoft's [`windows`](https://github.com/microsoft/windows-rs) crate; no UI framework |
| Text | Piece table over the memory-mapped file plus an append-only buffer; byte offsets throughout. Each piece caches its newline count, backed by a background line index, so line numbers cost O(pieces) |
| Rendering | Direct2D + DirectWrite, virtualised from the first visible line |
| I/O | Loader threads for opening, an index thread per file, a search thread, one writer thread for the log; the UI thread only sends messages |

## Known limitations

- Undo history is cleared when a tab is saved.
- Saving still runs on the UI thread; saving a multi-gigabyte file freezes the window for a few seconds.
- Lines that are several gigabytes long are slow to move around in, and the column number is counted from the start of the line.
- Case-insensitive search folds ASCII and Latin-1 only; regular expressions are not supported yet.
- No word wrap or double-click word selection yet.

## Roadmap

- Saving on a background thread
- Run the current file with its associated program (`Ctrl+R`)
- D3D11 and a glyph atlas in place of Direct2D, for an even faster cold start
- Phase 2, once the core is proven: syntax highlighting, LSP and plugins, always loaded asynchronously and never on the core's path

## Built with

[Rust](https://www.rust-lang.org/), [windows-rs](https://github.com/microsoft/windows-rs) (Direct2D, DirectWrite, Win32) and the [JetBrains Mono](https://www.jetbrains.com/lp/mono/) typeface, if installed.

## Author

Made by S.G. Johansson — [voidflow.tech](https://voidflow.tech/).

## License

Pytte is released under the [MIT License](LICENSE).
