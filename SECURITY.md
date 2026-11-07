# Security

Seam controls keyboards and mice, so security reports get priority.

## Reporting a problem

Please don't open a public issue. Use GitHub's
[private vulnerability reporting](../../security/advisories/new) for this
repository, with the Seam version, operating systems and steps to reproduce.

## What Seam protects

- **Pairing** runs an unauthenticated Noise `NN` handshake and shows a
  six-digit code derived from it on both computers. A person must confirm
  that the codes match before the receiving computer sends its 128-bit
  secret, so a machine in the middle is detected.
- **Every later connection** uses Noise `NNpsk0` keyed by that secret, and
  the connecting computer must also prove it holds the new session key. A
  recorded handshake can't be replayed.
- **Unauthenticated traffic** is limited: handshakes must finish within
  3 seconds, only a few may be in progress, and discovery beacons can't grow
  memory without bound.

## Known limits

- The pairing secret is stored in the person's settings folder
  (`%APPDATA%\Seam` or `~/Library/Application Support/Seam`). Anyone who can
  read that folder can control the computer. Treat it like a password.
- The local control port used by the settings window is protected by a
  token in the same folder. Another person signed in to the same computer
  can't read the token, but could occupy the port first if Seam isn't
  running for you.
- Discovery beacons are not authenticated. They only suggest addresses;
  every connection is still authenticated with the pairing secret.
- Release builds are not yet code-signed.
