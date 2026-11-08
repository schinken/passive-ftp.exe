use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, IsTerminal, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const USAGE: &str = "
Transfers files to and from a computer running an FTP server service
(sometimes called a daemon). Ftp can be used interactively.

FTP [-v] [-d] [-i] [-n] [-g] [-p] [-s:filename] [-a] [-A] [-x:sendbuffer] [-r:recvbuffer] [-b:asyncbuffers] [-w:windowsize] [host]

  -v              Suppresses display of remote server responses.
  -n              Suppresses auto-login upon initial connection.
  -i              Turns off interactive prompting during multiple file
                  transfers.
  -d              Enables debugging.
  -g              Disables filename globbing (see GLOB command).
  -p              Uses passive mode (PASV/EPSV) for data connections.
  -s:filename     Specifies a text file containing FTP commands; the
                  commands will automatically run after FTP starts.
  -a              Use any local interface when binding data connection.
  -A              login as anonymous.
  -x:send sockbuf Accepted for compatibility, ignored.
  -r:recv sockbuf Accepted for compatibility, ignored.
  -b:async count  Accepted for compatibility, ignored.
  -w:windowsize   Overrides the default transfer buffer size of 65536.
  host            Specifies the host name or IP address of the remote
                  host to connect to.

Notes:
  - mget and mput commands take y/n/q for yes/no/quit.
";

const CMDS: &[(&str, &str)] = &[
    ("!", "escape to the shell"),
    ("?", "print local help information"),
    ("append", "append to a file"),
    ("ascii", "set ascii transfer type"),
    ("bell", "beep when command completed"),
    ("binary", "set binary transfer type"),
    ("bye", "terminate ftp session and exit"),
    ("cd", "change remote working directory"),
    ("close", "terminate ftp session"),
    ("delete", "delete remote file"),
    ("debug", "toggle debugging mode"),
    ("dir", "list contents of remote directory"),
    ("disconnect", "terminate ftp session"),
    ("get", "receive file"),
    ("glob", "toggle metacharacter expansion of local file names"),
    ("hash", "toggle printing `#' for each buffer transferred"),
    ("help", "print local help information"),
    ("lcd", "change local working directory"),
    ("literal", "send arbitrary ftp command"),
    ("ls", "nlist contents of remote directory"),
    ("mdelete", "delete multiple files"),
    ("mdir", "list contents of multiple remote directories"),
    ("mget", "get multiple files"),
    ("mkdir", "make directory on the remote machine"),
    ("mls", "nlist contents of multiple remote directories"),
    ("mput", "send multiple files"),
    ("open", "connect to remote ftp"),
    ("prompt", "force interactive prompting on multiple commands"),
    ("put", "send one file"),
    ("pwd", "print working directory on remote machine"),
    ("quit", "terminate ftp session and exit"),
    ("quote", "send arbitrary ftp command"),
    ("recv", "receive file"),
    ("remotehelp", "get help from remote server"),
    ("rename", "rename file"),
    ("rmdir", "remove directory on the remote machine"),
    ("send", "send one file"),
    ("status", "show current status"),
    ("trace", "toggle packet tracing"),
    ("type", "set file transfer type"),
    ("user", "send new user information"),
    ("verbose", "toggle verbose mode"),
];

const NEEDS_CONN: &[&str] = &[
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
    ctrl: Option<BufReader<TcpStream>>,
    host: String,
    verbose: bool,
    debug: bool,
    prompt: bool,
    glob: bool,
    hash: bool,
    bell: bool,
    trace: bool,
    passive: bool,
    any_iface: bool,
    autologin: bool,
    anon: bool,
    binary: bool,
    bufsize: usize,
    timeout: Duration,
}

enum Data {
    Listen(TcpListener),
    Conn(TcpStream),
}

