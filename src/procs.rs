use std::collections::HashMap;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;

use crate::util::*;

pub const RING: usize = 60;
const MAXBR: usize = 256;
const MAXAPP: usize = 1024;
const PF_KTHREAD: u64 = 0x0020_0000;

pub struct App {
    pub name: String,
    pub cpu: [u64; 2],
    pub wk: [u64; 2],
    pub gpu: [f64; 2],
    pub vid: [f64; 2],
    pub last_tk: u64,
    pub last_gpu: f64,
}

#[derive(Default)]
pub struct Apps {
    pub list: Vec<App>,
    index: HashMap<String, usize>,
}

impl Apps {
    pub fn get(&mut self, name: &str) -> usize {
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        if self.list.len() >= MAXAPP - 1 && name != "[divers]" {
            return self.get("[divers]");
        }
        let clean: String = name.chars().map(|c| if c.is_control() { '_' } else { c }).take(39).collect();
        let i = self.list.len();
        self.list.push(App { name: clean, cpu: [0; 2], wk: [0; 2], gpu: [0.0; 2], vid: [0.0; 2], last_tk: 0, last_gpu: 0.0 });
        self.index.insert(name.to_string(), i);
        i
    }

    pub fn clear_counters(&mut self) {
        for a in &mut self.list {
            a.cpu = [0; 2];
            a.wk = [0; 2];
            a.gpu = [0.0; 2];
            a.vid = [0.0; 2];
        }
    }
}

#[derive(Default)]
struct Proc {
    pid: i32,
    start: u64,
    ticks: u64,
    wake: u64,
    app: usize,
    browser: Option<usize>,
    cgroup: Option<usize>,
    comm: [u8; 16],
    comm_len: u8,
    kthread: bool,
    no_fd: bool,
    keyed: bool,
    drm: Vec<(i32, u32)>,
}

pub struct BrowserProc {
    pub pid: i32,
    pub cid: i32,
    pub gen: u32,
    pub n: u16,
    pub kind: String,
    pub ring: [u16; RING],
}

struct DrmClient {
    gen: u32,
    ready: bool,
    last: f64,
    c: [u64; 5],
    t: [u64; 5],
}

/// CPU d'un cgroup qu'aucun processus vivant n'explique : processus trop courts pour être vus.
struct Cgroup {
    stat: String,
    app: usize,
    prev: Option<u64>,
    tracked: u64,
    carry: f64,
    gen: u32,
}

pub struct Scanner {
    procs: Vec<Proc>,
    spare: Vec<Proc>,
    pub browsers: Vec<Option<BrowserProc>>,
    drm: HashMap<u32, DrmClient>,
    cgroups: Vec<Cgroup>,
    cg_index: HashMap<String, usize>,
    pub dts: [f64; RING],
    pub gs: u64,
    pub sc: u32,
    pub browser_names: Vec<String>,
    hz: u64,
}

pub struct KeyInfo {
    pub name: String,
    pub cid: i32,
    pub kind: String,
    pub cgroup: Option<String>,
}

pub fn is_interp(c: &str) -> bool {
    const L: [&str; 12] = ["python", "node", "java", "perl", "ruby", "bash", "sh", "dash", "lua", "php", "deno", "bun"];
    L.iter().any(|p| {
        c.strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(|ch: char| ch.is_ascii_digit() || ch == '.'))
    })
}

fn is_wrapper(c: &str) -> bool {
    matches!(
        c,
        "zsh" | "fish" | "timeout" | "nice" | "ionice" | "env" | "sudo" | "xargs" | "uv" | "make" | "script" | "npm"
            | "npx" | "cargo" | "time" | "stdbuf" | "nohup" | "flock" | "setsid" | "systemd-run"
    ) || is_interp(c)
}

fn comm_ppid(pid: i32) -> Option<(String, i32)> {
    let mut b = [0u8; 512];
    let s = read_into(&format!("/proc/{pid}/stat"), &mut b)?;
    let lp = s.iter().position(|&c| c == b'(')?;
    let rp = s.iter().rposition(|&c| c == b')')?;
    let comm = String::from_utf8_lossy(&s[lp + 1..rp]).into_owned();
    let ppid = s.get(rp + 2..)?.split(|&c| c == b' ').nth(1).map(num)? as i32;
    Some((comm, ppid))
}

