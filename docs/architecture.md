# How Seam works

Seam has two programs:

- **`seam-agent`** runs in the background for as long as you are signed in.
  It captures the keyboard and mouse, talks to the other computers and applies
  their input. On Windows the installer starts it with a sign-in task; on
  macOS the app registers it as a login item (`~/Library/LaunchAgents`).
- **`seam`** is the settings window (and on Windows the notification-area
  icon). It never touches input; it reads the agent's state and sends it
  commands over a local, token-protected connection.

## Roles

The computer whose keyboard and mouse you use **controls** up to four others,
one per screen edge (`Config::peers`). It opens a connection to each of them.
A computer that is controlled listens for one controlling computer and keeps
the arrangement of that computer's screen in `Config::arrangement`. With one
pair, either computer's own mouse can cross to the other; when the
controlling computer drives several computers, the others only receive.

Inside the agent every connection is a `config::Link`: the shared
preferences plus that connection's address, secret and arrangement.

## Connections and ports

All ports derive from `listen_port` (default 47843):

| Port | Use |
|---|---|
| `listen_port` (TCP) | Input: pointer, keys, buttons, scrolling, layout changes, heartbeats |
| `+1` (TCP) | Clipboard and file transfers, bound to the input session |
| `+2` (TCP, localhost) | Commands from the settings window and tray |
| `+3` (TCP) | Pairing requests |
| `+3` (UDP) | Discovery beacons |
| `+4` (TCP) | A controlled computer's sound, bound to the input session |

Input, clipboard and sound use separate connections so a large copy never
delays the pointer or the sound. All are encrypted with the Noise `NNpsk0`
pattern keyed by the receiving computer's 128-bit pairing secret. `NNpsk0`'s first message can be
replayed by anyone who recorded it, so the connecting computer follows the
handshake with a message encrypted under the new session key, and the
receiving computer accepts the connection only once that decrypts
(`network.rs`). Each handshake must finish within 3 seconds as a whole,
whatever the pace of individual reads.

The Windows sign-in helper (below) uses `listen_port + 10` to `+ 12` and
`+ 14` in the same way.

## Pairing

`pairing.rs` runs an unauthenticated Noise `NN` handshake and derives a
six-digit code from the handshake hash. Both screens show the code; when the
person on the receiving computer accepts, its secret is sent over the
encrypted channel and saved by the requester. A machine in the middle would
cause the codes to differ.

## Screen arrangement

`layout.rs` keeps both computers' copies of an arrangement identical. The
controlling computer coordinates a two-phase change (prepare, commit, apply);
no new handoff happens until both have installed the same layout, and
interrupted changes are reconciled on reconnect. Displays are remembered by
persistent OS identities, so a monitor that changes number after a reboot is
found again and a missing one is never silently replaced.

## Input