impl Data {
    /// Waits at most `t` for the server's data connection; reads and writes then time out after `t`.
    fn open(self, t: Duration) -> io::Result<TcpStream> {
        let s = match self {
            Data::Conn(s) => s,
            Data::Listen(l) => {
                // ponytail: polls accept() every 5 ms (std has no accept timeout); WSAPoll would avoid the wakeups.
                l.set_nonblocking(true)?;
                let end = Instant::now() + t;
                loop {
                    match l.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() != io::ErrorKind::WouldBlock => return Err(e),
                        Err(_) if Instant::now() >= end => {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "no data connection from server",
                            ));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            }
        };
        s.set_nonblocking(false)?; // accepted sockets inherit non-blocking mode on Windows
        s.set_read_timeout(Some(t))?;
        s.set_write_timeout(Some(t))?;
        Ok(s)
    }
}

impl Ftp {
    fn new(input: Box<dyn BufRead>, tty: bool) -> Self {
        Ftp {
            input,
            tty,
            ctrl: None,
            host: String::new(),
            verbose: true,
            debug: false,
            prompt: true,
            glob: true,
            hash: false,
            bell: false,
            trace: false,
            passive: false,
            any_iface: false,
            autologin: true,
            anon: false,
            binary: false,
            bufsize: 65536,
            timeout: Duration::from_secs(60),
        }
    }

