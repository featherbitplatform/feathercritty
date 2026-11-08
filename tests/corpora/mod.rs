// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Realistic terminal output for tests and measurements of the history store (design spec 5.3,
//! 6.3): three seeded generators and two recorded streams. Every function returns the bytes to
//! feed a `Term` through `Processor::advance`, with SGR colors and one row per line (lines end in
//! `\r\n`, as a pty delivers them). All are deterministic per seed. Included with
//! `mod corpora;` by the tests that use it, so not every test uses every function.

#![allow(dead_code)]

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

const RESET: &str = "\x1b[0m";

fn rng(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(seed)
}

fn pick<'a>(rng: &mut ChaCha8Rng, list: &[&'a str]) -> &'a str {
    list[rng.gen_range(0..list.len())]
}

const CRATES: &[&str] = &[
    "serde",
    "serde_derive",
    "tokio",
    "tokio-util",
    "bytes",
    "futures-core",
    "futures-util",
    "hashbrown",
    "indexmap",
    "libc",
    "log",
    "memchr",
    "once_cell",
    "parking_lot",
    "proc-macro2",
    "quote",
    "regex",
    "regex-syntax",
    "syn",
    "smallvec",
    "thiserror",
    "unicode-ident",
    "uuid",
    "rand",
    "rand_core",
    "rand_chacha",
    "ryu",
    "itoa",
    "base64",
    "bitflags",
    "cfg-if",
    "either",
    "fnv",
    "http",
    "httparse",
    "hyper",
    "lazy_static",
    "mio",
    "num-traits",
    "pin-project-lite",
    "socket2",
    "tracing",
    "tracing-core",
    "zeroize",
    "chrono",
    "clap",
    "anyhow",
    "crc32fast",
];

const SRC_PATHS: &[&str] = &[
    "src/lib.rs",
    "src/main.rs",
    "src/config.rs",
    "src/server/mod.rs",
    "src/server/conn.rs",
    "src/server/router.rs",
    "src/client/pool.rs",
    "src/client/retry.rs",
    "src/codec/frame.rs",
    "src/codec/decode.rs",
    "src/store/cache.rs",
    "src/store/disk.rs",
    "src/util/time.rs",
    "src/util/buf.rs",
    "src/metrics.rs",
    "src/error.rs",
    "src/auth/token.rs",
    "src/auth/jwt.rs",
];

const VARS: &[&str] = &[
    "buf", "len", "idx", "conn", "state", "req", "resp", "key", "value", "ctx", "cfg", "tmp",
    "offset", "count", "guard", "handle", "peer", "addr", "elapsed", "limit",
];

/// A cargo build log: `Compiling` lines and `warning: unused variable` blocks with source
/// excerpts, gutters and carets, in cargo's colors.
pub fn build_log(seed: u64, rows: usize) -> Vec<u8> {
    let mut rng = rng(seed);
    let mut out = String::new();
    let mut n = 0;
    while n < rows {
        if rng.gen_range(0..100) < 82 {
            let name = pick(&mut rng, CRATES);
            let (x, y, z) = (rng.gen_range(0..3), rng.gen_range(0..30), rng.gen_range(0..20));
            out.push_str(&format!("\x1b[1m\x1b[92m   Compiling\x1b[0m {name} v{x}.{y}.{z}\r\n"));
            n += 1;
        } else {
            let var = pick(&mut rng, VARS);
            let path = pick(&mut rng, SRC_PATHS);
            let line = rng.gen_range(10..900);
            let col = rng.gen_range(5..40);
            let code = rng.gen_range(0..200);
            let lines = [
                format!("\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: unused variable: `{var}`\x1b[0m"),
                format!("\x1b[1m\x1b[38;5;12m   --> \x1b[0m{path}:{line}:{col}"),
                "\x1b[1m\x1b[38;5;12m    |\x1b[0m".to_owned(),
                format!(
                    "\x1b[1m\x1b[38;5;12m{line:>3} |\x1b[0m         let {var} = self.read_{}({code});",
                    pick(&mut rng, VARS)
                ),
                format!(
                    "\x1b[1m\x1b[38;5;12m    |\x1b[0m             \x1b[1m\x1b[33m{}\x1b[0m",
                    "^".repeat(var.len())
                ),
                format!(
                    "\x1b[1m\x1b[38;5;12m    |\x1b[0m             \x1b[1m\x1b[33mhelp: if this is intentional, prefix it with an underscore: `_{var}`\x1b[0m"
                ),
                "\x1b[1m\x1b[38;5;12m    |\x1b[0m".to_owned(),
                String::new(),
            ];
            for l in lines {
                out.push_str(&l);
                out.push_str("\r\n");
                n += 1;
            }
        }
    }
    out.into_bytes()
}

