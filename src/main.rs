//! Drop-in replacement for Windows `ftp.exe` that uses passive mode for all transfers.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path};
use std::process::Command;
use std::time::{Duration, Instant};

const USAGE: &str = "
Transfers files to and from a computer running an FTP server service
(sometimes called a daemon). Ftp can be used interactively.

FTP [-v] [-d] [-i] [-n] [-g] [-s:filename] [-a] [-A] [-x:sendbuffer] [-r:recvbuffer] [-b:asyncbuffers] [-w:windowsize] [host]

  -v              Suppresses display of remote server responses.
  -n              Suppresses auto-login upon initial connection.
  -i              Turns off interactive prompting during multiple file
                  transfers.
  -d              Enables debugging.
  -g              Disables filename globbing (see GLOB command).
  -s:filename     Specifies a text file containing FTP commands; the
                  commands will automatically run after FTP starts.
  -a              Use any local interface when binding data connection.
  -A              login as anonymous.
  -x:send sockbuf Overrides the default SO_SNDBUF size of 8192.
  -r:recv sockbuf Overrides the default SO_RCVBUF size of 8192.
  -b:async count  Overrides the default async count of 3
  -w:windowsize   Overrides the default transfer buffer size of 65535.
  host            Specifies the host name or IP address of the remote
                  host to connect to.

Notes:
  - mget and mput commands take y/n/q for yes/no/quit.
  - Use Control-C to abort commands.
";

/// An interactive command: the name users type (or abbreviate) and its help text.
#[derive(Debug, PartialEq)]
struct CommandInfo {
    name: &'static str,
    help: &'static str,
}

const COMMANDS: &[CommandInfo] = &[
    CommandInfo {
        name: "!",
        help: "escape to the shell",
    },
    CommandInfo {
        name: "?",
        help: "print local help information",
    },
    CommandInfo {
        name: "append",
        help: "append to a file",
    },
    CommandInfo {
        name: "ascii",
        help: "set ascii transfer type",
    },
    CommandInfo {
        name: "bell",
        help: "beep when command completed",
    },
    CommandInfo {
        name: "binary",
        help: "set binary transfer type",
    },
    CommandInfo {
        name: "bye",
        help: "terminate ftp session and exit",
    },
    CommandInfo {
        name: "cd",
        help: "change remote working directory",
    },
    CommandInfo {
        name: "close",
        help: "terminate ftp session",
    },
    CommandInfo {
        name: "delete",
        help: "delete remote file",
    },
    CommandInfo {
        name: "debug",
        help: "toggle debugging mode",
    },
    CommandInfo {
        name: "dir",
        help: "list contents of remote directory",
    },
    CommandInfo {
        name: "disconnect",
        help: "terminate ftp session",
    },
    CommandInfo {
        name: "get",
        help: "receive file",
    },
    CommandInfo {
        name: "glob",
        help: "toggle metacharacter expansion of local file names",
    },
    CommandInfo {
        name: "hash",
        help: "toggle printing `#' for each buffer transferred",
    },
    CommandInfo {
        name: "help",
        help: "print local help information",
    },
    CommandInfo {
        name: "lcd",
        help: "change local working directory",
    },
    CommandInfo {
        name: "literal",
        help: "send arbitrary ftp command",
    },
    CommandInfo {
        name: "ls",
        help: "nlist contents of remote directory",
    },
    CommandInfo {
        name: "mdelete",
        help: "delete multiple files",
    },
    CommandInfo {
        name: "mdir",
        help: "list contents of multiple remote directories",
    },
    CommandInfo {
        name: "mget",
        help: "get multiple files",
    },
    CommandInfo {
        name: "mkdir",
        help: "make directory on the remote machine",
    },
    CommandInfo {
        name: "mls",
        help: "nlist contents of multiple remote directories",
    },
    CommandInfo {
        name: "mput",
        help: "send multiple files",
    },
    CommandInfo {
        name: "open",
        help: "connect to remote ftp",
    },
    CommandInfo {
        name: "prompt",
        help: "force interactive prompting on multiple commands",
    },
    CommandInfo {
        name: "put",
        help: "send one file",
    },
    CommandInfo {
        name: "pwd",
        help: "print working directory on remote machine",
    },
    CommandInfo {
        name: "quit",
        help: "terminate ftp session and exit",
    },
    CommandInfo {
        name: "quote",
        help: "send arbitrary ftp command",
    },
    CommandInfo {
        name: "recv",
        help: "receive file",
    },
    CommandInfo {
        name: "remotehelp",
        help: "get help from remote server",
    },
    CommandInfo {
        name: "rename",
        help: "rename file",
    },
    CommandInfo {
        name: "rmdir",
        help: "remove directory on the remote machine",
    },
    CommandInfo {
        name: "send",
        help: "send one file",
    },
    CommandInfo {
        name: "status",
        help: "show current status",
    },
    CommandInfo {
        name: "trace",
        help: "toggle packet tracing",
    },
    CommandInfo {
        name: "type",
        help: "set file transfer type",
    },
    CommandInfo {
        name: "user",
        help: "send new user information",
    },
    CommandInfo {
        name: "verbose",
        help: "toggle verbose mode",
    },
];