    /// Applies ftp.exe flags. Returns (script, host args), or None to show usage.
    fn parse_args(
        &mut self,
        args: impl IntoIterator<Item = String>,
    ) -> Option<(Option<String>, Vec<String>)> {
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
                    'a' => self.any_iface = true,
                    'A' => self.anon = true,
                    'p' => self.passive = true,
                    's' | 'w' | 'x' | 'r' | 'b' => {
                        let v = opts[i + 1..].trim_start_matches(':');
                        match c {
                            's' => script = Some(v.to_string()),
                            'w' => self.bufsize = v.parse().unwrap_or(self.bufsize).max(1),
                            _ => {} // -x/-r/-b socket tuning: the OS auto-tunes better.
                        }
                        break;
                    }
                    _ => return None,
                }
            }
        }
        Some((script, host))
    }

    /// Runs one command line.
    fn exec(&mut self, line: &str) {
        let line = line.trim();
        let (name, rest) = match line.strip_prefix('!') {
            Some(r) => ("!".to_string(), r),
            None => {
                let (n, r) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
                (n.to_lowercase(), r.trim())
            }
        };
        if name.is_empty() {
            return;
        }
        match lookup(&name) {
            Ok(i) => self.run(CMDS[i].0, rest, split_args(rest)),
            Err(e) => println!("{e}"),
        }
    }

    /// Reads one line from the command source (console or -s script).
    fn ask(&mut self, prompt: &str, secret: bool) -> Option<String> {
        print!("{prompt}");
        io::stdout().flush().ok();
        let hide = secret && self.tty;
        if hide {
            console_echo(false);
        }
        let mut buf = Vec::new();
        let n = self.input.read_until(b'\n', &mut buf);
        if hide {
            console_echo(true);
        }
        if !matches!(n, Ok(1..)) {
            return None;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line
            .trim_start_matches('\u{feff}')
            .trim_end_matches(['\r', '\n']);
        // Like ftp.exe: echo script input so logs show what ran.
        if hide || !self.tty {
            println!("{}", if secret { "" } else { line });
        }
        Some(line.to_string())
    }

    /// Prompts for missing arguments: all prompts if none given, else up to `need`.
    fn need(&mut self, a: &mut Vec<String>, need: usize, prompts: &[&str]) -> bool {
        let want = if a.is_empty() { prompts.len() } else { need };
        while a.len() < want {
            match self.ask(prompts[a.len()], false) {
                Some(s) if !s.trim().is_empty() => a.push(s.trim().trim_matches('"').to_string()),
                _ => break,
            }
        }
        a.len() >= need
    }

    fn confirm(&mut self, what: &str, name: &str) -> Option<bool> {
        if !self.prompt {
            return Some(true);
        }
        let a = self.ask(&format!("{what} {name}? "), false)?;
        match a.trim_start().chars().next() {
            Some('n' | 'N') => Some(false),
            Some('q' | 'Q') => None,
            _ => Some(true),
        }
    }

    fn lost(&mut self, why: &str) {
        println!("{why}");
        self.ctrl = None;
    }

    fn reply(&mut self) -> (u16, String) {
        let mut text = String::new();
        let mut code = 0;
        loop {
            let Some(r) = self.ctrl.as_mut() else {
                return (0, text);
            };
            let mut buf = Vec::new();
            match r.read_until(b'\n', &mut buf) {
                Ok(1..) => {}
                Ok(0) => {
                    self.lost("Connection closed by remote host.");
                    return (0, text);
                }
                Err(e) => {
                    self.lost(&format!("> ftp: {e}"));
                    return (0, text);
                }
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\r', '\n']);
            let done = match (code, reply_start(line)) {
                (0, None) => false, // stray line before a reply: show it, keep waiting
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
                return (code, text);
            }
        }
    }

    fn cmd(&mut self, line: &str) -> (u16, String) {
        let Some(r) = self.ctrl.as_mut() else {
            println!("Not connected.");
            return (0, String::new());
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
        if r.get_mut()
            .write_all(format!("{line}\r\n").as_bytes())
            .is_err()
        {
            self.lost("Connection closed by remote host.");
            return (0, String::new());
        }
        self.reply()
    }

    fn code(&mut self, line: &str) -> u16 {
        self.cmd(line).0
    }

    fn open(&mut self, mut a: Vec<String>) {
        if self.ctrl.is_some() {
            println!("Already connected to {}, use disconnect first.", self.host);
            return;
        }
        if !self.need(&mut a, 1, &["To "]) {
            return;
        }
        let host = a[0].clone();
        let port = a.get(1).and_then(|p| p.parse().ok()).unwrap_or(21);
        let addrs: Vec<SocketAddr> = match (host.as_str(), port).to_socket_addrs() {
            Ok(it) => it.collect(),
            Err(_) => {
                println!("Unknown host {host}.");
                return;
            }
        };
        let s = match TcpStream::connect(&addrs[..]) {
            Ok(s) => s,
            Err(e) => {
                println!("> ftp: connect :{e}");
                return;
            }
        };
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(self.timeout)).ok();
        s.set_write_timeout(Some(self.timeout)).ok();
        println!("Connected to {host}.");
        self.ctrl = Some(BufReader::new(s));
        self.host = host;
        self.binary = false;
        if self.reply().0 / 100 != 2 {
            self.ctrl = None;
            return;
        }
        self.cmd("OPTS UTF8 ON");
        if self.autologin {
            self.login(Vec::new());
        }
    }

    fn login(&mut self, mut a: Vec<String>) {
        if a.is_empty() {
            if self.anon {
                let me = env::var("USERNAME").unwrap_or_default();
                let host = env::var("COMPUTERNAME").unwrap_or_default();
                a = vec!["anonymous".into(), format!("{me}@{host}")];
            } else {
                let p = format!("User ({}:(none)): ", self.host);
                let Some(u) = self.ask(&p, false) else { return };
                a.push(u);
            }
        }
        let mut code = self.code(&format!("USER {}", a[0]));
        if code == 331 {
            let pass = match a.get(1) {
                Some(p) => p.clone(),
                None => match self.ask("Password:", true) {
                    Some(p) => p,
                    None => return,
                },
            };
            code = self.code(&format!("PASS {pass}"));
        }
        if code == 332 {
            let acct = match a.get(2) {
                Some(p) => p.clone(),
                None => match self.ask("Account:", false) {
                    Some(p) => p,
                    None => return,
                },
            };
            code = self.code(&format!("ACCT {acct}"));
        }
        if code / 100 != 2 {
            println!("Login failed.");
        }
    }

    fn close(&mut self) {
        if self.ctrl.is_some() {
            self.code("QUIT");
            self.ctrl = None;
        }
    }

    fn quit(&mut self) -> ! {
        self.close();
        std::process::exit(0)
    }

    fn set_type(&mut self, binary: bool) {
        if self.code(if binary { "TYPE I" } else { "TYPE A" }) / 100 == 2 {
            self.binary = binary;
        }
    }

    /// Prepares the data connection: PASV/EPSV when passive, PORT/EPRT otherwise.
    fn data(&mut self) -> Option<Data> {
        let s = self.ctrl.as_ref()?.get_ref();
        let (local, peer) = (s.local_addr().ok()?, s.peer_addr().ok()?);
        if self.passive {
            let (code, text) = self.cmd(if peer.is_ipv4() { "PASV" } else { "EPSV" });
            let port = pasv_port(code, &text)?;
            // The address in a PASV reply is ignored (NAT-safe, like curl): always use the control peer.
            return match TcpStream::connect((peer.ip(), port)) {
                Ok(d) => Some(Data::Conn(d)),
                Err(e) => {
                    println!("> ftp: connect :{e}");
                    None
                }
            };
        }
        let bind: IpAddr = match (self.any_iface, local.ip()) {
            (false, ip) => ip,
            (true, IpAddr::V4(_)) => Ipv4Addr::UNSPECIFIED.into(),
            (true, IpAddr::V6(_)) => Ipv6Addr::UNSPECIFIED.into(),
        };
        let l = match TcpListener::bind((bind, 0)) {
            Ok(l) => l,
            Err(e) => {
                println!("> ftp: bind :{e}");
                return None;
            }
        };
        let p = l.local_addr().ok()?.port();
        let cmd = match local.ip() {
            IpAddr::V4(ip) => {
                let o = ip.octets();
                format!(
                    "PORT {},{},{},{},{},{}",
                    o[0],
                    o[1],
                    o[2],
                    o[3],
                    p >> 8,
                    p & 255
                )
            }
            IpAddr::V6(ip) => format!("EPRT |2|{ip}|{p}|"),
        };
        (self.code(&cmd) / 100 == 2).then_some(Data::Listen(l))
    }

    fn copy(&self, r: &mut dyn Read, w: &mut dyn Write, ascii: bool) -> io::Result<u64> {
        let mut buf = vec![0u8; self.bufsize];
        let mut conv = Vec::new();
        let (mut total, mut marks, mut cr) = (0u64, 0u64, false);
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if ascii {
                to_crlf(&buf[..n], &mut cr, &mut conv);
                w.write_all(&conv)?;
            } else {
                w.write_all(&buf[..n])?;
            }
            total += n as u64;
            if self.hash && total / HASH_BYTES > marks {
                print!("{}", "#".repeat((total / HASH_BYTES - marks) as usize));
                io::stdout().flush()?;
                marks = total / HASH_BYTES;
            }
        }
        w.flush()?;
        if marks > 0 {
            println!();
        }
        Ok(total)
    }

    fn finish(&mut self, res: io::Result<u64>, start: Instant, dir: &str) -> bool {
        let secs = start.elapsed().as_secs_f64();
        if let Err(e) = &res {
            println!("> ftp: {e}");
        }
        let ok = self.reply().0 / 100 == 2;
        if let Ok(n) = res
            && n > 0
            && self.verbose
        {
            let kbs = n as f64 / 1000.0 / secs.max(0.001);
            println!("ftp: {n} bytes {dir} in {secs:.2}Seconds {kbs:.2}Kbytes/sec.");
        }
        if self.bell {
            print!("\x07");
        }
        ok && res.is_ok()
    }

    fn recv(&mut self, cmd: &str, out: &mut dyn Write) -> bool {
        let Some(data) = self.data() else {
            return false;
        };
        if self.code(cmd) / 100 != 1 {
            return false;
        }
        let start = Instant::now();
        let ascii = !self.binary;
        let res = data
            .open(self.timeout)
            .and_then(|mut s| self.copy(&mut s, out, ascii));
        self.finish(res, start, "received")
    }

    fn get(&mut self, cmd: String, local: &str) {
        if local == "-" {
            self.recv(&cmd, &mut io::stdout());
            return;
        }
        // Write to a temp file so a failed transfer never clobbers an existing file.
        let tmp = format!("{local}.part");
        let Ok(f) = File::create(&tmp) else {
            println!("Error opening local file {local}.");
            return;
        };
        let mut f = BufWriter::with_capacity(self.bufsize, f);
        let ok = self.recv(&cmd, &mut f);
        drop(f);
        if ok && fs::rename(&tmp, local).is_ok() {
            return;
        }
        fs::remove_file(&tmp).ok();
        if ok {
            println!("Error opening local file {local}.");
        }
    }

    fn put(&mut self, verb: &str, local: &str, remote: &str) {
        let Ok(mut f) = File::open(local) else {
            println!("Error opening local file {local}.");
            return;
        };
        let Some(data) = self.data() else { return };
        if self.code(&format!("{verb} {remote}")) / 100 != 1 {
            return;
        }
        let start = Instant::now();
        let ascii = !self.binary;
        let res = data
            .open(self.timeout)
            .and_then(|mut s| self.copy(&mut f, &mut s, ascii));
        self.finish(res, start, "sent");
    }

    fn remote_glob(&mut self, pat: &str) -> Vec<String> {
        if !self.glob {
            return vec![pat.into()];
        }
        let mut buf = Vec::new();
        let v = std::mem::replace(&mut self.verbose, false);
        self.recv(&format!("NLST {pat}"), &mut buf);
        self.verbose = v;
        String::from_utf8_lossy(&buf)
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    fn local_glob(&self, pat: &str) -> Vec<String> {
        if !self.glob || !pat.contains(['*', '?']) {
            return vec![pat.into()];
        }
        let p = Path::new(pat);
        let dir = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let Some(fpat) = p.file_name().and_then(|f| f.to_str()) else {
            return vec![];
        };
        let mut v: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| wild(fpat.as_bytes(), n.as_bytes()))
            .map(|n| p.with_file_name(n).to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn status(&self) {
        if self.ctrl.is_some() {
            println!("Connected to {}.", self.host);
        } else {
            println!("Not connected.");
        }
        println!(
            "Type: {}; Verbose: {} ; Bell: {} ; Prompting: {} ; Globbing: {}",
            if self.binary { "binary" } else { "ascii" },
            on(self.verbose),
            on(self.bell),
            on(self.prompt),
            on(self.glob)
        );
        println!(
            "Debugging: {} ; Hash mark printing: {} ; Passive: {} .",
            on(self.debug),
            on(self.hash),
            on(self.passive)
        );
    }

    fn run(&mut self, name: &str, rest: &str, mut a: Vec<String>) {
        if NEEDS_CONN.contains(&name) && self.ctrl.is_none() {
            println!("Not connected.");
            return;
        }
        match name {
            "!" => shell(rest),
            "?" | "help" => help(a.first()),
            "append" | "put" | "send" => {
                if self.need(&mut a, 1, &["Local file ", "Remote file "]) {
                    let verb = if name == "append" { "APPE" } else { "STOR" };
                    self.put(verb, &a[0], a.get(1).unwrap_or(&a[0]));
                }
            }
            "ascii" => self.set_type(false),
            "binary" => self.set_type(true),
            "bell" => toggle(&mut self.bell, "Bell mode"),
            "bye" | "quit" => self.quit(),
            "cd" => {
                if self.need(&mut a, 1, &["Remote directory "]) {
                    self.code(&format!("CWD {}", a[0]));
                }
            }
            "close" | "disconnect" => self.close(),
            "debug" => toggle(&mut self.debug, "Debugging"),
            "delete" => {
                if self.need(&mut a, 1, &["Remote file "]) {
                    self.code(&format!("DELE {}", a[0]));
                }
            }
            "dir" | "ls" => {
                let verb = if name == "dir" { "LIST" } else { "NLST" };
                let cmd = a.first().map_or(verb.into(), |d| format!("{verb} {d}"));
                self.get(cmd, a.get(1).map_or("-", |s| s));
            }
            "get" | "recv" => {
                if self.need(&mut a, 1, &["Remote file ", "Local file "]) {
                    self.get(format!("RETR {}", a[0]), a.get(1).unwrap_or(&a[0]));
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
                if let Some(d) = a.first()
                    && let Err(e) = env::set_current_dir(d)
                {
                    println!("{d}: {e}");
                    return;
                }
                let cwd = env::current_dir().unwrap_or_default();
                println!("Local directory now {}.", cwd.display());
            }
            "literal" | "quote" => {
                let line = if rest.is_empty() {
                    self.ask("Command line to send ", false)
                } else {
                    Some(rest.into())
                };
                if let Some(line) = line.filter(|l| !l.trim().is_empty()) {
                    let mut code = self.code(&line);
                    while code / 100 == 1 {
                        code = self.reply().0;
                    }
                }
            }
            "mdelete" | "mget" => {
                if !self.need(&mut a, 1, &["Remote files "]) {
                    return;
                }
                for pat in &a {
                    for f in self.remote_glob(pat) {
                        match self.confirm(name, &f) {
                            Some(true) if name == "mget" => self.get(format!("RETR {f}"), &f),
                            Some(true) => _ = self.code(&format!("DELE {f}")),
                            Some(false) => {}
                            None => return,
                        }
                    }
                }
            }
            "mdir" | "mls" => {
                if !self.need(&mut a, 2, &["Remote files ", "Local file "]) {
                    return;
                }
                let local = a.pop().unwrap_or_default();
                if local != "-" && self.confirm("output to local-file:", &local) != Some(true) {
                    return;
                }
                let mut out: Box<dyn Write> = if local == "-" {
                    Box::new(io::stdout())
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
                for r in &a {
                    self.recv(&format!("{verb} {r}"), &mut out);
                }
            }
            "mkdir" => {
                if self.need(&mut a, 1, &["Directory name "]) {
                    self.code(&format!("XMKD {}", a[0]));
                }
            }
            "mput" => {
                if !self.need(&mut a, 1, &["Local files "]) {
                    return;
                }
                for pat in &a {
                    for f in self.local_glob(pat) {
                        match self.confirm(name, &f) {
                            Some(true) => self.put("STOR", &f, &f),
                            Some(false) => {}
                            None => return,
                        }
                    }
                }
            }
            "open" => self.open(a),
            "prompt" => toggle(&mut self.prompt, "Interactive mode"),
            "pwd" | "remotehelp" => {
                let cmd = match (name, a.first()) {
                    ("pwd", _) => "XPWD".to_string(),
                    (_, Some(c)) => format!("HELP {c}"),
                    _ => "HELP".to_string(),
                };
                let v = std::mem::replace(&mut self.verbose, true);
                self.code(&cmd);
                self.verbose = v;
            }
            "rename" => {
                if self.need(&mut a, 2, &["From name ", "To name "])
                    && self.code(&format!("RNFR {}", a[0])) == 350
                {
                    self.code(&format!("RNTO {}", a[1]));
                }
            }
            "rmdir" => {
                if self.need(&mut a, 1, &["Directory name "]) {
                    self.code(&format!("XRMD {}", a[0]));
                }
            }
            "status" => self.status(),
            "trace" => toggle(&mut self.trace, "Packet tracing"),
            "type" => match a.first().map(|s| s.to_lowercase()).as_deref() {
                None => println!(
                    "Using {} mode to transfer files.",
                    if self.binary { "binary" } else { "ascii" }
                ),
                Some("ascii") => self.set_type(false),
                Some("binary" | "image") => self.set_type(true),
                Some(m) => println!("{m}: unknown mode."),
            },
            "user" => {
                if self.need(&mut a, 1, &["Username "]) {
                    self.login(a);
                }
            }
            "verbose" => toggle(&mut self.verbose, "Verbose mode"),
            _ => println!("Invalid command."),
        }
    }
}

fn on(b: bool) -> &'static str {
    if b { "On" } else { "Off" }
}

fn toggle(flag: &mut bool, what: &str) {
    *flag = !*flag;
    println!("{what} {} .", on(*flag));
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
            let n: Vec<u16> = text[3..]
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|x| x.parse().ok())
                .take(6)
                .collect();
            Some(n.get(4)? << 8 | n.get(5)?)
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
fn wild(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => wild(&p[1..], s) || (!s.is_empty() && wild(p, &s[1..])),
        (Some(b'?'), Some(_)) => wild(&p[1..], &s[1..]),
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b) && wild(&p[1..], &s[1..]),
        _ => false,
    }
}

/// Splits on whitespace; double quotes group words (`put "my file.txt"`).
fn split_args(s: &str) -> Vec<String> {
    let (mut v, mut cur, mut quoted, mut any) = (Vec::new(), String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => (quoted, any) = (!quoted, true),
            c if c.is_whitespace() && !quoted => {
                if any {
                    v.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => (cur.push(c), any = true).1,
        }
    }
    if any {
        v.push(cur);
    }
    v
}

fn lookup(name: &str) -> Result<usize, &'static str> {
    if let Some(i) = CMDS.iter().position(|c| c.0 == name) {
        return Ok(i);
    }
    let mut hits = CMDS
        .iter()
        .enumerate()
        .filter(|(_, c)| c.0.starts_with(name));
    match (hits.next(), hits.next()) {
        (Some((i, _)), None) => Ok(i),
        (Some(_), Some(_)) => Err("Ambiguous command."),
        _ => Err("Invalid command."),
    }
}

fn help(arg: Option<&String>) {
    if let Some(c) = arg {
        match lookup(&c.to_lowercase()) {
            Ok(i) => println!("{:<16}{}", CMDS[i].0, CMDS[i].1),
            Err(_) => println!("Invalid help command {c}."),
        }
        return;
    }
    println!("Commands may be abbreviated.  Commands are:\n");
    let rows = CMDS.len().div_ceil(5);
    for r in 0..rows {
        let line: String = (r..CMDS.len())
            .step_by(rows)
            .map(|i| format!("{:<16}", CMDS[i].0))
            .collect();
        println!("{}", line.trim_end());
    }
}

fn shell(line: &str) {
    let line = line.trim();
    #[cfg(windows)]
    let mut c = Command::new(env::var("COMSPEC").unwrap_or("cmd.exe".into()));
    #[cfg(windows)]
    if !line.is_empty() {
        use std::os::windows::process::CommandExt;
        c.raw_arg(format!("/C {line}"));
    }
    #[cfg(not(windows))]
    let mut c = Command::new("sh");
    #[cfg(not(windows))]
    if !line.is_empty() {
        c.args(["-c", line]);
    }
    c.status().ok();
}

#[cfg(windows)]
fn console_echo(on: bool) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(id: u32) -> isize;
        fn GetConsoleMode(h: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: isize, mode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_ECHO_INPUT: u32 = 4;
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(h, &mut mode) != 0 {
            SetConsoleMode(
                h,
                if on {
                    mode | ENABLE_ECHO_INPUT
                } else {
                    mode & !ENABLE_ECHO_INPUT
                },
            );
        }
    }
}

