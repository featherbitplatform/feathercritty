// Modified by the featherbit authors for feathercritty; see README.md for the list of changes.

//! Checks that the differential test's oracle (`tests/oracle/`) is the published
//! `alacritty_terminal` 0.26.0 source plus exactly the hunks of `tests/oracle/PATCHES.diff`.
//!
//! Every vendored file, with `PATCHES.diff` applied in reverse, must hash (SHA-256, line endings
//! normalized to LF) to the value `tests/oracle/MANIFEST` records for the published file, and the
//! vendored file list must equal the manifest's. Offline: nothing is fetched. To change the
//! patches or move the oracle to a new upstream release, see `tests/oracle/PATCHES.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Files in `tests/oracle/` that are ours, not the vendored crate's.
const OWN_FILES: [&str; 4] = ["MANIFEST", "PATCHES.diff", "PATCHES.md", "refresh.py"];

fn oracle_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/oracle")
}

fn read_lf(path: &Path) -> String {
    fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

/// Vendored files, as `/`-separated paths relative to `tests/oracle/`.
fn vendored_files(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        if entry.file_type().unwrap().is_dir() {
            vendored_files(&entry.path(), &path, out);
        } else if !(prefix.is_empty() && OWN_FILES.contains(&name.as_str())) {
            out.push(path);
        }
    }
}

/// One hunk of a unified diff: where its new side starts (0-based) and both sides' lines.
struct Hunk {
    new_start: usize,
    old: Vec<String>,
    new: Vec<String>,
}

/// `PATCHES.diff` as hunks per file.
fn parse_patches(diff: &str) -> BTreeMap<String, Vec<Hunk>> {
    let mut files: BTreeMap<String, Vec<Hunk>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in diff.split_inclusive('\n') {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current = Some(path.trim_end().to_owned());
            files.entry(path.trim_end().to_owned()).or_default();
        } else if line.starts_with("--- a/") {
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let new = header.split(' ').nth(1).unwrap().trim_start_matches('+');
            let mut parts = new.split(',');
            let start: usize = parts.next().unwrap().parse().unwrap();
            let count: usize = parts.next().map_or(1, |count| count.parse().unwrap());
            let new_start = if count == 0 { start } else { start - 1 };
            let path = current.clone().expect("hunk before file header");
            files.get_mut(&path).unwrap().push(Hunk { new_start, old: vec![], new: vec![] });
        } else {
            let path = current.as_ref().expect("diff line before file header");
            let hunk = files.get_mut(path).unwrap().last_mut().expect("diff line before hunk");
            match line.split_at(1) {
                (" ", text) => {
                    hunk.old.push(text.to_owned());
                    hunk.new.push(text.to_owned());
                },
                ("-", text) => hunk.old.push(text.to_owned()),
                ("+", text) => hunk.new.push(text.to_owned()),
                _ => panic!("unexpected diff line {line:?}"),
            }
        }
    }
    files
}

/// `patched` with `hunks` applied in reverse; every hunk's new side must match exactly.
fn unpatch(path: &str, patched: &str, hunks: &[Hunk]) -> String {
    let lines: Vec<&str> = patched.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut next = 0;
    for hunk in hunks {
        assert!(hunk.new_start >= next, "{path}: overlapping hunks");
        let end = hunk.new_start + hunk.new.len();
        assert!(
            end <= lines.len() && lines[hunk.new_start..end] == hunk.new[..],
            "{path}: hunk at line {} does not match the vendored file",
            hunk.new_start + 1,
        );
        out.extend(lines[next..hunk.new_start].iter().copied());
        out.extend(hunk.old.iter().map(String::as_str));
        next = end;
    }
    out.extend(lines[next..].iter().copied());
    out
}

#[test]
fn oracle_is_pristine_except_patches() {
    let dir = oracle_dir();
    let manifest: BTreeMap<String, String> = read_lf(&dir.join("MANIFEST"))
        .lines()
        .map(|line| {
            let (hash, path) = line.split_once("  ").unwrap();
            (path.to_owned(), hash.to_owned())
        })
        .collect();

    let mut files = Vec::new();
    vendored_files(&dir, "", &mut files);
    files.sort();
    assert_eq!(files, manifest.keys().cloned().collect::<Vec<_>>(), "vendored file list");

    let patches = parse_patches(&read_lf(&dir.join("PATCHES.diff")));
    assert!(!patches.is_empty(), "PATCHES.diff is empty");
    for path in patches.keys() {
        assert!(manifest.contains_key(path), "PATCHES.diff names an unknown file {path}");
    }

    for (path, hash) in &manifest {
        let content = read_lf(&dir.join(path));
        let original = match patches.get(path) {
            Some(hunks) => unpatch(path, &content, hunks),
            None => content,
        };
        assert_eq!(&hex(&sha256(original.as_bytes())), hash, "{path} differs from 0.26.0");
    }
}

#[test]
fn sha256_known_values() {
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    assert_eq!(
        hex(&sha256(long)),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SHA-256 (FIPS 180-4), so the check needs no extra dependency.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());

    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(value);
        }
    }

    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}