const NEEDS_CONNECTION: &[&str] = &[
    "append",
    "cd",
    "close",
    "delete",
    "dir",
    "disconnect",
    "get",
    "literal",
    "ls",
    "mdelete",
    "mdir",
    "mget",
    "mkdir",
    "mls",
    "mput",
    "put",
    "pwd",
    "quote",
    "recv",
    "remotehelp",
    "rename",
    "rmdir",
    "send",
    "user",
];

const HASH_BYTES: u64 = 2048;

struct Ftp {
    input: Box<dyn BufRead>,
    tty: bool,
    control: Option<BufReader<TcpStream>>,
    host: String,
    verbose: bool,
    debug: bool,
    prompt: bool,
    glob: bool,
    hash: bool,
    bell: bool,
    trace: bool,
    autologin: bool,
    anonymous: bool,
    binary: bool,
    buffer_size: usize,
    timeout: Duration,
}

type Sink<'a> = Box<dyn Write + 'a>;

/// A complete server reply. `code` is 0 when the connection is gone.
#[derive(Debug, PartialEq)]
struct Reply {
    code: u16,
    text: String,
}

/// What the command line asks for besides the flags.
struct CommandLine {
    script: Option<String>,
    host: Vec<String>,
}

/// Answer to an mget/mput/mdelete prompt.
enum Answer {
    Yes,
    No,
    Quit,
}

/// Console output that replaces invalid UTF-8: Windows consoles reject such bytes,
/// which a listing with Latin-1 file names would otherwise trigger.
// ponytail: a multi-byte character split across two reads prints as two U+FFFD.
struct Console;

impl Write for Console {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        print!("{}", String::from_utf8_lossy(b));
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// Raw bytes when redirected, lossy text on a console.
fn stdout_sink() -> Sink<'static> {
    if io::stdout().is_terminal() {
        Box::new(Console)
    } else {
        Box::new(io::stdout())
    }
}

impl Ftp {
    fn new(input: Box<dyn BufRead>, tty: bool) -> Self {
        Ftp {
            input,
            tty,
            control: None,
            host: String::new(),
            verbose: true,
            debug: false,
            prompt: true,
            glob: true,
            hash: false,
            bell: false,
            trace: false,
            autologin: true,
            anonymous: false,
            binary: false,
            buffer_size: 65535,
            timeout: Duration::from_secs(15 * 60),
        }
    }

    /// Applies ftp.exe flags. Returns None to show usage.
    fn parse_args(&mut self, args: impl IntoIterator<Item = String>) -> Option<CommandLine> {
        let mut script = None;
        let mut host = Vec::new();
        for arg in args {
            let Some(opts) = arg.strip_prefix('-') else {
                host.push(arg);
                continue;
            };
            for (i, c) in opts.char_indices() {
                match c {
                    'v' => self.verbose = false,
                    'd' => self.debug = true,
                    'i' => self.prompt = false,
                    'n' => self.autologin = false,
                    'g' => self.glob = false,
                    'a' => {} // Data connections are passive: nothing to bind locally.
                    'A' => self.anonymous = true,
                    's' | 'w' | 'x' | 'r' | 'b' => {
                        let v = opts[i + 1..].trim_start_matches(':');
                        match c {
                            's' => script = Some(v.to_string()),
                            'w' => self.buffer_size = v.parse().unwrap_or(self.buffer_size).max(1),
                            _ => {} // -x/-r/-b socket tuning: the OS auto-tunes better.
                        }
                        break;
                    }
                    _ => return None,
                }
            }
        }
        Some(CommandLine { script, host })
    }

    /// Runs one `ftp>` command line, e.g. `get a.txt`.
    fn exec(&mut self, line: &str) {
        let line = line.trim();
        let (name, rest) = match line.strip_prefix('!') {
            Some(r) => ("!".to_string(), r),
            None => {
                let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
                (word.to_lowercase(), rest.trim())
            }
        };
        if name.is_empty() {
            return;
        }
        match lookup(&name) {
            Ok(c) => self.run(c.name, rest, split_args(rest)),
            Err(e) => println!("{e}"),
        }
    }

    /// Reads one line from the command source (console or -s script).
    fn ask(&mut self, prompt: &str) -> Option<String> {
        self.read_input(prompt, /* secret= */ false)
    }

    /// Like `ask`, but hides the typed text (passwords).
    fn ask_secret(&mut self, prompt: &str) -> Option<String> {
        self.read_input(prompt, /* secret= */ true)
    }

    fn read_input(&mut self, prompt: &str, secret: bool) -> Option<String> {
        print!("{prompt}");
        io::stdout().flush().ok();
        let hide = secret && self.tty;
        if hide {
            set_console_echo(false);
        }
        let mut buf = Vec::new();
        let n = self.input.read_until(b'\n', &mut buf);
        if hide {
            set_console_echo(true);
        }
        if !matches!(n, Ok(1..)) {
            return None;
        }
        let line = String::from_utf8(buf).unwrap_or_else(|e| decode_oem(e.as_bytes()));
        let line = line
            .trim_start_matches('\u{feff}')
            .trim_end_matches(['\r', '\n']);
        // Like ftp.exe: echo script input so logs show what ran.
        if hide || !self.tty {
            println!("{}", if secret { "" } else { line });
        }
        Some(line.to_string())
    }

