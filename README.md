# passive-ftp.exe

Drop-in replacement for Windows `ftp.exe` that can do **passive FTP**.

Same command line, same commands, same `-s:` scripts. One extra flag:

```
ftp -p -s:upload.txt ftp.example.com
```

`-p` uses PASV (IPv4) / EPSV (IPv6) instead of PORT/EPRT. Without `-p` it behaves like the original (active mode).

## Download

Grab `ftp.exe` from the [releases](https://github.com/schinken/passive-ftp.exe/releases). Single static exe, no runtime needed.

## Differences to the original

- `-p` enables passive mode.
- In passive mode the IP address in the PASV reply is ignored, the data connection goes to the control connection's host (works behind NAT).
- `-x`, `-r`, `-b` are accepted but ignored (the OS tunes socket buffers better).
- Ctrl-C exits instead of aborting the running transfer.
- Downloads go to `<file>.part` first, so a failed transfer never overwrites an existing file.

## Build

```
cargo build --release
```

Push a tag (`git tag v1.0.0 && git push --tags`) and GitHub Actions builds `ftp.exe` and attaches it to a release.
