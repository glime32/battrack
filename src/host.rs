use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;

use crate::hw::{Hw, Sys};
use crate::procs::make_key;
use crate::tracker::browsers_from_env;
use crate::util::*;

const FAMILY: [&str; 8] = ["helium", "chrome", "chromium", "brave", "vivaldi-bin", "msedge", "opera", "thorium"];

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

fn gpu_flags(pid: i32) -> String {
    let mut b = vec![0u8; 8192];
    let Some(cl) = read_into(&format!("/proc/{pid}/cmdline"), &mut b) else { return String::new() };
    let cl = String::from_utf8_lossy(cl).replace('\0', " ");
    let keep = ["--enable-features=", "--disable-features=", "--use-", "--ignore-gpu", "--enable-gpu", "--disable-gpu"];
    let mut out = String::new();
    for a in cl.split(' ').filter(|a| keep.iter().any(|k| a.starts_with(k))) {
        if out.len() + a.len() + 1 > 599 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(a);
    }
    out
}

/// Historique du démon : pid → (m1, m10, m60), seulement si le fichier est frais.
fn daemon_history(path: &str) -> (bool, HashMap<i32, [String; 3]>) {
    let mut map = HashMap::new();
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|e| e.as_secs() < 3 * 60 + 30);
    if !fresh {
        return (false, map);
    }
    let Ok(text) = std::fs::read_to_string(path) else { return (false, map) };
    let mut daemon = false;
    for line in text.lines() {
        if line.starts_with('#') {
            daemon = true;
            continue;
        }
        let v: Vec<&str> = line.split('\t').collect();
        if v.len() >= 6 {
            if let Ok(pid) = v[0].parse() {
                let m = |s: &str| if s == "-" { "null".to_string() } else { s.to_string() };
                map.insert(pid, [m(v[3]), m(v[4]), m(v[5])]);
            }
        }
    }
    (daemon, map)
}

pub fn run() -> i32 {
    let parent = read_line(&format!("/proc/{}/comm", std::os::unix::process::parent_id())).unwrap_or_default();
    let browsers = browsers_from_env();
    let comm = if FAMILY.contains(&parent.as_str()) { parent } else { browsers[0].clone() };
    let names = vec![comm.clone()];
    let hw = Hw::discover();
    let paths = Paths::new();
    let hz = sysconf(libc::_SC_CLK_TCK).max(1) as f64;
    let page_kb = (sysconf(libc::_SC_PAGESIZE) / 1024).max(1) as u64;
    let mut cache: HashMap<(i32, u64), (i32, String)> = HashMap::new();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 1024];
    loop {
        let mut len = [0u8; 4];
        if stdin.read_exact(&mut len).is_err() {
            return 0;
        }
        let n = u32::from_ne_bytes(len) as usize;
        if n > 65536 {
            return 0;
        }
        let mut body = vec![0u8; n];
        if stdin.read_exact(&mut body).is_err() {
            return 0;
        }
        let (daemon, hist) = daemon_history(&paths.browser);
        let mut s = Sys::default();
        hw.read_bat(&mut s);
        let pkg = Hw::rapl(&hw.pkg).unwrap_or(-1.0);
        let psys = Hw::rapl(&hw.psys).unwrap_or(-1.0);
        let boot = boottime();
        let mut out = format!(
            "{{\"v\":2,\"hz\":{},\"t\":{:.0},\"daemon\":{},\"bat\":{},\"pw\":{:.2},\"pct\":{},\"pkg\":{pkg:.0},\"psys\":{psys:.0},\"rmax\":{:.0},\"procs\":[",
            hz as u64,
            mono() * 1000.0,
            daemon as u8,
            s.on_bat as u8,
            s.pw,
            s.pct,
            if hw.rmax_psys > 0.0 { hw.rmax_psys } else { hw.rmax_pkg }
        );
        let mut flags = String::new();
        let mut first = true;
        let mut alive = Vec::new();
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for e in rd.flatten() {
                let n = e.file_name();
                if !n.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) {
                    continue;
                }
                let pid = num(n.as_bytes()) as i32;
                let Some(st) = read_into(&format!("/proc/{pid}/stat"), &mut buf) else { continue };
                let (Some(lp), Some(rp)) = (st.iter().position(|&c| c == b'('), st.iter().rposition(|&c| c == b')')) else { continue };
                if rp + 2 >= st.len() || &st[lp + 1..rp] != comm.as_bytes() {
                    continue;
                }
                let f: Vec<u64> = st[rp + 2..].split(|&c| c == b' ').map(num).collect();
                if f.len() < 22 {
                    continue;
                }
                let (ticks, start, rss) = (f[11] + f[12], f[19], f[21]);
                alive.push((pid, start));
                let (cid, kind) = cache
                    .entry((pid, start))
                    .or_insert_with(|| {
                        let k = make_key(pid, &comm, false, &names);
                        (k.cid, if k.kind.is_empty() { "?".into() } else { k.kind })
                    })
                    .clone();
                if flags.is_empty() && kind == "gpu" {
                    flags = gpu_flags(pid);
                    if flags.is_empty() {
                        flags.push('-');
                    }
                }
                let none = ["null".to_string(), "null".to_string(), "null".to_string()];
                let m = hist.get(&pid).unwrap_or(&none);
                let _ = write!(
                    out,
                    "{}{{\"pid\":{pid},\"cid\":{cid},\"ty\":\"{}\",\"tk\":{ticks},\"age\":{:.1},\"rss\":{},\"m1\":{},\"m10\":{},\"m60\":{}}}",
                    if first { "" } else { "," },
                    json_str(&kind),
                    boot - start as f64 / hz,
                    rss * page_kb,
                    m[0],
                    m[1],
                    m[2]
                );
                first = false;
            }
        }
        cache.retain(|k, _| alive.contains(k));
        let flags = if flags == "-" { String::new() } else { flags };
        let _ = write!(out, "],\"flags\":\"{}\"}}", json_str(&flags));
        let bytes = out.as_bytes();
        if stdout.write_all(&(bytes.len() as u32).to_ne_bytes()).is_err() || stdout.write_all(bytes).is_err() || stdout.flush().is_err() {
            return 0;
        }
    }
}