const MODULES: &[&str] = &[
    "api::server",
    "api::router",
    "db::pool",
    "db::query",
    "cache::lru",
    "auth::session",
    "auth::token",
    "queue::worker",
    "queue::retry",
    "net::tcp",
    "net::tls",
    "sched::cron",
    "metrics::export",
    "storage::blob",
];

const USERS: &[&str] = &["alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi"];
const PATHS: &[&str] =
    &["/v1/orders", "/v1/users", "/v1/items", "/healthz", "/v2/search", "/metrics"];

fn ip(rng: &mut ChaCha8Rng) -> String {
    format!("10.{}.{}.{}", rng.gen_range(0..4), rng.gen_range(0..256), rng.gen_range(1..255))
}

/// One of 20 message templates filled with varying ids, durations and addresses.
fn message(rng: &mut ChaCha8Rng, which: usize) -> String {
    let id = rng.gen_range(1000..999_999);
    let ms = rng.gen_range(1..2500);
    let a = ip(rng);
    let user = pick(rng, USERS);
    let path = pick(rng, PATHS);
    let n = rng.gen_range(1..400);
    match which {
        0 => format!("accepted connection from {a}:{}", rng.gen_range(1024..65535)),
        1 => format!("request id={id} GET {path} status=200 took {ms}ms"),
        2 => format!("request id={id} POST {path} status=201 took {ms}ms"),
        3 => format!("cache hit key=user:{id} ttl={n}s"),
        4 => format!("cache miss key=item:{id}, loading from database"),
        5 => format!("session opened for user {user} from {a}"),
        6 => format!("session expired for user {user} after {n} minutes idle"),
        7 => format!("worker {} picked job {id} from queue default", n % 16),
        8 => format!("job {id} finished in {ms}ms, {n} records written"),
        9 => format!("retrying job {id}, attempt {} of 5, backoff {ms}ms", n % 5 + 1),
        10 => format!("connection pool at {n}/400, {} waiting", n % 7),
        11 => format!("slow query ({ms}ms): SELECT * FROM orders WHERE customer_id = {id}"),
        12 => format!("TLS handshake with {a} completed in {ms}ms"),
        13 => format!("token for user {user} refreshed, valid for {n} seconds"),
        14 => format!("flushed {n} metrics to exporter in {ms}ms"),
        15 => format!("upstream {a} returned status {} for {path}", 500 + n % 4),
        16 => format!("timeout talking to {a} after {ms}ms (request id={id})"),
        17 => format!("blob {id} stored, {} bytes, checksum {:08x}", n * 1024, rng.r#gen::<u32>()),
        18 => format!("config reloaded, {n} routes active"),
        _ => format!("rate limit exceeded for {a}, rejecting request id={id}"),
    }
}

/// A service log: ISO timestamp, a colored level, a module path and a message.
pub fn service_log(seed: u64, rows: usize) -> Vec<u8> {
    let mut rng = rng(seed);
    let mut out = String::new();
    let mut ms_total: u64 = rng.gen_range(0..80_000_000);
    for _ in 0..rows {
        ms_total += rng.gen_range(0..900);
        let (h, m, s, ms) = (
            ms_total / 3_600_000 % 24,
            ms_total / 60_000 % 60,
            ms_total / 1000 % 60,
            ms_total % 1000,
        );
        let which = rng.gen_range(0..20);
        let (level, color) = match which {
            15 | 16 | 19 => ("ERROR", "\x1b[31m"),
            6 | 9 | 11 => (" WARN", "\x1b[33m"),
            _ => (" INFO", "\x1b[32m"),
        };
        let module = pick(&mut rng, MODULES);
        let text = message(&mut rng, which);
        out.push_str(&format!(
            "\x1b[2m2026-11-08T{h:02}:{m:02}:{s:02}.{ms:03}Z\x1b[0m {color}{level}{RESET} \x1b[2m{module}\x1b[0m: {text}\r\n"
        ));
    }
    out.into_bytes()
}

const COMMANDS: &[&str] = &[
    "ls -l",
    "ls -la src",
    "ls -l /var/log",
    "ls -lh target",
    "ls -l tests",
    "ls -la ~/projects",
    "ls -l docs",
    "ls -lt | head -20",
    "ls -l /etc",
    "ls -la .config",
];

const FILE_NAMES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "README.md",
    "CHANGELOG.md",
    "LICENSE",
    "src",
    "tests",
    "benches",
    "examples",
    "target",
    "docs",
    ".git",
    ".gitignore",
    "build.rs",
    "main.rs",
    "lib.rs",
    "mod.rs",
    "config.toml",
    "notes.txt",
    "run.sh",
    "deploy.sh",
    "data.json",
    "server.log",
    "app.tar.gz",
];