    /// Prompts for missing arguments: all prompts if none given, else up to `required`.
    fn require_args(&mut self, args: &mut Vec<String>, required: usize, prompts: &[&str]) -> bool {
        let wanted = if args.is_empty() {
            prompts.len()
        } else {
            required
        };
        while args.len() < wanted {
            match self.ask(prompts[args.len()]) {
                Some(arg) if !arg.trim().is_empty() => {
                    args.push(arg.trim().trim_matches('"').to_string())
                }
                _ => break,
            }
        }
        args.len() >= required
    }

    fn confirm(&mut self, what: &str, name: &str) -> Answer {
        if !self.prompt {
            return Answer::Yes;
        }
        let Some(answer) = self.ask(&format!("{what} {name}? ")) else {
            return Answer::Quit;
        };
        match answer.trim_start().chars().next() {
            Some('n' | 'N') => Answer::No,
            Some('q' | 'Q') => Answer::Quit,
            _ => Answer::Yes,
        }
    }

    fn drop_connection(&mut self, why: &str) {
        println!("{why}");
        self.control = None;
    }

    fn reply(&mut self) -> Reply {
        let mut text = String::new();
        let mut code = 0;
        loop {
            let Some(r) = self.control.as_mut() else {
                return Reply { code: 0, text };
            };
            let mut buf = Vec::new();
            match r.read_until(b'\n', &mut buf) {
                Ok(1..) => {}
                Ok(0) => {
                    self.drop_connection("Connection closed by remote host.");
                    return Reply { code: 0, text };
                }
                Err(e) => {
                    self.drop_connection(&format!("> ftp: {e}"));
                    return Reply { code: 0, text };
                }
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\r', '\n']);
            let done = match (code, reply_start(line)) {
                (0, None) => false, // Stray line before a reply: show it, keep waiting.
                (0, Some((c, more))) => {
                    code = c;
                    !more
                }
                (c, _) => line.starts_with(&format!("{c} ")) || line == c.to_string(),
            };
            if self.verbose || code >= 500 {
                println!("{line}");
            }
            if code == 0 {
                continue;
            }
            text += line;
            text.push('\n');
            if done {
                return Reply { code, text };
            }
        }
    }

    fn command(&mut self, line: &str) -> Reply {
        let Some(control) = self.control.as_mut() else {
            println!("Not connected.");
            return Reply {
                code: 0,
                text: String::new(),
            };
        };
        if self.debug {
            println!(
                "---> {}",
                if line.starts_with("PASS ") {
                    "PASS XXXX"
                } else {
                    line
                }
            );
        }
        if control
            .get_mut()
            .write_all(format!("{line}\r\n").as_bytes())
            .is_err()
        {
            self.drop_connection("Connection closed by remote host.");
            return Reply {
                code: 0,
                text: String::new(),
            };
        }
        self.reply()
    }

    /// Like `command`, for callers that only need the reply code.
    fn send(&mut self, line: &str) -> u16 {
        self.command(line).code
    }

    fn open(&mut self, mut args: Vec<String>) {
        if self.control.is_some() {
            println!("Already connected to {}, use disconnect first.", self.host);
            return;
        }
        if !self.require_args(&mut args, 1, &["To "]) {
            return;
        }
        let port: u16 = args.get(1).and_then(|port| port.parse().ok()).unwrap_or(21);
        let host = args.swap_remove(0);
        let addrs: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
            Ok(it) => it.collect(),
            Err(_) => {
                println!("Unknown host {host}.");
                return;
            }
        };
        let stream = match TcpStream::connect(&addrs[..]) {
            Ok(stream) => stream,
            Err(e) => {
                println!("> ftp: connect :{e}");
                return;
            }
        };
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(self.timeout)).ok();
        stream.set_write_timeout(Some(self.timeout)).ok();
        println!("Connected to {host}.");
        self.control = Some(BufReader::new(stream));
        self.host = host;
        self.binary = false;
        if self.reply().code / 100 != 2 {
            self.control = None;
            return;
        }
        self.command("OPTS UTF8 ON");
        if self.autologin {
            self.login(Vec::new());
        }
    }

    fn login(&mut self, args: Vec<String>) {
        let args = if args.is_empty() && self.anonymous {
            let me = env::var("USERNAME").unwrap_or_default();
            let host = env::var("COMPUTERNAME").unwrap_or_default();
            vec!["anonymous".into(), format!("{me}@{host}")]
        } else {
            args
        };
        let mut args = args.into_iter();
        let (user, pass, account) = (args.next(), args.next(), args.next());
        let prompt = format!("User ({}:(none)): ", self.host);
        let Some(user) = user.or_else(|| self.ask(&prompt)) else {
            return;
        };
        let mut code = self.send(&format!("USER {user}"));
        if code == 331 {
            let Some(pass) = pass.or_else(|| self.ask_secret("Password:")) else {
                return;
            };
            code = self.send(&format!("PASS {pass}"));
        }
        if code == 332 {
            let Some(account) = account.or_else(|| self.ask("Account:")) else {
                return;
            };
            code = self.send(&format!("ACCT {account}"));
        }
        if code / 100 != 2 {
            println!("Login failed.");
        }
    }

    fn close(&mut self) {
        if self.control.is_some() {
            self.send("QUIT");
            self.control = None;
        }
    }

    fn quit(&mut self) -> ! {
        self.close();
        std::process::exit(0)
    }

    fn set_binary(&mut self, binary: bool) {
        if self.send(if binary { "TYPE I" } else { "TYPE A" }) / 100 == 2 {
            self.binary = binary;
        }
    }

    /// Opens the data connection with PASV (IPv4) or EPSV (IPv6).
    fn data_connection(&mut self) -> Option<TcpStream> {
        let peer = self.control.as_ref()?.get_ref().peer_addr().ok()?;
        let reply = self.command(if peer.is_ipv4() { "PASV" } else { "EPSV" });
        let port = pasv_port(reply.code, &reply.text)?;
        // The address in a PASV reply is ignored (NAT-safe, like curl): always use the control peer.
        let stream = match TcpStream::connect((peer.ip(), port)) {
            Ok(stream) => stream,
            Err(e) => {
                println!("> ftp: connect :{e}");
                return None;
            }
        };
        stream.set_read_timeout(Some(self.timeout)).ok();
        stream.set_write_timeout(Some(self.timeout)).ok();
        Some(stream)
    }

    fn copy(&self, reader: &mut dyn Read, writer: &mut dyn Write, ascii: bool) -> io::Result<u64> {
        let mut buf = vec![0u8; self.buffer_size];
        let mut converted = Vec::new();
        let (mut total, mut marks, mut cr) = (0u64, 0u64, false);
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if ascii {
                to_crlf(&buf[..n], &mut cr, &mut converted);
                writer.write_all(&converted)?;
            } else {
                writer.write_all(&buf[..n])?;
            }
            total += n as u64;
            if self.hash && total / HASH_BYTES > marks {
                print!("{}", "#".repeat((total / HASH_BYTES - marks) as usize));
                io::stdout().flush()?;
                marks = total / HASH_BYTES;
            }
        }
        writer.flush()?;
        if marks > 0 {
            println!();
        }
        Ok(total)
    }

    fn finish(&mut self, result: io::Result<u64>, start: Instant, direction: &str) -> bool {
        let secs = start.elapsed().as_secs_f64();
        if let Err(e) = &result {
            println!("> ftp: {e}");
        }
        let ok = self.reply().code / 100 == 2;
        if let Ok(n) = result
            && n > 0
            && self.verbose
        {
            let kbytes_per_sec = n as f64 / 1000.0 / secs.max(0.001);
            println!(
                "ftp: {n} bytes {direction} in {secs:.2}Seconds {kbytes_per_sec:.2}Kbytes/sec."
            );
        }
        if self.bell {
            print!("\x07");
        }
        ok && result.is_ok()
    }

    /// Runs a download command. `open` creates the output only after the server accepted
    /// the command, so a failed RETR never truncates an existing local file.
    fn receive<'a>(&mut self, command: &str, open: impl FnOnce() -> io::Result<Sink<'a>>) -> bool {
        let Some(mut stream) = self.data_connection() else {
            return false;
        };
        if self.send(command) / 100 != 1 {
            return false;
        }
        let start = Instant::now();
        let ascii = !self.binary;
        let result = open().and_then(|mut out| self.copy(&mut stream, &mut *out, ascii));
        drop(stream);
        self.finish(result, start, "received")
    }

    fn get(&mut self, command: String, local: &str) {
        if local == "-" {
            self.receive(&command, || Ok(stdout_sink()));
            return;
        }
        let capacity = self.buffer_size;
        self.receive(&command, || {
            let f = File::create(local)
                .inspect_err(|_| println!("Error opening local file {local}."))?;
            Ok(Box::new(BufWriter::with_capacity(capacity, f)))
        });
    }

    fn put(&mut self, verb: &str, local: &str, remote: &str) {
        let Ok(mut f) = File::open(local) else {
            println!("Error opening local file {local}.");
            return;
        };
        let Some(mut stream) = self.data_connection() else {
            return;
        };
        if self.send(&format!("{verb} {remote}")) / 100 != 1 {
            return;
        }
        let start = Instant::now();
        let ascii = !self.binary;
        let result = self.copy(&mut f, &mut stream, ascii);
        drop(stream); // The server needs EOF before it confirms the upload.
        self.finish(result, start, "sent");
    }

    fn remote_glob(&mut self, pattern: &str) -> Vec<String> {
        if !self.glob {
            return vec![pattern.into()];
        }
        let mut listing = Vec::new();
        let was_verbose = std::mem::replace(&mut self.verbose, false);
        self.receive(&format!("NLST {pattern}"), || Ok(Box::new(&mut listing)));
        self.verbose = was_verbose;
        String::from_utf8_lossy(&listing)
            .lines()
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect()
    }

    fn local_glob(&self, pattern: &str) -> Vec<String> {
        if !self.glob || !pattern.contains(['*', '?']) {
            return vec![pattern.into()];
        }
        let path = Path::new(pattern);
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let Some(file_pattern) = path.file_name().and_then(|name| name.to_str()) else {
            return vec![];
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return vec![];
        };
        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
            if let Ok(name) = entry.file_name().into_string()
                && is_file
                && wildcard_match(file_pattern.as_bytes(), name.as_bytes())
            {
                matches.push(path.with_file_name(name).to_string_lossy().into_owned());
            }
        }
        matches.sort();
        matches
    }

    fn status(&self) {
        if self.control.is_some() {
            println!("Connected to {}.", self.host);
        } else {
            println!("Not connected.");
        }
        println!(
            "Type: {}; Verbose: {} ; Bell: {} ; Prompting: {} ; Globbing: {}",
            if self.binary { "binary" } else { "ascii" },
            on_off(self.verbose),
            on_off(self.bell),
            on_off(self.prompt),
            on_off(self.glob)
        );
        println!(
            "Debugging: {} ; Hash mark printing: {} .",
            on_off(self.debug),
            on_off(self.hash)
        );
    }

    fn run(&mut self, name: &str, rest: &str, mut args: Vec<String>) {
        if NEEDS_CONNECTION.contains(&name) && self.control.is_none() {
            println!("Not connected.");
            return;
        }
        match name {
            "!" => shell(rest),
            "?" | "help" => help(args.first().map(String::as_str)),
            "append" | "put" | "send" => {
                if self.require_args(&mut args, 1, &["Local file ", "Remote file "]) {
                    let verb = if name == "append" { "APPE" } else { "STOR" };
                    self.put(verb, &args[0], args.get(1).unwrap_or(&args[0]));
                }
            }
            "ascii" => self.set_binary(false),
            "binary" => self.set_binary(true),
            "bell" => toggle(&mut self.bell, "Bell mode"),
            "bye" | "quit" => self.quit(),
            "cd" => {
                if self.require_args(&mut args, 1, &["Remote directory "]) {
                    self.send(&format!("CWD {}", args[0]));
                }
            }
            "close" | "disconnect" => self.close(),
            "debug" => toggle(&mut self.debug, "Debugging"),
            "delete" => {
                if self.require_args(&mut args, 1, &["Remote file "]) {
                    self.send(&format!("DELE {}", args[0]));
                }
            }
            "dir" | "ls" => {
                let verb = if name == "dir" { "LIST" } else { "NLST" };
                let command = args
                    .first()
                    .map_or(verb.into(), |dir| format!("{verb} {dir}"));
                self.get(command, args.get(1).map_or("-", |s| s));
            }
            "get" | "recv" => {
                if self.require_args(&mut args, 1, &["Remote file ", "Local file "]) {
                    self.get(format!("RETR {}", args[0]), args.get(1).unwrap_or(&args[0]));
                }
            }
            "glob" => toggle(&mut self.glob, "Globbing"),
            "hash" => {
                self.hash = !self.hash;
                if self.hash {
                    println!("Hash mark printing On  ftp: ({HASH_BYTES} bytes/hash mark) .");
                } else {
                    println!("Hash mark printing Off .");
                }
            }
            "lcd" => {
                if let Some(dir) = args.first()
                    && let Err(e) = env::set_current_dir(dir)
                {
                    println!("{dir}: {e}");
                    return;
                }
                let cwd = env::current_dir().unwrap_or_default();
                println!("Local directory now {}.", cwd.display());
            }
            "literal" | "quote" => {
                let line = if rest.is_empty() {
                    self.ask("Command line to send ")
                } else {
                    Some(rest.into())
                };
                if let Some(line) = line.filter(|l| !l.trim().is_empty()) {
                    let mut code = self.send(&line);
                    while code / 100 == 1 {
                        code = self.reply().code;
                    }
                }
            }
            "mdelete" | "mget" => {
                if !self.require_args(&mut args, 1, &["Remote files "]) {
                    return;
                }
                for pattern in &args {
                    for file in self.remote_glob(pattern) {
                        match self.confirm(name, &file) {
                            Answer::Yes if name == "mget" => {
                                if let Some(local) = mget_target(&file) {
                                    self.get(format!("RETR {file}"), local);
                                }
                            }
                            Answer::Yes => _ = self.send(&format!("DELE {file}")),
                            Answer::No => {}
                            Answer::Quit => return,
                        }
                    }
                }
            }
            "mdir" | "mls" => {
                if !self.require_args(&mut args, 2, &["Remote files ", "Local file "]) {
                    return;
                }
                let local = args.pop().unwrap_or_default();
                if local != "-"
                    && !matches!(self.confirm("output to local-file:", &local), Answer::Yes)
                {
                    return;
                }
                let mut out: Sink = if local == "-" {
                    stdout_sink()
                } else {
                    match File::create(&local) {
                        Ok(f) => Box::new(BufWriter::new(f)),
                        Err(_) => {
                            println!("Error opening local file {local}.");
                            return;
                        }
                    }
                };
                let verb = if name == "mdir" { "LIST" } else { "NLST" };
                for remote in &args {
                    self.receive(&format!("{verb} {remote}"), || Ok(Box::new(&mut out)));
                }
            }
            "mkdir" => {
                if self.require_args(&mut args, 1, &["Directory name "]) {
                    self.send(&format!("XMKD {}", args[0]));
                }
            }
            "mput" => {
                if !self.require_args(&mut args, 1, &["Local files "]) {
                    return;
                }
                for pattern in &args {
                    for file in self.local_glob(pattern) {
                        match self.confirm(name, &file) {
                            Answer::Yes => self.put("STOR", &file, &file),
                            Answer::No => {}
                            Answer::Quit => return,
                        }
                    }
                }
            }
            "open" => self.open(args),
            "prompt" => toggle(&mut self.prompt, "Interactive mode"),
            "pwd" | "remotehelp" => {
                let command = match (name, args.first()) {
                    ("pwd", _) => "XPWD".to_string(),
                    (_, Some(topic)) => format!("HELP {topic}"),
                    _ => "HELP".to_string(),
                };
                let was_verbose = std::mem::replace(&mut self.verbose, true);
                self.send(&command);
                self.verbose = was_verbose;
            }
            "rename" => {
                if self.require_args(&mut args, 2, &["From name ", "To name "])
                    && self.send(&format!("RNFR {}", args[0])) == 350
                {
                    self.send(&format!("RNTO {}", args[1]));
                }
            }
            "rmdir" => {
                if self.require_args(&mut args, 1, &["Directory name "]) {
                    self.send(&format!("XRMD {}", args[0]));
                }
            }
            "status" => self.status(),
            "trace" => toggle(&mut self.trace, "Packet tracing"),
            "type" => match args.first().map(|s| s.to_lowercase()).as_deref() {
                None => println!(
                    "Using {} mode to transfer files.",
                    if self.binary { "binary" } else { "ascii" }
                ),
                Some("ascii") => self.set_binary(false),
                Some("binary" | "image") => self.set_binary(true),
                Some(mode) => println!("{mode}: unknown mode."),
            },
            "user" => {
                if self.require_args(&mut args, 1, &["Username "]) {
                    self.login(args);
                }
            }
            "verbose" => toggle(&mut self.verbose, "Verbose mode"),
            _ => println!("Invalid command."),
        }
    }
}

