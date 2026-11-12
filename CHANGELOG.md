# Changelog

## 1.2.1

Double-clicking works on a Mac you control from another computer.

### Install

- **Windows 10 or 11:** download `Seam-1.2.1-windows-setup.exe` below and
  run it; it updates an earlier version in place, keeping your settings and
  pairings. The setup program isn't signed yet; if SmartScreen appears,
  choose **More info → Run anyway**. `SHA256SUMS.txt` has its checksum.
- **macOS 11 or later:** releases don't include the Mac app yet. Build it
  from source with `scripts/build-macos.sh --install`; see
  [Build from source](https://github.com/solarisc/seam#build-from-source).

1.2.1 connects to 1.2.0. The fix is for the Mac being controlled, so update
Seam on that Mac to get it.

### Fixed

- **Double-click on a controlled Mac.** Clicks sent to a Mac were always
  posted as single clicks, so double-clicking in Finder or any other app
  did nothing. Seam now counts quick presses of the same button in about
  the same place, using the double-click speed set on the Mac, so double
  and triple clicks work as they do with a mouse plugged into the Mac. A
  press that is too slow, moves too far or uses another button starts a
  new count.

## 1.2.0

Copied files are sent when you paste them, and the Screens page keeps every
computer in place however your displays are arranged.

### Install

- **Windows 10 or 11:** download `Seam-1.2.0-windows-setup.exe` below and
  run it; it updates an earlier version in place, keeping your settings and
  pairings. The setup program isn't signed yet; if SmartScreen appears,
  choose **More info → Run anyway**. `SHA256SUMS.txt` has its checksum.
- **macOS 11 or later:** releases don't include the Mac app yet. Build it
  from source with `scripts/build-macos.sh --install`; see
  [Build from source](https://github.com/solarisc/seam#build-from-source).

Update Seam on every computer: 1.2.0 doesn't connect to earlier versions.

### Changed

- **Copied files are sent when you paste them.** Copying files or folders
  sent them to the other computers at once, even while you went on working
  on the computer you copied them on. Now the other computers only learn
  what was copied, their names and sizes, and a file is sent when you paste
  it on one of them. File Explorer shows its usual copy progress while the
  files arrive, and a Mac finishes the paste once they have. The computer
  you control others from passes files between them the same way, fetching
  them only when they are pasted. On Windows, files from another computer
  paste into File Explorer and into apps that take files pasted from Remote
  Desktop. Update Seam on every computer: this version doesn't connect to
  earlier ones.

### Fixed

- **The Screens page and edges follow how displays are arranged.** A
  computer placed against an edge that another display covers sat on top of
  that display, drawn in red, and the pointer couldn't cross to it. A
  computer whose displays reached back toward this one was drawn over this
  computer's screens. Now an edge always meets the outside of both
  computers' desktops: when another display covers the chosen one's edge,
  the edge moves to the display beyond it, on both computers, and is saved
  there. A computer's other displays are drawn beyond the one it meets, so
  no screen on the page covers another. Display menus show the display each
  edge actually uses, and a display whose edge is covered can't be chosen.

## 1.1.0

Sound from the computers you control, and better handling of display
changes on a Mac.

### Install

- **Windows 10 or 11:** download `Seam-1.1.0-windows-setup.exe` below and
  run it; it updates an earlier version in place, keeping your settings and
  pairings. The setup program isn't signed yet; if SmartScreen appears,
  choose **More info → Run anyway**. `SHA256SUMS.txt` has its checksum.
- **macOS 11 or later:** releases don't include the Mac app yet. Build it
  from source with `scripts/build-macos.sh --install`; see
  [Build from source](https://github.com/solarisc/seam#build-from-source).

Update Seam on every computer: playing a computer's sound elsewhere needs
1.1.0 on both.

### Added

- **Sound from the computers you control.** Turn on **Play its sound here**
  under a computer on the **Screens** page to hear it through the speakers
  or headphones of the computer you're using. The switch works at once,
  without restarting sharing. Sound is sent uncompressed over its own
  encrypted connection, only while something plays, and stays in step with
  a delay of a few hundredths of a second. A Windows computer shares every
  app's sound, whichever speakers or headphones it plays on. Both computers
  need this version.
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