#[cfg(not(windows))]
fn console_echo(_on: bool) {}

fn main() {
    let mut f = Ftp::new(Box::new(io::stdin().lock()), io::stdin().is_terminal());
    let Some((script, host)) = f.parse_args(env::args().skip(1)) else {
        print!("{USAGE}");
        return;
    };
    if let Some(s) = script {
        let Ok(file) = File::open(&s) else {
            println!("Error opening script file {s}.");
            return;
        };
        f.input = Box::new(BufReader::new(file));
        f.tty = false;
    }
    if !host.is_empty() {
        f.open(host);
    }
    loop {
        let Some(line) = f.ask("ftp> ", false) else {
            f.quit()
        };
        f.exec(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            pasv_port(227, "227 Entering Passive Mode (10,0,0,1,195,80)."),
            Some(195 * 256 + 80)
        );
        assert_eq!(pasv_port(227, "227 =10,0,0,1,4,1"), Some(1025));
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
            wild(b"*.TXT", b"a.txt") && wild(b"f?o*", b"foo.bar") && !wild(b"*.txt", b"a.txt.bak")
        );

        assert_eq!(
            split_args(r#"put "C:\my file.txt"  x"#),
            ["put", r"C:\my file.txt", "x"]
        );
        assert_eq!(split_args(r#"a "" b"#), ["a", "", "b"]);

        assert_eq!(lookup("bi").map(|i| CMDS[i].0), Ok("binary"));
        assert_eq!(lookup("dir").map(|i| CMDS[i].0), Ok("dir"));
        assert_eq!(lookup("d"), Err("Ambiguous command."));
        assert_eq!(lookup("xyz"), Err("Invalid command."));
    }

    #[test]
    fn flags() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut f = Ftp::new(Box::new(io::empty()), false);
        let (script, host) = f
            .parse_args(args(&[
                "-inv",
                "-p",
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
        assert!(!f.prompt && !f.autologin && !f.verbose && f.passive && f.bufsize == 10);
        assert!(
            Ftp::new(Box::new(io::empty()), false)
                .parse_args(args(&["-?"]))
                .is_none()
        );
    }

    /// Ftp wired to a local socket; returns the server end. `input` feeds prompts.
    fn connected(input: &str) -> (Ftp, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let mut f = Ftp::new(Box::new(io::Cursor::new(input.to_string())), false);
        f.ctrl = Some(BufReader::new(c));
        (f, l.accept().unwrap().0)
    }

    /// Minimal FTP server: passive data, `files` as (name, content).
    /// `RETR hang` answers 150 then 425 and never connects (a firewalled active-mode transfer).
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
                let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
                let reply = match cmd {
                    "PASV" => {
                        let l = TcpListener::bind("127.0.0.1:0").unwrap();
                        let p = l.local_addr().unwrap().port();
                        data = Some(l);
                        format!("227 ok (127,0,0,1,{},{})", p >> 8, p & 255)
                    }
                    "RETR" if arg == "hang" => "150 ok\r\n425 no connection".into(),
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
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn active_mode_gives_up_when_server_never_connects() {
        let (mut f, s) = connected("");
        let srv = fake(s, vec![]);
        f.timeout = Duration::from_millis(500);
        let local = tmpdir("hang").join("x");
        f.get("RETR hang".into(), local.to_str().unwrap()); // used to block in accept() forever
        assert!(!local.exists());
        drop(f);
        srv.join().unwrap();
    }

    #[test]
    fn replies() {
        let (mut f, mut s) = connected("");
        s.write_all(
            b"junk line\r\n\r\n220 ready\r\n230-multi\r\n 230 not the end\r\n230\r\n331 next\r\n",
        )
        .unwrap();
        assert_eq!(f.reply(), (220, "220 ready\n".into()));
        assert_eq!(f.reply().0, 230);
        assert_eq!(f.reply().0, 331);
    }
}