/// Local name for a server-supplied mget name: as-is like ftp.exe while it stays inside
/// the current directory, else only its file name, so `../x` or `C:\x` can't escape it.
fn mget_target(name: &str) -> Option<&str> {
    let path = Path::new(name);
    let inside = path
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if inside {
        return Some(name);
    }
    path.file_name()?.to_str()
}

fn on_off(b: bool) -> &'static str {
    if b { "On" } else { "Off" }
}

fn toggle(flag: &mut bool, what: &str) {
    *flag = !*flag;
    println!("{what} {} .", on_off(*flag));
}

/// Code and "more lines follow" flag if `line` starts a reply (`123 text` / `123-text`).
fn reply_start(line: &str) -> Option<(u16, bool)> {
    let code = line
        .get(..3)
        .filter(|c| c.bytes().all(|b| b.is_ascii_digit()))?
        .parse()
        .ok()?;
    match line.as_bytes().get(3) {
        None | Some(b' ') => Some((code, false)),
        Some(b'-') => Some((code, true)),
        _ => None,
    }
}

/// Port from a 227 (PASV) or 229 (EPSV) reply.
fn pasv_port(code: u16, text: &str) -> Option<u16> {
    match code {
        227 => {
            let rest = text.get(3..)?;
            let rest = rest.find('(').map_or(rest, |i| &rest[i..]); // Skip digits in the text before "(".
            let numbers: Vec<u16> = rest
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|digits| digits.parse().ok())
                .take(6)
                .collect();
            Some(numbers.get(4)? << 8 | numbers.get(5)?)
        }
        229 => text.split('|').nth(3)?.parse().ok(),
        _ => None,
    }
}

