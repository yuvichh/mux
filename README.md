<div align="center">
  <img src="assets/logo.png" alt="mux" width="220">
  <p><strong>Copy and sync files between machines.</strong></p>
</div>

mux is a command-line tool for Linux, macOS and Windows. Send a folder to
another machine with four words, copy from a server you can `ssh` to, download
from the web, or sync a folder. Machines connect directly when possible and
can use an encrypted relay connection.

The website and the documentation are at [mux.yuvich.com](https://mux.yuvich.com).

## Install

On **macOS or Linux**, run this in a terminal.

```sh
curl -fsSL https://mux.yuvich.com/install.sh | sh
```

It downloads the build for your machine, checks its SHA-256 digest and puts
`mux` in `~/.local/bin`. Run it again to upgrade. To remove mux, delete that
file.

| Or with | Run |
| --- | --- |
| Homebrew, on macOS or Linux | `brew install yuvichh/tap/mux` |
| Nix | `nix profile install github:yuvichh/mux` |

The Nix command builds the current source, which is newer than the latest
release.

On **Windows**, download [mux.exe][windows] and run `.\mux.exe --version` in
PowerShell from the download folder. Put `mux.exe` in a directory on your
`PATH` to use it from anywhere.

You can also download a build yourself.

| Platform | Download |
| --- | --- |
| macOS | [Apple silicon][mac-arm] · [Intel][mac-intel] |
| Linux (glibc 2.17+) | [x86-64][linux-intel] · [ARM64][linux-arm] |
| Windows | [x64 executable][windows] |

The [release][release] includes archives and SHA-256 checksums.

## Send a folder

On the machine that has the folder:

```sh
mux send ./photos
```

It prints four words, such as `polar-bulb-catalog-range`. On the other
machine:

```sh
mux get polar-bulb-catalog-range
```

The folder arrives in the current directory, every byte checked, and both
commands end by themselves. The words work for the first machine that uses
them and for no other. The two machines can be on different networks,
behind home routers.

## Copy from or to a server

If you can `ssh` to a machine, you can copy from it and to it. Nothing is
installed there.

```sh
mux get server:/srv/site
mux cp ./site server:/srv/
```

## Share with several machines

`mux send --ticket ./photos` prints a ticket and runs until you stop it.
Whoever has the ticket can download while it runs, and nobody else can.

| Add | Who can download |
| --- | --- |
| `--once` | Only the first machine that uses the ticket. |
| `--to ID` | Only the machine with that id. Run `mux id` on it to print the id. |
| `--swarm` | Anyone with the ticket, and they also download from each other. |

On each receiver, replace `TICKET` with the ticket you received. This creates
`./downloads/photos`. Run the same command again to resume an interrupted transfer.

```sh
mux get 'TICKET' ./downloads/
```

## Sync an existing folder

Use `sync` to update the exact directory you name. Preview the changes first.

```sh
mux sync 'TICKET' ./backup/photos --dry-run
mux sync 'TICKET' ./backup/photos
```

A ticket identifies one snapshot. To pull later edits, offer the updated
folder and sync its new ticket to the same destination. Mux verifies and
reuses local chunks, including chunks shifted by an insertion. Extra local
files stay in place unless you pass `--delete`. Replaced and deleted files
go to `.mux-trash` by default.

## More commands

| Command | Use |
| --- | --- |
| `mux get URL` | Download a file over HTTP or HTTPS. |
| `mux sync server:/srv/site ./site` | Bring a folder up to date from a machine you can `ssh` to. |
| `mux cp SOURCE... FOLDER/` | Copy several things at once, or under another name: `get` and `send` are short for it. |
| `mux cp ./photos ./archive/` | Copy a local directory with its metadata. |
| `mux probe 'TICKET'` | Inspect a snapshot and see which peers have it. |

For scripting, `--json` writes one JSON object per line. See the [command
reference](docs/commands.md) for exit codes, checksum verification, mirrors
and relay options.

**Upgrading from p2rent 0.2?** Install mux on every participating machine
and generate new tickets. Mux keeps the existing device identity on first
use. See the [migration notes][release].

[How it works](docs/how-it-works.md) · [Performance](docs/performance.md) ·
[Architecture](docs/architecture.md) · [Changelog](CHANGELOG.md) ·
[Contributing](CONTRIBUTING.md) · [MIT license](LICENSE)

[release]: https://github.com/yuvichh/mux/releases/tag/v0.3.0
[mac-arm]: https://github.com/yuvichh/mux/releases/download/v0.3.0/mux-Darwin-arm64
[mac-intel]: https://github.com/yuvichh/mux/releases/download/v0.3.0/mux-Darwin-x86_64
[linux-arm]: https://github.com/yuvichh/mux/releases/download/v0.3.0/mux-Linux-aarch64
[linux-intel]: https://github.com/yuvichh/mux/releases/download/v0.3.0/mux-Linux-x86_64
[windows]: https://github.com/yuvichh/mux/releases/download/v0.3.0/mux.exe
