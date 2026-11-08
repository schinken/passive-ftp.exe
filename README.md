# passive-ftp.exe

Drop-in replacement for Windows `ftp.exe` that can do **passive FTP**.

Same command line, same commands, same `-s:` scripts. One extra flag:

```
ftp -p -s:upload.txt ftp.example.com
```

`-p` uses PASV (IPv4) / EPSV (IPv6) instead of PORT/EPRT. Without `-p` it behaves like the original (active mode).

## Download

Grab `ftp.exe` (and `ftp.exe.sha256`) from the [releases](https://github.com/schinken/passive-ftp.exe/releases). Single static exe, no runtime needed.

Active mode needs the server to connect back to you. Windows Firewall's built-in FTP rule only covers `C:\Windows\System32\ftp.exe`, so for this exe use `-p` or allow it in the firewall.

## Differences to the original

- `-p` enables passive mode.
- In passive mode the IP address in the PASV reply is ignored, the data connection goes to the control connection's host (works behind NAT).
- Nothing hangs forever: if the server doesn't connect back (active mode) or a connection stalls for 60 seconds, the command fails.
- With `-v`, 4xx errors (e.g. aborted transfer, disk full) are still shown, not only 5xx.
- `mget` saves each file under its bare name in the current directory, even if the server lists `dir/file` or `../file`.
- Script files should be UTF-8 (as for the original). Lines that aren't valid UTF-8 are read in the OEM code page, which is what `echo ... >> script.txt` in cmd.exe writes.
- File names are always sent as UTF-8.
- On a console, file names that aren't valid UTF-8 show up as `�` instead of breaking the output.
- `-x`, `-r`, `-b` are accepted but ignored (the OS tunes socket buffers better).
- Ctrl-C exits instead of aborting the running transfer.

## Build

```
cargo build --release
```

CI checks `cargo fmt`, clippy and the tests on every push. To release, bump `version` in `Cargo.toml`, then push a matching tag (`git tag v1.2.0 && git push origin v1.2.0`). GitHub Actions builds `ftp.exe` and attaches it plus a SHA-256 checksum to a release.
