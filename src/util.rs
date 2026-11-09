use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn read_into<'a>(path: &str, buf: &'a mut [u8]) -> Option<&'a [u8]> {
    let mut f = File::open(path).ok()?;
    let n = f.read(buf).ok()?;
    Some(&buf[..n])
}

pub fn read_all(path: &str, out: &mut Vec<u8>) -> bool {
    out.clear();
    let Ok(mut f) = File::open(path) else { return false };
    let mut chunk = [0u8; 16384];
    loop {
        match f.read(&mut chunk) {
            Ok(0) => return true,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(_) => return false,
        }
    }
}

pub fn read_line(path: &str) -> Option<String> {
    let mut b = [0u8; 1024];
    let s = read_into(path, &mut b)?;
    if s.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(s).trim_end_matches(['\n', ' ']).to_string())
}

pub fn read_num(path: &str) -> Option<f64> {
    let mut b = [0u8; 64];
    let s = read_into(path, &mut b)?;
    let s = std::str::from_utf8(s).ok()?.trim();
    let end = s
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(s.len());
    s[..end].parse().ok()
}

pub fn num(t: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for &c in t {
        if !c.is_ascii_digit() {
            break;
        }
        v = v.wrapping_mul(10).wrapping_add((c - b'0') as u64);
    }
    v
}

pub fn skip_ws(t: &[u8]) -> &[u8] {
    let i = t.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(t.len());
    &t[i..]
}

pub fn nth_field(b: &[u8], n: usize) -> u64 {
    b.split(|c| c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .nth(n)
        .map(num)
        .unwrap_or(0)
}

pub fn clock(id: libc::clockid_t) -> f64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(id, &mut t) };
    t.tv_sec as f64 + t.tv_nsec as f64 / 1e9
}

pub fn mono() -> f64 {
    clock(libc::CLOCK_MONOTONIC)
}

pub fn boottime() -> f64 {
    clock(libc::CLOCK_BOOTTIME)
}

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub struct Tm {
    pub mday: i32,
    pub mon: i32,
    pub hour: i32,
    pub min: i32,
}

pub fn local(t: i64) -> Tm {
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    Tm { mday: tm.tm_mday, mon: tm.tm_mon + 1, hour: tm.tm_hour, min: tm.tm_min }
}

pub fn dur(s: f64) -> String {
    let m = (s / 60.0 + 0.5) as i64;
    if m >= 1440 {
        format!("{}j{:02}h", m / 1440, (m % 1440) / 60)
    } else {
        format!("{}h{:02}", m / 60, m % 60)
    }
}

pub fn sysconf(name: libc::c_int) -> i64 {
    unsafe { libc::sysconf(name) as i64 }
}

pub fn exists(p: &str) -> bool {
    std::path::Path::new(p).exists()
}

pub fn dir_names(p: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub struct Paths {
    pub state_dir: String,
    pub run_dir: String,
    pub state: String,
    pub pid: String,
    pub browser: String,
    pub now: String,
}

impl Paths {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let state_dir = match std::env::var("XDG_STATE_HOME") {
            Ok(s) if !s.is_empty() => format!("{s}/battrack"),
            _ => format!("{home}/.local/state/battrack"),
        };
        let run_dir = match std::env::var("XDG_RUNTIME_DIR") {
            Ok(s) if !s.is_empty() => format!("{s}/battrack"),
            _ => format!("/tmp/battrack-{}", unsafe { libc::getuid() }),
        };
        Paths {
            state: format!("{state_dir}/state.tsv"),
            pid: format!("{run_dir}/daemon.pid"),
            browser: format!("{run_dir}/browser.tsv"),
            now: format!("{run_dir}/now.txt"),
            state_dir,
            run_dir,
        }
    }
}

/// Écrit dans un fichier temporaire puis renomme : un lecteur ne voit jamais un fichier à moitié écrit.
pub fn write_atomic(path: &str, data: &str) -> bool {
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, data).is_err() {
        return false;
    }
    std::fs::rename(&tmp, path).is_ok()
}

pub fn width(s: &str) -> usize {
    s.chars().count()
}

pub fn pad(s: &str, w: usize) -> String {
    let n = width(s);
    if n >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - n))
    }
}
