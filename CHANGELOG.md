# Changelog

## Unreleased

### Added

- **Sound from the computers you control.** Turn on **Play its sound here**
  under a computer on the **Screens** page to hear it through the speakers
  or headphones of the computer you're using. The switch works at once,
  without restarting sharing. Sound is sent uncompressed over its own
  encrypted connection, only while something plays, and stays in step with
  a delay of a few hundredths of a second. Both computers need this version.
  A Mac shares its sound on macOS 14.6 or later, once it's allowed under
  **Privacy & Security → Screen & System Audio Recording**.

### Fixed

- **A Mac now notices display changes.** Seam on a Mac kept describing the
  displays it had when it started. After a monitor was plugged in or the lid
  was closed, the pointer arriving from another computer was held to the old
  display's size. Parts of the screen, such as the Dock, were out of reach,
  and the pointer crossed back before reaching the edge. Seam waited for
  macOS to announce the change, which some versions never do. Every computer
  now checks its displays each second and reconnects with the new layout
  within a second of a change. Update Seam on the Mac to get this fix.
- **The main display takes over from a disconnected one.** When the display
  an edge belonged to was disconnected, as when a laptop's lid closes, that
  computer stopped taking the pointer and showed **Selected display is no
  longer available**. With only one display left, there was no picker to
  choose another. Now the computer's main display takes the edge until the
  chosen display is connected again, and the edge then goes back to it.

## 1.0.0

The first release of Seam: one keyboard, mouse and clipboard across your
Windows and Mac computers on the local network.

### Install

- **Windows 10 or 11:** download `Seam-1.0.0-windows-setup.exe` below and
  run it. Seam starts right away and every time you sign in. The setup
  program isn't signed yet; if SmartScreen appears, choose **More info →
  Run anyway**. `SHA256SUMS.txt` has its checksum.
- **macOS 11 or later:** releases don't include the Mac app yet. Build it
  from source with `scripts/build-macos.sh --install`; see
  [Build from source](https://github.com/solarisc/seam#build-from-source).

Install Seam on every computer you want to use, then pair them from the
**Screens** page.

### What's in it

- **Move between computers** by pushing the pointer off a shared screen
  edge, with the keyboard following. One computer shares its keyboard and
  mouse with up to four others, one on each edge, and you choose which
  display each edge belongs to.
- **Pair once** by picking a computer from the list and confirming a
  six-digit code on both. Every connection after that is authenticated and
  encrypted (Noise protocol with ChaCha20-Poly1305).
- **Low latency.** Pointer and keys use their own connection with no
  buffering or smoothing, so the remote pointer feels local.
- **Clipboard, images and files.** Copy on one computer and paste on
  another, including whole folders. Large files stream in the background
  without slowing the pointer.
- **Mac and Windows shortcuts.** Command and Control are swapped
  automatically, so copy and paste work as you expect on both.
- **Windows sign-in and lock screens.** Type your Windows password from the
  computer with the keyboard, even right after a restart.
- **Games and 3D apps.** On Windows, while a game or an app like Blender has
  taken the mouse, the pointer stays in it instead of crossing to another
  computer, and a controlled computer receives raw mouse movement so you
  can turn as far as you like.
- **Sleep and wake.** **Keep this computer awake** stops a controlled
  computer from going to sleep on its own, and **Keep the screen on** stops
  its screen from turning off too. Pushing the pointer against the edge of
  a computer that's asleep wakes it with Wake-on-LAN.
- **Getting back.** Ctrl+Alt+Shift+Esc, or Ctrl+Alt+Del on Windows, brings
  the keyboard and mouse back at once. On Windows, Ctrl+Alt+Shift with an
  arrow key switches to the computer on that side, and Scroll Lock keeps the
  pointer where it is.
- **Clean uninstall.** The Windows uninstaller removes everything Seam
  added and can also delete its settings and pairings.