- macOS: a Core Graphics event tap (`mac_capture.rs`) and posted events.
- Windows: low-level hooks plus raw input for edge detection
  (`windows_capture.rs`) and `SendInput` (`windows_input.rs`). No edge is
  crossed while the foreground app confines the cursor, hides it full
  screen (games' mouse-look) or holds mouse capture with no button down
  (Blender's wrapping grabs) (`windows_pointer_lock.rs`). A controlled
  Windows computer reports the same state (`PointerLock`), and also when an
  app moves the cursor away from where Seam put it during a drag or while
  hiding it. Its controller then sends raw mouse counts (`Motion`) instead of
  positions and holds the return edge until the app lets go for 100 ms, or
  at once when another app comes to the front.
  Ordinary windows hold the cursor too, and never count: Windows confines it
  while a window is moved or resized and captures it for menus (both show in
  the thread's `GUITHREADINFO` flags), popups such as a browser's bar for
  selected text capture it in a window of their own, an app keeps a drag's
  capture or clip until it handles the button's release (a visible cursor
  held within 250 ms of one proves nothing), and video players hide a
  resting cursor until it moves (a hidden cursor alone counts once the
  pointer has moved for 50 ms, and a controlled computer looks again then).

Pointer motion is coalesced, never delayed: only positions already waiting
are merged. Keys, buttons and scrolls keep their order. When control returns
or a connection drops, held keys and buttons are released on the computer
that received them.

## Sound

While **Play its sound here** is on for a controlled computer, the
controlling computer keeps a connection open to that computer's sound port,
bound to the input session like the clipboard's (`sound/mod.rs`). The switch
is a live command to the agent, which also saves it; no restart is needed.
The controlled computer records its default output device only while that
connection is open (`sound/capture.rs`): WASAPI loopback on Windows, a Core
Audio process tap on macOS 14.6 and later. It folds the recording to stereo,
rounds it to 16 bits with triangular dither and sends it at its own sample
rate, as one block of little-endian bytes per buffer the system delivers,
about 10 ms each. After 250 ms of silence it sends a pause and nothing more
until sound returns; while paused, silent buffers are dropped as they are
recorded. The Windows sign-in helper answers on its sound port that the
computer is locked.

The controlling computer plays through its default output device. On
Windows (`sound/push.rs`, `sound/wasapi.rs`) the network thread converts
each packet as it arrives and writes it straight into the speakers'
shared-mode buffer, so sound waits in no other buffer and no thread wakes on
a timer to feed the speakers. When a sound starts, the buffer is topped up
with silence to the target, so it starts no later than it continues. On
macOS (`sound/playback.rs`) the speakers ask for sound, and their callback
takes it from a buffer the network thread fills. Either way the conversion
is a crossfaded frame slip when both computers use the same rate, and a sinc
resampler otherwise, and it leans the rate by up to 0.5 % to hold the buffer
at its target, which follows the drift between the two audio clocks.

The target is as short as the network allows (`sound/pacing.rs`). Sound
arrives in the sender's buffers and leaves in the speakers', about 10 ms
each, and as the two clocks drift the speakers now and then find no new
packet. So every 2 seconds of playing, the buffer checks two things over the
last 10 seconds: that a typical low point (the tenth percentile) still held a
packet plus a margin, and that the lowest held the margin. Short of either,
the target grows at once; with room to spare, and the buffer settled, it
shrinks by up to 2 ms. The margin starts at 4 ms and widens by 3 ms each time
the buffer runs dry. On a steady wired network between two Windows
computers, sound plays here about 30 ms after it plays there.

After 2 seconds with nothing to play, the speakers pause, which costs
nothing, and resume when sound arrives; after 10 minutes paused they close.
The worker that manages them wakes only for that, a switch, or once a second
while sound plays, when it also follows a change of default device. The
threads that send and that play sound run at real-time audio priority (Pro
Audio on Windows, a time-constraint thread on macOS), as the audio callbacks
do.

## Windows sign-in and lock screens

The installer adds the `SeamSignIn` service (`windows_signin.rs`). While
nobody is signed in to the console session, or the session is locked, it
starts a receive-only helper on the secure desktop using a copy of the
computer's pairing kept in `%ProgramData%\Seam\sign-in` (readable only by
SYSTEM and administrators). The service refreshes the copy from the signed-in
person's settings and restarts the helper when it changes, so unpairing takes
effect on the lock screen too. Only a computer paired to be controlled has a
copy; the helper's own screen arrangement is kept when the copy is refreshed.
With nobody signed in, as after a restart, the copy from the last person stays
in place, so their controlling computer can sign in to this one. The helper listens on `listen_port + 10` and
announces that port in its discovery beacon; while the session is locked the
person's agent turns connections away, so the controlling computer connects
to the helper instead.

## Sleep and wake

With **Keep this computer awake** on, the agent holds a Windows power request
(`PowerRequestSystemRequired`) or a macOS `PreventUserIdleSystemSleep`
assertion for as long as it runs (`power.rs`). Neither keeps the display on.

A controlling computer reads each controlled computer's hardware address
from its own neighbor (ARP) table whenever a connection to it succeeds, and
saves it with the pairing (`wake.rs`); nothing new crosses the wire. While
that computer isn't connected, the input hooks keep its edge. Pushing
against it for 250 ms asks the link's connection thread, never the hook
itself, to send a magic packet to every local subnet's broadcast address and
the last known address, then to retry the connection every second for a
minute.