/// Premier ancêtre qui n'est ni un shell ni un interpréteur : claude, konsole, systemd…
fn launcher(pid: i32) -> Option<String> {
    let (_, mut cur) = comm_ppid(pid)?;
    for _ in 0..8 {
        if cur <= 1 {
            return None;
        }
        let (comm, ppid) = comm_ppid(cur)?;
        if !is_wrapper(&comm) {
            return Some(comm);
        }
        cur = ppid;
    }
    None
}

fn interp_key(pid: i32, comm: &str, args: &[String]) -> String {
    let mut target: Option<&str> = None;
    let mut inline = "stdin";
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-" {
            break;
        }
        if a.starts_with('-') {
            match a {
                "-m" | "-jar" => {
                    target = args.get(i + 1).map(|s| s.as_str());
                    break;
                }
                "-c" | "-e" | "-p" | "--eval" | "--print" => {
                    inline = "-c";
                    break;
                }
                "-X" | "-W" | "-r" | "--require" | "-cp" | "-classpath" | "--import" | "--loader" => i += 2,
                _ => i += 1,
            }
            continue;
        }
        target = Some(a);
        break;
    }
    if let Some(t) = target {
        return format!("{comm}:{}", t.rsplit('/').next().unwrap_or(t));
    }
    if matches!(comm, "bash" | "sh" | "dash") {
        return comm.to_string();
    }
    match launcher(pid) {
        Some(l) => format!("{comm}:{inline} [{l}]"),
        None => format!("{comm}:{inline}"),
    }
}

fn kthread_name(comm: &str) -> String {
    if let Some(rest) = comm.strip_prefix("irq/") {
        let n = rest.split_once('-').map(|(_, b)| b).unwrap_or(rest);
        return format!("[k]irq:{n}");
    }
    let base = comm.split('/').next().unwrap_or(comm);
    let t = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':' || c == '-');
    format!("[k]{}", if t.is_empty() { base } else { t })
}

/// "…/app-org.kde.konsole@3f2a….service" → "konsole", "…/app-claude-code-cyan-1021.scope/tab(9).scope" → "claude-code"
pub fn cgroup_label(rel: &str) -> String {
    if rel.contains("lxc.payload") {
        return if rel.contains("waydroid") { "waydroid".into() } else { "lxc".into() };
    }
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    let part = parts.iter().rev().find(|p| p.starts_with("app-")).or(parts.last()).copied().unwrap_or("?");
    let mut s = part;
    for suf in [".service", ".scope", ".slice"] {
        if let Some(x) = s.strip_suffix(suf) {
            s = x;
            break;
        }
    }
    s = s.strip_prefix("app-").unwrap_or(s);
    if let Some(at) = s.find('@') {
        s = &s[..at];
    }
    if let Some((head, tail)) = s.rsplit_once('-') {
        if !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            s = head;
        }
    }
    s = s.strip_prefix("org.kde.").unwrap_or(s);
    if s.starts_with("claude-code") {
        return "claude-code".into();
    }
    s.to_string()
}