/// A shell session: a colored prompt, a command, then 1 to 30 lines of `ls -l` style output.
pub fn shell_session(seed: u64, rows: usize) -> Vec<u8> {
    let mut rng = rng(seed);
    let mut out = String::new();
    let mut n = 0;
    while n < rows {
        let cmd = pick(&mut rng, COMMANDS);
        out.push_str(&format!(
            "\x1b[1;32mdev\x1b[0m:\x1b[1;34m~/work/{}\x1b[0m$ {cmd}\r\n",
            pick(&mut rng, &["api", "web", "tools", "docs", "core"])
        ));
        n += 1;
        for _ in 0..rng.gen_range(1..=30) {
            let name = pick(&mut rng, FILE_NAMES);
            let dir = matches!(
                name,
                "src" | "tests" | "benches" | "examples" | "target" | "docs" | ".git"
            );
            let exec = name.ends_with(".sh");
            let perms = if dir {
                "drwxr-xr-x"
            } else if exec {
                "-rwxr-xr-x"
            } else {
                "-rw-r--r--"
            };
            let size = if dir { 4096 } else { rng.gen_range(0..2_000_000) };
            let day = rng.gen_range(1..29);
            let (h, m) = (rng.gen_range(0..24), rng.gen_range(0..60));
            let color = if dir {
                "\x1b[1;34m"
            } else if exec {
                "\x1b[1;32m"
            } else if name.ends_with(".gz") {
                "\x1b[1;31m"
            } else {
                ""
            };
            out.push_str(&format!(
                "{perms} {:>2} dev dev {size:>8} Nov {day:>2} {h:02}:{m:02} {color}{name}{RESET}\r\n",
                rng.gen_range(1..9)
            ));
            n += 1;
        }
    }
    out.into_bytes()
}

/// A recorded stream, redacted of absolute paths (`/work`): `"cargo-build"` (output of
/// `cargo build --color=always`) or `"git-graph"` (`git log --graph --color=always`). Lines end
/// in `\r\n`.
pub fn recorded(name: &str) -> Vec<u8> {
    let raw: &[u8] = match name {
        "cargo-build" => include_bytes!("data/cargo-build.log"),
        "git-graph" => include_bytes!("data/git-graph.log"),
        other => panic!("no recorded stream named {other:?}"),
    };
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 40);
    for &b in raw {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}
