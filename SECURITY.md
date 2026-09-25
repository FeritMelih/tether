# Security

## What tether protects

A tether host lets whoever connects to it type into every program it holds, as the user who
runs it. So the boundary is the operating-system user, as with tmux or a shell's own
terminal: processes of the same user are trusted, everyone else is kept out.

- **Windows.** The host's named pipe has a random name, a DACL granting the user's SID alone
  and denying network logons, and rejects remote clients. The host checks the SID of each
  connecting process as well.
- **macOS and Linux.** The socket sits in a directory of mode 0700 owned by the user, is
  itself 0600, and each peer's uid is checked.
- **The token.** Every connection proves a 256-bit token held in the user's state directory
  (HMAC-SHA256 over both sides' nonces), and the host proves it back, so a client never
  reveals the token to a process squatting on a stale name.
- **No network.** The host never listens on one. An application that shows sessions remotely
  does so through its own connection, with its own authentication.

## What it does not

- A process running as the same user can read the state directory and reach the host. That
  is the same trust a user's own terminal extends to it.
- Sessions end with their host. A host that crashes takes its programs with it.

## Reporting

Report a vulnerability privately to the maintainer (the address on the repository owner's
profile), not in a public issue. You will hear back within a week.