pub fn make_key(pid: i32, comm: &str, kthread: bool, browsers: &[String]) -> KeyInfo {
    let mut k = KeyInfo { name: String::new(), cid: 0, kind: String::new(), cgroup: None };
    if kthread {
        k.name = kthread_name(comm);
        return k;
    }
    let mut buf = vec![0u8; 4096];
    if let Some(cg) = read_into(&format!("/proc/{pid}/cgroup"), &mut buf) {
        let cg = String::from_utf8_lossy(cg).into_owned();
        if let Some(rel) = cg.lines().find_map(|l| l.strip_prefix("0::")) {
            let rel = rel.trim();
            if !rel.is_empty() && rel != "/" {
                k.cgroup = Some(rel.to_string());
            }
        }
        if cg.contains("lxc") {
            let pre = if cg.contains("waydroid") { "[android]" } else { "[lxc]" };
            k.name = format!("{pre}{comm}");
            return k;
        }
    }
    let n = read_into(&format!("/proc/{pid}/cmdline"), &mut buf).map(|b| b.len()).unwrap_or(0);
    let mut args: Vec<String> = buf[..n]
        .split(|&c| c == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    // Chromium et Electron réécrivent leur titre : un seul argument séparé par des espaces.
    if args.len() == 1 && args[0].contains(' ') {
        args = args[0].split(' ').filter(|s| !s.is_empty()).map(String::from).collect();
    }
    let (mut ty, mut sub, mut ext) = (None, None, false);
    for a in args.iter().skip(1) {
        if let Some(v) = a.strip_prefix("--type=") {
            ty = Some(v);
        } else if let Some(v) = a.strip_prefix("--utility-sub-type=") {
            sub = Some(v);
        } else if a == "--extension-process" {
            ext = true;
        } else if let Some(v) = a.strip_prefix("--renderer-client-id=") {
            k.cid = v.parse().unwrap_or(0);
        }
    }
    if let Some(t) = ty {
        let kind: String = match (t, sub) {
            ("renderer", _) => if ext { "ext" } else { "renderer" }.into(),
            ("gpu-process", _) => "gpu".into(),
            ("utility", Some(s)) => s.split('.').next().unwrap_or(s).chars().take(23).collect(),
            (other, _) => other.chars().take(23).collect(),
        };
        k.name = format!("{comm}:{kind}");
        k.kind = kind.chars().take(11).collect();
        return k;
    }
    if browsers.iter().any(|b| b == comm) {
        k.name = comm.to_string();
        k.kind = "main".into();
        return k;
    }
    k.name = if is_interp(comm) && !args.is_empty() { interp_key(pid, comm, &args) } else { comm.to_string() };
    k
}

fn wake_sum(pid: i32, kthread: bool) -> u64 {
    let mut b = [0u8; 128];
    if kthread {
        return read_into(&format!("/proc/{pid}/schedstat"), &mut b).map(|x| nth_field(x, 2)).unwrap_or(0);
    }
    let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return 0 };
    let mut path = String::with_capacity(48);
    let mut s = 0;
    for e in rd.flatten() {
        path.clear();
        let _ = write!(path, "/proc/{pid}/task/{}/schedstat", e.file_name().to_string_lossy());
        if let Some(x) = read_into(&path, &mut b) {
            s += nth_field(x, 2);
        }
    }
    s
}

struct FdInfo {
    cid: u32,
    xe: bool,
    c: [u64; 5],
    t: [u64; 5],
}

fn engine(e: &[u8]) -> Option<usize> {
    match e {
        b"rcs" | b"render" | b"gfx" => Some(0),
        b"ccs" | b"compute" => Some(1),
        b"bcs" | b"copy" | b"dma" => Some(2),
        b"vcs" | b"video" | b"dec" | b"enc" | b"jpeg" => Some(3),
        b"vecs" | b"video-enhance" => Some(4),
        _ => None,
    }
}

fn parse_fdinfo(pid: i32, fd: i32) -> Option<FdInfo> {
    let mut b = [0u8; 4096];
    let data = read_into(&format!("/proc/{pid}/fdinfo/{fd}"), &mut b)?;
    let mut f = FdInfo { cid: 0, xe: false, c: [0; 5], t: [0; 5] };
    let mut ok = false;
    for line in data.split(|&c| c == b'\n') {
        if let Some(v) = line.strip_prefix(b"drm-client-id:") {
            f.cid = num(skip_ws(v)) as u32;
            ok = true;
            continue;
        }
        let (total, cycles, rest) = if let Some(r) = line.strip_prefix(b"drm-total-cycles-") {
            (true, true, r)
        } else if let Some(r) = line.strip_prefix(b"drm-cycles-") {
            (false, true, r)
        } else if let Some(r) = line.strip_prefix(b"drm-engine-") {
            (false, false, r)
        } else {
            continue;
        };
        let Some(colon) = rest.iter().position(|&c| c == b':') else { continue };
        let Some(e) = engine(&rest[..colon]) else { continue };
        let v = num(skip_ws(&rest[colon + 1..]));
        if total {
            f.t[e] += v;
        } else {
            f.c[e] += v;
        }
        f.xe |= cycles;
    }
    ok.then_some(f)
}