/// ASCII mode: bare LF becomes CRLF, CRLF stays (Windows line endings locally, CRLF on the wire).
fn to_crlf(chunk: &[u8], cr: &mut bool, out: &mut Vec<u8>) {
    out.clear();
    for &b in chunk {
        if b == b'\n' && !*cr {
            out.push(b'\r');
        }
        *cr = b == b'\r';
        out.push(b);
    }
}

/// Case-insensitive `*` / `?` match, as on Windows.
// ponytail: backtracking matcher, exponential on patterns with many `*`; fine for file names.
fn wildcard_match(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            wildcard_match(&pattern[1..], text)
                || (!text.is_empty() && wildcard_match(pattern, &text[1..]))
        }
        (Some(b'?'), Some(_)) => wildcard_match(&pattern[1..], &text[1..]),
        (Some(a), Some(b)) => {
            a.eq_ignore_ascii_case(b) && wildcard_match(&pattern[1..], &text[1..])
        }
        _ => false,
    }
}

/// Splits on whitespace; double quotes group words (`put "my file.txt"`).
fn split_args(line: &str) -> Vec<String> {
    let (mut args, mut current, mut quoted, mut in_arg) = (Vec::new(), String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => (quoted, in_arg) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => (current.push(c), in_arg = true).1,
        }
    }
    if in_arg {
        args.push(current);
    }
    args
}

