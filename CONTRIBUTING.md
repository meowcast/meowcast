# Contributing

Thank you for looking. This is a young project with one supported board; ports, fixes,
measurements on other units and documentation are all useful.

## Before you start

- **Bugs and ideas**: open an issue. For a device problem, say which board, what the PCB
  markings are, the commit you built, and attach `/run/ui.log`, `/run/appd.log` or the USB console output.
- **Security problems**: not in the tracker. See [SECURITY.md](SECURITY.md).
- **A new board**: read [boards/README.md](boards/README.md) and open an issue early, so the
  parts of the tree that still assume the first board can be loosened together.
- **Large changes**: describe the plan in an issue first.

## Workflow

1. Fork, branch from `main`, keep a change to one topic.
2. `./meow build` and `./meow test` must pass. Say in the pull request what you ran and on
   what hardware: "builds", "host tests", "RAM-booted on rk3228a-dongle", "flashed".
   Untested is fine if you say so.
3. Open a pull request. Maintainers review, may ask for changes, and squash or merge.

By contributing you agree that your work is licensed under the licence of the files you
change (GPL-3.0-or-later unless a file says otherwise). Add a `Signed-off-by:` line
(`git commit -s`) to certify the [Developer Certificate of Origin](https://developercertificate.org/).

## Rules that protect people

- **No secrets, no personal infrastructure.** Addresses, account names, keys and tokens
  belong in `config.env`, which is ignored. Use `example.org` names in code, tests and docs.
- **No vendor firmware.** No flash dumps, loader binaries, extracted vendor files or
  disassembly of them. Fetch from the vendor's public repository at a pinned commit, or have
  the user read it from their own device.
- **Flash tools stay conservative.** A change to anything under `boards/*/flash` or
  `boards/*/rockusb` must keep the write window and the checks, default to a dry run, and
  say how it was tested on hardware.
- **Pin what you download.** A new source archive or repository gets a SHA-256 or a commit.
- **Apps for other companies' devices and services** do not go into this repository: propose
  them in [meowcast/apps-extra](https://github.com/meowcast/apps-extra), or publish them from a repository of your own
  ([cloud/store/README.md](cloud/store/README.md#a-repository-of-your-own)).

## Style

- Shell is POSIX `sh` (`set -eu`); Python is standard library only for build and host tools.
- Rust: `cargo fmt` is not enforced yet; match the file you are in. No new dependency without
  a note on its licence and size: the whole system has to fit in 11 MiB of flash.
- Comments say why, and what was measured. Hardware claims carry a date and the board.
- Documentation is written in plain present tense; say what is not verified.
- Every screen must work with a D-pad: up, down, left, right, OK, Back.

## Layout

See the table in [README.md](README.md) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
Component READMEs are the reference for their directory; update them with the code.