fn fd_scan(p: &mut Proc) {
    let rd = match std::fs::read_dir(format!("/proc/{}/fd", p.pid)) {
        Ok(r) => r,
        Err(e) => {
            p.no_fd = e.kind() == std::io::ErrorKind::PermissionDenied;
            return;
        }
    };
    p.drm.clear();
    for e in rd.flatten() {
        let Some(fd) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let Ok(target) = std::fs::read_link(e.path()) else { continue };
        if !target.as_os_str().as_bytes().starts_with(b"/dev/dri/") {
            continue;
        }
        let Some(f) = parse_fdinfo(p.pid, fd) else { continue };
        if p.drm.len() < 4 && !p.drm.iter().any(|&(_, c)| c == f.cid) {
            p.drm.push((fd, f.cid));
        }
    }
}

impl Scanner {
    pub fn new(browser_names: Vec<String>, hz: u64) -> Self {
        Scanner {
            procs: Vec::new(),
            spare: Vec::new(),
            browsers: (0..MAXBR).map(|_| None).collect(),
            drm: HashMap::new(),
            cgroups: Vec::new(),
            cg_index: HashMap::new(),
            dts: [0.0; RING],
            gs: 0,
            sc: 0,
            browser_names,
            hz,
        }
    }

    pub fn nprocs(&self) -> usize {
        self.procs.len()
    }

    pub fn ndrm(&self) -> usize {
        self.drm.len()
    }

    fn cgroup_get(&mut self, apps: &mut Apps, rel: String) -> usize {
        if let Some(&i) = self.cg_index.get(&rel) {
            return i;
        }
        let app = apps.get(&format!("[courts] {}", cgroup_label(&rel)));
        let i = self.cgroups.len();
        self.cgroups.push(Cgroup { stat: format!("/sys/fs/cgroup{rel}/cpu.stat"), app, prev: None, tracked: 0, carry: 0.0, gen: self.sc });
        self.cg_index.insert(rel, i);
        i
    }

    fn browser_alloc(&mut self, pid: i32, cid: i32, kind: &str) -> Option<usize> {
        let i = self.browsers.iter().position(|b| b.is_none())?;
        let kind = if kind.is_empty() { "?".to_string() } else { kind.to_string() };
        self.browsers[i] = Some(BrowserProc { pid, cid, gen: self.sc, n: 0, kind, ring: [0; RING] });
        Some(i)
    }

    fn drm_proc(&mut self, p: &mut Proc, apps: &mut Apps, now: f64, ci: Option<usize>) {
        let sc = self.sc;
        let mut i = 0;
        while i < p.drm.len() {
            let (fd, cid) = p.drm[i];
            let f = match parse_fdinfo(p.pid, fd) {
                Some(f) if f.cid == cid => f,
                _ => {
                    p.drm.swap_remove(i);
                    continue;
                }
            };
            let d = self.drm.entry(f.cid).or_insert(DrmClient { gen: sc, ready: false, last: now, c: [0; 5], t: [0; 5] });
            if d.ready {
                let el = now - d.last;
                let (mut g3, mut gv) = (0.0, 0.0);
                for e in 0..5 {
                    let dc = f.c[e].saturating_sub(d.c[e]);
                    let busy = if f.xe {
                        let dt = f.t[e].saturating_sub(d.t[e]);
                        if dt > 0 { (dc as f64 / dt as f64 * el).min(el) } else { 0.0 }
                    } else {
                        dc as f64 / 1e9
                    };
                    if e < 3 {
                        g3 += busy;
                    } else {
                        gv += busy;
                    }
                }
                let a = &mut apps.list[p.app];
                a.last_gpu += g3 + gv;
                if let Some(c) = ci {
                    a.gpu[c] += g3;
                    a.vid[c] += gv;
                }
            }
            d.c = f.c;
            d.t = f.t;
            d.last = now;
            d.gen = sc;
            d.ready = true;
            i += 1;
        }
    }