fn lookup(name: &str) -> Result<&'static CommandInfo, &'static str> {
    if let Some(c) = COMMANDS.iter().find(|c| c.name == name) {
        return Ok(c);
    }
    let mut hits = COMMANDS.iter().filter(|c| c.name.starts_with(name));
    match (hits.next(), hits.next()) {
        (Some(c), None) => Ok(c),
        (Some(_), Some(_)) => Err("Ambiguous command."),
        _ => Err("Invalid command."),
    }
}

fn help(arg: Option<&str>) {
    if let Some(name) = arg {
        match lookup(&name.to_lowercase()) {
            Ok(c) => println!("{:<16}{}", c.name, c.help),
            Err(_) => println!("Invalid help command {name}."),
        }
        return;
    }
    println!("Commands may be abbreviated.  Commands are:\n");
    let rows = COMMANDS.len().div_ceil(5);
    for row in 0..rows {
        let line: String = COMMANDS
            .iter()
            .skip(row)
            .step_by(rows)
            .map(|c| format!("{:<16}", c.name))
            .collect();
        println!("{}", line.trim_end());
    }
}

fn shell(line: &str) {
    let line = line.trim();
    let mut command = Command::new(env::var("COMSPEC").unwrap_or("cmd.exe".into()));
    if !line.is_empty() {
        // raw_arg: cmd.exe parses its command line itself, so pass it through as typed.
        command.raw_arg(format!("/C {line}"));
    }
    command.status().ok();
}

