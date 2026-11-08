# passive-ftp.exe

Drop-in replacement for Windows `ftp.exe` that always uses **passive FTP**.

Made for applications that call `ftp.exe` and can't be changed. Put this `ftp.exe` into the application's folder (Windows looks there, and in the current directory, before `System32`). From then on every transfer uses passive mode. Same command line, same commands, same `-s:` scripts, same output.

Why: the original only does active mode, where the server connects back to you. Firewalls and NAT usually block that. In passive mode the client opens every connection itself.

## Download

Grab `ftp.exe` (and `ftp.exe.sha256`) from the [releases](https://github.com/schinken/passive-ftp.exe/releases). Single static exe, no runtime needed.

## Differences to the original

- Always passive: PASV for IPv4, EPSV for IPv6. The address in a PASV reply is ignored, the data connection goes to the server you're connected to (works behind NAT). `-a` is accepted and does nothing.
- Never hangs forever: a connection that stays silent for 15 minutes fails the command.
- `mget` never writes outside the current directory: server names like `../x` or `C:\x` are saved under their bare file name.
- Script lines that aren't valid UTF-8 are read in the OEM code page (what `echo ... >> script.txt` in cmd.exe writes) instead of turning into `?`.
- On a console, file names that aren't valid UTF-8 show up as `�` instead of breaking the output.
- File names are always sent as UTF-8.
- `-x`, `-r`, `-b` are accepted but ignored (Windows tunes socket buffers itself).
- Ctrl-C ends the program instead of only the running transfer.

## Build

On Windows:

```
cargo build --release
```

CI checks `cargo fmt`, clippy and the tests on every push. To release, bump `version` in `Cargo.toml`, then push a matching tag (`git tag v1.2.0 && git push origin v1.2.0`). GitHub Actions builds `ftp.exe` and attaches it plus a SHA-256 checksum to a release.