    /// Un passage sur /proc. `ci` : contexte de l'intervalle (0 batterie, 1 secteur, None = ne pas compter).
    /// Renvoie les ticks CPU attribués (processus vus + processus courts retrouvés via les cgroups).
    #[allow(clippy::too_many_arguments)]
    pub fn scan(&mut self, apps: &mut Apps, now: f64, prev_boot: f64, first: bool, ci: Option<usize>, fdall: bool, gi: usize) -> u64 {
        let Ok(dir) = std::fs::read_dir("/proc") else { return 0 };
        self.sc = self.sc.wrapping_add(1);
        for a in &mut apps.list {
            a.last_tk = 0;
            a.last_gpu = 0.0;
        }
        for c in &mut self.cgroups {
            c.tracked = 0;
        }
        let mut next = std::mem::take(&mut self.spare);
        next.clear();
        let (mut sorted, mut lastpid, mut attr) = (true, 0, 0u64);
        let hz = self.hz as f64;
        let mut buf = [0u8; 1024];
        let mut path = String::with_capacity(32);
        for e in dir.flatten() {
            let name = e.file_name();
            let name = name.as_bytes();
            if name.is_empty() || !(b'1'..=b'9').contains(&name[0]) {
                continue;
            }
            let pid = num(name) as i32;
            path.clear();
            let _ = write!(path, "/proc/{pid}/stat");
            let Some(b) = read_into(&path, &mut buf) else { continue };
            let (Some(lp), Some(rp)) = (b.iter().position(|&c| c == b'('), b.iter().rposition(|&c| c == b')')) else { continue };
            if rp < lp || rp + 2 >= b.len() {
                continue;
            }
            let comm_b = &b[lp + 1..rp];
            let comm_b = &comm_b[..comm_b.len().min(15)];
            let mut f = [0u64; 25];
            for (k, t) in b[rp + 2..].split(|&c| c == b' ').enumerate() {
                let field = k + 3;
                if field > 24 {
                    break;
                }
                f[field] = num(t);
            }
            let (start, tk) = (f[22], f[14] + f[15]);

            let old = self.procs.binary_search_by_key(&pid, |p| p.pid).ok().filter(|&i| self.procs[i].start == start);
            let mut p = match old {
                Some(i) => {
                    let o = &mut self.procs[i];
                    Proc { drm: std::mem::take(&mut o.drm), ..*o }
                }
                None => Proc { pid, start, kthread: f[9] & PF_KTHREAD != 0, ..Default::default() },
            };
            let has_old = old.is_some();
            if pid < lastpid {
                sorted = false;
            }
            lastpid = pid;
            let born = !has_old && !first && start as f64 / hz > prev_boot - 0.5;

            if !p.keyed || &p.comm[..p.comm_len as usize] != comm_b {
                p.comm = [0; 16];
                p.comm[..comm_b.len()].copy_from_slice(comm_b);
                p.comm_len = comm_b.len() as u8;
                p.keyed = true;
                let comm = String::from_utf8_lossy(comm_b).into_owned();
                let k = make_key(pid, &comm, p.kthread, &self.browser_names);
                p.app = apps.get(&k.name);
                p.cgroup = k.cgroup.map(|rel| self.cgroup_get(apps, rel));
                if let Some(bi) = p.browser.take() {
                    self.browsers[bi] = None;
                }
                if self.browser_names.contains(&comm) {
                    p.browser = self.browser_alloc(pid, k.cid, &k.kind);
                }
            }

            let dd = if has_old { tk.saturating_sub(p.ticks) } else if born { tk } else { 0 };
            p.ticks = tk;
            if let Some(g) = p.cgroup {
                let c = &mut self.cgroups[g];
                c.gen = self.sc;
                c.tracked += dd;
            }
            if dd > 0 {
                let a = &mut apps.list[p.app];
                a.last_tk += dd;
                if let Some(c) = ci {
                    a.cpu[c] += dd;
                }
                attr += dd;
            }
            if dd > 0 || born {
                let w = wake_sum(pid, p.kthread);
                if w > 0 {
                    let dw = if p.wake > 0 && w >= p.wake { w - p.wake } else if born { w } else { 0 };
                    p.wake = w;
                    if let Some(c) = ci {
                        apps.list[p.app].wk[c] += dw;
                    }
                }
            }
            if !p.kthread && !p.no_fd && (!has_old || (fdall && dd > 0)) {
                fd_scan(&mut p);
            }
            if !p.drm.is_empty() && (dd > 0 || !has_old) {
                self.drm_proc(&mut p, apps, now, ci);
            }
            if let Some(bi) = p.browser {
                if let Some(x) = self.browsers[bi].as_mut() {
                    x.gen = self.sc;
                    if has_old || born {
                        x.ring[gi] = dd.min(65535) as u16;
                        if (x.n as usize) < RING {
                            x.n += 1;
                        }
                    }
                }
            }
            next.push(p);
        }
        if !sorted {
            next.sort_by_key(|p| p.pid);
        }
        let mut sb = [0u8; 256];
        for c in &mut self.cgroups {
            if c.gen != self.sc {
                continue;
            }
            let Some(usage) = read_into(&c.stat, &mut sb).and_then(|b| b.strip_prefix(b"usage_usec ").map(num)) else { continue };
            if let (Some(prev), Some(ctx)) = (c.prev, ci) {
                let v = usage.saturating_sub(prev) as f64 * hz / 1e6 - c.tracked as f64 + c.carry;
                if v >= 1.0 {
                    let whole = v.floor();
                    let a = &mut apps.list[c.app];
                    a.cpu[ctx] += whole as u64;
                    a.last_tk += whole as u64;
                    attr += whole as u64;
                    c.carry = v - whole;
                } else {
                    // l'arrondi au tick par processus fait du bruit : on le reporte au tour suivant
                    c.carry = v.max(-5.0 * hz);
                }
            } else {
                c.carry = 0.0;
            }
            c.prev = Some(usage);
        }
        for b in &mut self.browsers {
            if b.as_ref().is_some_and(|x| x.gen != self.sc) {
                *b = None;
            }
        }
        let sc = self.sc;
        self.drm.retain(|_, d| sc.wrapping_sub(d.gen) <= 30);
        self.spare = std::mem::replace(&mut self.procs, next);
        if sc.is_multiple_of(60) && self.cgroups.len() > 256 {
            self.gc_cgroups();
        }
        attr
    }