fn set_console_echo(on: bool) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(id: u32) -> isize;
        fn GetConsoleMode(handle: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: isize, mode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_ECHO_INPUT: u32 = 4;
    // SAFETY: plain Win32 calls with a handle we just got and a valid pointer to a local u32.
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(
                handle,
                if on {
                    mode | ENABLE_ECHO_INPUT
                } else {
                    mode & !ENABLE_ECHO_INPUT
                },
            );
        }
    }
}

/// Decodes a non-UTF-8 script line with the OEM code page,
/// which is what `echo ... >> script.txt` in cmd.exe writes.
fn decode_oem(bytes: &[u8]) -> String {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MultiByteToWideChar(
            cp: u32,
            flags: u32,
            s: *const u8,
            len: i32,
            w: *mut u16,
            wn: i32,
        ) -> i32;
    }
    const CP_OEMCP: u32 = 1;
    let mut wide = vec![0u16; bytes.len()];
    // SAFETY: both pointers are valid for the lengths passed, and a code page never yields
    // more UTF-16 units than input bytes, so `w` is large enough.
    let len = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            wide.as_mut_ptr(),
            wide.len() as i32,
        )
    };
    if len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..len as usize])
}

fn main() {
    let mut ftp = Ftp::new(Box::new(io::stdin().lock()), io::stdin().is_terminal());
    let Some(CommandLine { script, host }) = ftp.parse_args(env::args().skip(1)) else {
        print!("{USAGE}");
        return;
    };
    if let Some(path) = script {
        let Ok(file) = File::open(&path) else {
            println!("Error opening script file {path}.");
            return;
        };
        ftp.input = Box::new(BufReader::new(file));
        ftp.tty = false;
    }
    if !host.is_empty() {
        ftp.open(host);
    }
    loop {
        let Some(line) = ftp.ask("ftp> ") else {
            ftp.quit()
        };
        ftp.exec(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn helpers() {
        assert_eq!(
            pasv_port(227, "227 Entering Passive Mode (10,0,0,1,195,80)."),
            Some(195 * 256 + 80)
        );
        assert_eq!(pasv_port(227, "227 =10,0,0,1,4,1"), Some(1025));
        assert_eq!(pasv_port(227, "227 Passive v2 (10,0,0,1,4,1)"), Some(1025));
        assert_eq!(
            pasv_port(229, "229 Entering Extended Passive Mode (|||6446|)"),
            Some(6446)
        );
        assert_eq!(pasv_port(500, "500 nope"), None);

        let (mut cr, mut out) = (false, Vec::new());
        to_crlf(b"a\nb\r", &mut cr, &mut out);
        assert_eq!(out, b"a\r\nb\r");
        to_crlf(b"\nc\n", &mut cr, &mut out); // CRLF split across chunks stays CRLF
        assert_eq!(out, b"\nc\r\n");

        assert!(
            wildcard_match(b"*.TXT", b"a.txt")
                && wildcard_match(b"f?o*", b"foo.bar")
                && !wildcard_match(b"*.txt", b"a.txt.bak")
        );

        assert_eq!(
            split_args(r#"put "C:\my file.txt"  x"#),
            ["put", r"C:\my file.txt", "x"]
        );
        assert_eq!(split_args(r#"a "" b"#), ["a", "", "b"]);

        assert_eq!(lookup("bi").map(|c| c.name), Ok("binary"));
        assert_eq!(lookup("dir").map(|c| c.name), Ok("dir"));
        assert_eq!(lookup("d"), Err("Ambiguous command."));
        assert_eq!(lookup("xyz"), Err("Invalid command."));
    }

    #[test]
    fn console_accepts_invalid_utf8() {
        assert_eq!(Console.write(b"B\xfccher\n").unwrap(), 7);
    }

    #[test]
    fn script_lines() {
        let script = b"\xef\xbb\xbfopen h\r\nput B\xfccher.txt\r\nbye";
        let mut f = Ftp::new(Box::new(io::Cursor::new(script.to_vec())), false);
        assert_eq!(f.ask("").as_deref(), Some("open h")); // BOM and CRLF stripped
        assert!(f.ask("").unwrap().starts_with("put B")); // non-UTF-8 doesn't end the script
        assert_eq!(f.ask("").as_deref(), Some("bye")); // last line without newline
        assert_eq!(f.ask(""), None);
    }

    #[test]
    fn flags() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut f = Ftp::new(Box::new(io::empty()), false);
        let CommandLine { script, host } = f
            .parse_args(args(&[
                "-inv",
                "-a",
                "-s:x y.txt",
                "-w:10",
                "-x:1",
                "host",
                "21",
            ]))
            .unwrap();
        assert_eq!(
            (script.as_deref(), host),
            (Some("x y.txt"), args(&["host", "21"]))
        );
        assert!(!f.prompt && !f.autologin && !f.verbose && f.buffer_size == 10);
        assert!(
            Ftp::new(Box::new(io::empty()), false)
                .parse_args(args(&["-?"]))
                .is_none()
        );
        let unknown = Ftp::new(Box::new(io::empty()), false).parse_args(args(&["-p", "host"]));
        assert!(
            unknown.is_none(),
            "like ftp.exe, unknown flags such as -p show the usage"
        );
    }

    /// Ftp wired to a local socket; returns the server end. `input` feeds prompts.
    fn connected(input: &str) -> (Ftp, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let mut f = Ftp::new(Box::new(io::Cursor::new(input.to_string())), false);
        f.control = Some(BufReader::new(c));
        (f, l.accept().unwrap().0)
    }

    /// Minimal FTP server: passive data, `files` as (name, content).
    /// `RETR stall` opens the data connection but never sends data or a final reply.
    fn fake(s: TcpStream, files: Vec<(&'static str, &'static str)>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut r = BufReader::new(s);
            let mut data = None;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let line = line.trim_end();
                let (command, arg) = line.split_once(' ').unwrap_or((line, ""));
                let reply = match command {
                    "PASV" => {
                        let l = TcpListener::bind("127.0.0.1:0").unwrap();
                        let p = l.local_addr().unwrap().port();
                        data = Some(l);
                        format!("227 ok (127,0,0,1,{},{})", p >> 8, p & 255)
                    }
                    "RETR" if arg == "stall" => {
                        r.get_mut().write_all(b"150 ok\r\n").unwrap();
                        let _data = data.take().unwrap().accept().unwrap();
                        // Hold everything open until the client gives up.
                        r.read_line(&mut String::new()).ok();
                        return;
                    }
                    "RETR" | "NLST" => match files.iter().find(|f| f.0 == arg) {
                        Some((_, body)) => {
                            r.get_mut().write_all(b"150 ok\r\n").unwrap();
                            let (mut d, _) = data.take().unwrap().accept().unwrap();
                            d.write_all(body.as_bytes()).unwrap();
                            "226 done".into()
                        }
                        None => "550 not found".into(),
                    },
                    _ => "200 ok".into(),
                };
                r.get_mut()
                    .write_all(format!("{reply}\r\n").as_bytes())
                    .unwrap();
            }
        })
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = env::temp_dir().join(format!("ftp-test-{name}-{}", std::process::id()));
        fs::remove_dir_all(&d).ok();
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn downloads() {
        let (mut f, s) = connected("");
        let files = vec![
            ("a", "new"),
            (
                "*",
                "../evil.txt\r\n/abs.txt\r\nC:\\evil\\drive.txt\r\nsub/ok.txt\r\n",
            ),
            ("C:\\evil\\drive.txt", "drive"),
            ("../evil.txt", "evil"),
            ("/abs.txt", "abs"),
            ("sub/ok.txt", "ok"),
        ];
        let srv = fake(s, files);
        f.prompt = false;
        let dir = tmpdir("get");
        let work = dir.join("work");
        fs::create_dir_all(work.join("sub")).unwrap();
        let x = work.join("x");
        fs::write(&x, "keep").unwrap();
        fs::write(work.join("x.part"), "mine").unwrap();

        f.get("RETR missing".into(), x.to_str().unwrap());
        assert_eq!(fs::read_to_string(&x).unwrap(), "keep"); // 550: local file untouched
        f.get("RETR a".into(), x.to_str().unwrap());
        assert_eq!(fs::read_to_string(&x).unwrap(), "new");
        assert_eq!(fs::read_to_string(work.join("x.part")).unwrap(), "mine");

        env::set_current_dir(&work).unwrap(); // only this test depends on the cwd
        f.exec("mget *");
        assert_eq!(fs::read_to_string(work.join("evil.txt")).unwrap(), "evil");
        assert_eq!(fs::read_to_string(work.join("abs.txt")).unwrap(), "abs");
        assert_eq!(fs::read_to_string(work.join("drive.txt")).unwrap(), "drive");
        assert_eq!(fs::read_to_string(work.join("sub/ok.txt")).unwrap(), "ok"); // Kept, like ftp.exe.
        assert!(!dir.join("evil.txt").exists()); // server names can't escape the cwd
        drop(f);
        srv.join().unwrap();
    }

    #[test]
    fn stalled_transfer_times_out() {
        let (mut f, s) = connected("");
        let srv = fake(s, vec![]);
        f.timeout = Duration::from_millis(300);
        let control = f.control.as_ref().unwrap().get_ref();
        control.set_read_timeout(Some(f.timeout)).unwrap(); // As `open` does.
        let local = tmpdir("stall").join("x");
        f.get("RETR stall".into(), local.to_str().unwrap());
        assert!(
            f.control.is_none(),
            "a silent server must not block forever"
        );
        srv.join().unwrap();
    }

    #[test]
    fn replies() {
        let (mut f, mut s) = connected("");
        s.write_all(
            b"junk line\r\n\r\n220 ready\r\n230-multi\r\n 230 not the end\r\n230\r\n331 next\r\n",
        )
        .unwrap();
        let ready = Reply {
            code: 220,
            text: "220 ready\n".into(),
        };
        assert_eq!(f.reply(), ready);
        assert_eq!(f.reply().code, 230);
        assert_eq!(f.reply().code, 331);
    }
}