    fn gc_cgroups(&mut self) {
        let sc = self.sc;
        let mut map = vec![None; self.cgroups.len()];
        let mut kept = Vec::new();
        for (i, c) in std::mem::take(&mut self.cgroups).into_iter().enumerate() {
            if sc.wrapping_sub(c.gen) <= 60 {
                map[i] = Some(kept.len());
                kept.push(c);
            }
        }
        self.cg_index.retain(|_, v| match map[*v] {
            Some(n) => {
                *v = n;
                true
            }
            None => false,
        });
        for p in &mut self.procs {
            p.cgroup = p.cgroup.and_then(|g| map[g]);
        }
        self.cgroups = kept;
    }

    pub fn browser_table(&self, gi: usize, interval: u64) -> Option<String> {
        if self.browsers.iter().all(|b| b.is_none()) {
            return None;
        }
        let mut out = format!("#battrack-browser\t1\t{}\t{}\n", unix_now(), interval);
        let hz = self.hz as f64;
        for x in self.browsers.iter().flatten() {
            let _ = write!(out, "{}\t{}\t{}", x.pid, x.cid, x.kind);
            for w in [1usize, 10, 60] {
                let k = w.min(x.n as usize);
                let (mut sr, mut st) = (0.0, 0.0);
                for j in 0..k {
                    let id = (gi + RING - j) % RING;
                    sr += x.ring[id] as f64;
                    st += self.dts[id];
                }
                if k > 0 && st > 0.0 {
                    let _ = write!(out, "\t{:.2}", sr * 100.0 / hz / st);
                } else {
                    out.push_str("\t-");
                }
            }
            let _ = writeln!(out, "\t{}", x.n);
        }
        Some(out)
    }
}
