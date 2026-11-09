use std::fmt::Write as _;
use std::process::{Command, Stdio};

use crate::hw::{mode_short, radios, Hw};
use crate::state::*;
use crate::tracker::Tracker;
use crate::util::*;

fn run(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let Ok(out) = out else { return String::new() };
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut s = String::with_capacity(raw.len());
    let mut esc = false;
    for ch in raw.chars() {
        if esc {
            esc = ch != 'm';
        } else if ch == '\x1b' {
            esc = true;
        } else {
            s.push(ch);
        }
    }
    s
}

fn pci_tag(cls: u64) -> &'static str {
    match cls >> 8 {
        0x0108 => "nvme",
        0x0500 => "ram",
        0x0700 => "uart",
        0x0805 => "sd",
        0x0280 => "wifi",
        0x0300 | 0x0380 => "gpu",
        0x0401 | 0x0403 => "audio",
        0x0480 => "ipu",
        0x0600 => "host",
        0x0601 => "isa",
        0x0604 => "pcie",
        0x0780 => "mei",
        0x0880 => "sys",
        0x0c03 => "usb",
        0x0c05 => "smbus",
        0x0c80 => "serial",
        0x1180 => "therm",
        0x1200 => "npu",
        _ => "?",
    }
}

#[derive(Default)]
struct Conf {
    ppd: String,
    plat: String,
    drv: String,
    gov: String,
    epp: String,
    res: String,
    hz: i32,
    vrr: String,
    bright: i32,
    kbd: i32,
    kbdmax: i32,
    wifi: String,
    bt: Option<bool>,
    aspm: String,
    pci: Vec<String>,
    procs: Vec<&'static str>,
    cmd: String,
    xe: String,
    kern: String,
    turbo: bool,
    maxmhz: i32,
    capmhz: i32,
    usbas: f64,
    dirty: f64,
}

const DAEMONS: [&str; 36] = [
    "baloo_file", "baloo_file_extr", "tracker-miner-f", "kdeconnectd", "ananicy-cpp", "bpftune", "intel_lpmd", "tlp",
    "auto-cpufreq", "thermald", "tuned", "system76-power", "syncthing", "dropbox", "nextcloud", "packagekitd", "dockerd",
    "containerd", "waydroid", "lxc-start", "ollama", "steam", "Discord", "discord", "spotify", "easyeffects", "zoom",
    "teams", "slack", "akonadi_control", "kwalletd6", "geoclue", "fwupd", "snapd", "cupsd", "1password",
];

fn read_conf(hw: &Hw) -> Conf {
    let mut c = Conf { bright: -1, kbd: -1, ..Default::default() };
    c.ppd = run("powerprofilesctl", &["get"]).trim().to_string();
    c.plat = read_line("/sys/firmware/acpi/platform_profile").unwrap_or_default();
    c.drv = read_line("/sys/devices/system/cpu/cpufreq/policy0/scaling_driver").unwrap_or_default();
    if let Some(st) = read_line("/sys/devices/system/cpu/intel_pstate/status") {
        c.drv = format!("{}/{st}", c.drv);
    }
    c.gov = read_line("/sys/devices/system/cpu/cpufreq/policy0/scaling_governor").unwrap_or_default();
    let mut epps: Vec<String> = Vec::new();
    for i in 0..64 {
        match read_line(&format!("/sys/devices/system/cpu/cpufreq/policy{i}/energy_performance_preference")) {
            Some(e) if !epps.contains(&e) => epps.push(e),
            Some(_) => {}
            None if i > 0 => break,
            None => {}
        }
    }
    c.epp = epps.join("/");
    c.turbo = read_num("/sys/devices/system/cpu/intel_pstate/no_turbo").unwrap_or(0.0) == 0.0;
    c.maxmhz = (read_num("/sys/devices/system/cpu/cpufreq/policy0/cpuinfo_max_freq").unwrap_or(0.0) / 1000.0) as i32;
    c.capmhz = (read_num("/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq").unwrap_or(0.0) / 1000.0) as i32;
    // kscreen-doctor répond parfois vide : trois essais
    for _ in 0..3 {
        let b = run("kscreen-doctor", &["-o"]);
        if let Some(m) = b.find("Modes:") {
            if let Some(star) = b[m..].find('*').map(|i| i + m) {
                let start = b[..star].rfind([' ', ':']).map(|i| i + 1).unwrap_or(m);
                let mode = b[start..star].trim_end_matches('!');
                if let Some((res, hz)) = mode.split_once('@') {
                    c.res = res.to_string();
                    c.hz = hz.trim_end_matches('!').parse::<f64>().map(|v| v.round() as i32).unwrap_or(0);
                }
            }
        }
        if let Some(v) = b.find("Vrr: ") {
            c.vrr = b[v + 5..].split_whitespace().next().unwrap_or("").to_string();
        }
        if c.hz > 0 {
            break;
        }
    }
    if !hw.backlight.is_empty() {
        let v = read_num(&format!("{}/brightness", hw.backlight));
        let m = read_num(&format!("{}/max_brightness", hw.backlight));
        if let (Some(v), Some(m)) = (v, m) {
            if m > 0.0 {
                c.bright = (v / m * 100.0).round() as i32;
            }
        }
    }
    if !hw.kbd.is_empty() {
        c.kbd = read_num(&format!("{}/brightness", hw.kbd)).map(|v| v as i32).unwrap_or(-1);
        c.kbdmax = hw.kbd_max as i32;
    }
    if let Some(n) = dir_names("/sys/class/net").into_iter().find(|n| exists(&format!("/sys/class/net/{n}/wireless"))) {
        let t = run("iw", &["dev", &n, "get", "power_save"]);
        c.wifi = t.split_once(": ").map(|(_, v)| v.trim().to_string()).unwrap_or_else(|| "?".into());
    }
    c.bt = radios().1;
    if let Some(p) = read_line("/sys/module/pcie_aspm/parameters/policy") {
        if let (Some(l), Some(r)) = (p.find('['), p.find(']')) {
            c.aspm = p[l + 1..r].to_string();
        }
    }
    for d in dir_names("/sys/bus/pci/devices") {
        if read_line(&format!("/sys/bus/pci/devices/{d}/power/control")).as_deref() != Some("on") {
            continue;
        }
        let cls = read_line(&format!("/sys/bus/pci/devices/{d}/class")).unwrap_or_default();
        let cls = u64::from_str_radix(cls.trim_start_matches("0x"), 16).unwrap_or(0);
        let nm = d.strip_prefix("0000:00:").or_else(|| d.strip_prefix("0000:")).unwrap_or(&d);
        c.pci.push(format!("{nm}({})", pci_tag(cls)));
    }
    c.usbas = read_num("/sys/module/usbcore/parameters/autosuspend").unwrap_or(-1.0);
    c.dirty = read_num("/proc/sys/vm/dirty_writeback_centisecs").unwrap_or(-100.0) / 100.0;
    let mut xe = Vec::new();
    for (param, short) in [("enable_psr", "psr"), ("enable_fbc", "fbc"), ("enable_dc", "dc"), ("enable_panel_replay", "pr")] {
        let v = read_line(&format!("/sys/module/xe/parameters/{param}"))
            .or_else(|| read_line(&format!("/sys/module/i915/parameters/{param}")));
        if let Some(v) = v {
            xe.push(format!("{short}={v}"));
        }
    }
    c.xe = xe.join(" ");
    let mut found = [false; DAEMONS.len()];
    for d in dir_names("/proc") {
        if !d.starts_with(|ch: char| ch.is_ascii_digit()) {
            continue;
        }
        if let Some(comm) = read_line(&format!("/proc/{d}/comm")) {
            if let Some(i) = DAEMONS.iter().position(|k| *k == comm) {
                found[i] = true;
            }
        }
    }
    c.procs = DAEMONS.iter().zip(found).filter(|(_, f)| *f).map(|(k, _)| *k).collect();
    let cmdline = read_line("/proc/cmdline").unwrap_or_default();
    c.cmd = cmdline
        .split(' ')
        .filter(|t| {
            !(t.is_empty()
                || ["root=", "rootflags=", "initrd=", "BOOT_IMAGE=", "cryptdevice=", "resume="].iter().any(|p| t.starts_with(p))
                || *t == "rw"
                || *t == "ro")
        })
        .collect::<Vec<_>>()
        .join(" ");
    c.kern = read_line("/proc/sys/kernel/osrelease").unwrap_or_default();
    c
}

struct Row {
    i: usize,
    w: [f64; 2],
    cpu: [f64; 2],
    gpu: [f64; 2],
    wk: [f64; 2],
    score: f64,
}

fn ctx_cell(has: bool, dyn_w: Option<f64>, wsum: f64, w: f64, cpu: f64, gpu: f64, wk: Option<f64>) -> String {
    if !has {
        return "     -     -     -      -".into();
    }
    let ww = match dyn_w {
        Some(d) if wsum > 0.0 => format!("{:5.2}", d * w / wsum),
        _ => "    -".into(),
    };
    let wk = wk.map(|v| format!("{v:6.0}")).unwrap_or_else(|| "     -".into());
    format!(" {ww} {cpu:5.1} {gpu:5.1} {wk}")
}

fn row2(o: &mut String, label: &str, a: &str, b: &str) {
    let _ = writeln!(o, "{} {} {}", pad(label, 13), pad(a, 34), b);
}

pub fn daemon_pid(paths: &Paths) -> Option<i32> {
    let pid: i32 = read_line(&paths.pid)?.trim().parse().ok()?;
    if read_line(&format!("/proc/{pid}/comm")).as_deref() != Some("battrack") {
        return None;
    }
    // vivant = le verrou du fichier pid est tenu
    let f = std::fs::File::open(&paths.pid).ok()?;
    use std::os::fd::AsRawFd;
    let held = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
    held.then_some(pid)
}

fn flush_daemon(pid: i32, path: &str) {
    let before = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    unsafe { libc::kill(pid, libc::SIGUSR1) };
    for _ in 0..80 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        let now = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if now.is_some() && now != before {
            return;
        }
    }
}

pub fn report() -> String {
    let mut t = Tracker::new(60);
    let pid = daemon_pid(&t.paths);
    if let Some(p) = pid {
        flush_daemon(p, &t.paths.state);
    }
    let state_path = t.paths.state.clone();
    t.acc.load(&mut t.apps, &state_path, false);
    let hw = &t.hw;
    let acc = &t.acc;
    let apps = &t.apps;
    let cf = read_conf(hw);
    let mut sys = crate::hw::Sys::default();
    hw.read_bat(&mut sys);
    let hz = sysconf(libc::_SC_CLK_TCK).max(1) as f64;

    let mut o = String::with_capacity(8192);
    let now = unix_now();
    let since = if acc.since > 0 { acc.since } else { now };
    let (tn, ts) = (local(now), local(since));
    let _ = write!(
        o,
        "### battrack {} · {:02}/{:02} {:02}:{:02} · depuis {:02}/{:02} {:02}:{:02} ({})",
        env!("CARGO_PKG_VERSION"),
        tn.mday,
        tn.mon,
        tn.hour,
        tn.min,
        ts.mday,
        ts.mon,
        ts.hour,
        ts.min,
        dur((now - since) as f64)
    );
    match pid {
        Some(p) => {
            let mut b = [0u8; 1024];
            if let Some(st) = read_into(&format!("/proc/{p}/stat"), &mut b) {
                if let Some(rp) = st.iter().rposition(|&c| c == b')') {
                    let f: Vec<u64> = st[rp + 2..].split(|&c| c == b' ').map(num).collect();
                    if f.len() > 19 {
                        let cpu = (f[11] + f[12]) as f64 / hz;
                        let age = boottime() - f[19] as f64 / hz;
                        let _ = write!(o, " · démon {cpu:.1}s CPU ({:.4}%)", if age > 0.0 { cpu / age * 100.0 } else { 0.0 });
                    }
                }
            }
        }
        None => o.push_str(" · DÉMON ARRÊTÉ (systemctl --user start battrack)"),
    }
    o.push('\n');

    let full = hw.bat_num("energy_full").map(|v| v / 1e6);
    let design = hw.bat_num("energy_full_design").map(|v| v / 1e6);
    let rapl = Hw::rapl(&hw.pkg).is_some() as u8 + Hw::rapl(&hw.psys).is_some() as u8;
    let _ = writeln!(
        o,
        "MACHINE  bat {:.1}/{:.1} Wh ({:.0}%) · {} cycles · {}% {} · noyau {} · RAPL {}",
        full.unwrap_or(-1.0),
        design.unwrap_or(-1.0),
        match (full, design) {
            (Some(f), Some(d)) if d > 0.0 => f / d * 100.0,
            _ => 0.0,
        },
        hw.bat_num("cycle_count").map(|v| v as i64).unwrap_or(-1),
        sys.pct,
        sys.status,
        cf.kern,
        match rapl {
            2 => "pkg+psys",
            1 => "partiel",
            _ => "illisible",
        }
    );

    let c = &acc.c;
    let has = [c[0].t >= 60.0, c[1].t >= 60.0];
    row2(&mut o, "", "── BATTERIE ──", "── SECTEUR ──");
    let chg = if c[1].chg_t > 60.0 { format!(" · en charge {:.0}% (exclu des W psys)", c[1].chg_t / c[1].t * 100.0) } else { String::new() };
    row2(&mut o, "temps actif", &dur(c[0].t), &format!("{}{chg}", dur(c[1].t)));

    let batw = (c[0].t_bat >= 60.0).then(|| if c[0].wh > 0.05 { c[0].wh / (c[0].t / 3600.0) } else { c[0].j_bat / c[0].t_bat });
    let psysw: Vec<Option<f64>> = c.iter().map(|x| (x.t_psys >= 60.0).then(|| x.j_psys / x.t_psys)).collect();
    let pkgw: Vec<Option<f64>> = c.iter().map(|x| (x.t_pkg >= 60.0).then(|| x.j_pkg / x.t_pkg)).collect();
    let q = |h: &[u32], v: f64| hist_q(h, v).unwrap_or(-1.0);
    let cell0 = match batw {
        Some(w) => format!("{w:.2} (p10 {:.1} · p90 {:.1})", q(&c[0].h_bat, 0.1), q(&c[0].h_bat, 0.9)),
        None => if has[0] { "?" } else { "—" }.into(),
    };
    row2(&mut o, "P batterie W", &cell0, "— (branché)");
    let cells: Vec<String> = (0..2)
        .map(|i| match psysw[i] {
            Some(w) => format!("{w:.2} (p10 {:.1} · p90 {:.1})", q(&c[i].h_psys, 0.1), q(&c[i].h_psys, 0.9)),
            None => "—".into(),
        })
        .collect();
    row2(&mut o, "P psys W", &cells[0], &cells[1]);
    let cells: Vec<String> = (0..2)
        .map(|i| match pkgw[i] {
            Some(w) => format!("{w:.2} (repos {:.2} · p90 {:.1})", q(&c[i].h_pkg, 0.05), q(&c[i].h_pkg, 0.9)),
            None => "—".into(),
        })
        .collect();
    row2(&mut o, "P package W", &cells[0], &cells[1]);
    let cells: Vec<String> = (0..2)
        .map(|i| {
            let sys = if i == 0 { batw.or(psysw[0]) } else { psysw[1] };
            match (sys, pkgw[i]) {
                (Some(s), Some(p)) => format!("{:.2} (écran, wifi, ssd…)", s - p),
                _ => "—".into(),
            }
        })
        .collect();
    row2(&mut o, "hors package", &cells[0], &cells[1]);
    let cells: Vec<String> = (0..2)
        .map(|i| {
            let x = &c[i];
            if !has[i] {
                return "—".into();
            }
            format!(
                "{:.1}% · {:.1}k ctx/s · {:.1}k irq/s",
                if x.tot > 0 { 100.0 * x.busy as f64 / x.tot as f64 } else { 0.0 },
                x.ctxt as f64 / x.t / 1000.0,
                x.intr as f64 / x.t / 1000.0
            )
        })
        .collect();
    row2(&mut o, "CPU occupé", &cells[0], &cells[1]);
    let cells: Vec<String> = (0..2)
        .map(|i| {
            let x = &c[i];
            if !has[i] || x.cst_den <= 0.0 {
                return "—".into();
            }
            let mut parts = Vec::new();
            for (j, n) in acc.cst_names.iter().enumerate() {
                let v = x.cst[j] / x.cst_den * 100.0;
                if n.starts_with("POLL") && v < 0.5 {
                    continue;
                }
                parts.push(format!("{} {v:.0}", n.split("_ACPI").next().unwrap_or(n)));
            }
            parts.join(" ")
        })
        .collect();
    row2(&mut o, "C-états %", &cells[0], &cells[1]);
    let cells: Vec<String> = (0..2)
        .map(|i| {
            let x = &c[i];
            if !has[i] || x.gt_den <= 0.0 || acc.gt_names.is_empty() {
                return "—".into();
            }
            acc.gt_names
                .iter()
                .enumerate()
                .map(|(j, n)| format!("{n} {:.0}%", x.gt[j] / x.gt_den * 100.0))
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .collect();
    row2(&mut o, "GPU au repos", &cells[0], &cells[1]);
    let env = |f: &dyn Fn(&Ctx) -> Option<String>| -> Vec<String> {
        (0..2).map(|i| if has[i] { f(&c[i]).unwrap_or_else(|| "—".into()) } else { "—".into() }).collect()
    };
    let cells = env(&|x| (x.br_t > 0.0).then(|| format!("lum {:.0}% · éteint {:.0}%", x.br_sum / x.br_t, x.scr_off / x.t * 100.0)));
    row2(&mut o, "écran", &cells[0], &cells[1]);
    let cells = env(&|x| (x.kbd_t > 0.0).then(|| format!("moy {:.0}% · allumé {:.0}% du temps", x.kbd_sum / x.kbd_t, x.kbd_on / x.kbd_t * 100.0)));
    row2(&mut o, "clavier", &cells[0], &cells[1]);
    let cells = env(&|x| {
        (x.rad_t > 0.0).then(|| {
            format!(
                "wifi {:.0}% (co {:.0}%) · bt {:.0}%",
                x.wifi_on / x.rad_t * 100.0,
                x.wifi_up / x.rad_t * 100.0,
                x.bt_on / x.rad_t * 100.0
            )
        })
    });
    row2(&mut o, "radios", &cells[0], &cells[1]);
    let cap = full.filter(|&f| f > 0.0).unwrap_or(50.0);
    let a0 = batw.filter(|&w| w > 0.1).map(|w| format!("~{} ({cap:.0} Wh)", dur(cap / w * 3600.0))).unwrap_or_else(|| "—".into());
    let a1 = psysw[1].filter(|&w| w > 0.1).map(|w| format!("si débranché ~{}", dur(cap / w * 3600.0))).unwrap_or_else(|| "—".into());
    row2(&mut o, "autonomie", &a0, &a1);

    for i in 0..2 {
        if !has[i] || acc.modes.is_empty() {
            continue;
        }
        let x = &c[i];
        let mut parts = Vec::new();
        for (m, name) in acc.modes.iter().enumerate() {
            if x.m_t[m] < 1.0 {
                continue;
            }
            let mut s = format!("{} {:.0}%", mode_short(name), x.m_t[m] / x.t * 100.0);
            if x.m_tsys[m] > 30.0 {
                let _ = write!(s, " {:.1}W", x.m_jsys[m] / x.m_tsys[m]);
            }
            if x.m_tpkg[m] > 30.0 {
                let _ = write!(s, " (pkg {:.1})", x.m_jpkg[m] / x.m_tpkg[m]);
            }
            parts.push(s);
        }
        let _ = writeln!(o, "modes {} : {}", if i == 1 { "secteur " } else { "batterie" }, parts.join(" · "));
    }
    if acc.susp_s > 60.0 || acc.sleep_s > 60.0 {
        let mut s = String::from("veille  ");
        if acc.susp_s > 60.0 {
            let _ = write!(s, " sur batterie {} · {:.2} Wh · {:.2} W ·", dur(acc.susp_s), acc.susp_wh, acc.susp_wh / (acc.susp_s / 3600.0));
        }
        if acc.sleep_s > 60.0 {
            let _ = write!(s, " sommeil matériel (S0ix) {:.0}% de {}", acc.sleep_hw_s / acc.sleep_s * 100.0, dur(acc.sleep_s));
        } else {
            s.push_str(" sommeil matériel : pas encore mesuré");
        }
        let _ = writeln!(o, "{s}");
    }
    if !acc.sess.is_empty() {
        let k0 = acc.sess.len().saturating_sub(4);
        let parts: Vec<String> = acc.sess[k0..]
            .iter()
            .map(|x| {
                let tm = local(x.start);
                format!(
                    "{:02}/{:02} {:02}:{:02} {} {:.1}W {}→{}%",
                    tm.mday,
                    tm.mon,
                    tm.hour,
                    tm.min,
                    dur(x.act_s),
                    if x.act_s > 60.0 { x.act_wh / (x.act_s / 3600.0) } else { 0.0 },
                    x.p0,
                    x.p1
                )
            })
            .collect();
        let _ = writeln!(o, "sessions batterie: {}", parts.join(" | "));
    }

    let mhz = if cf.capmhz > 0 && cf.maxmhz > 0 && cf.capmhz < cf.maxmhz { format!(" fmax={}/{}MHz", cf.capmhz, cf.maxmhz) } else { String::new() };
    let or_q = |s: &str| if s.is_empty() { "?".to_string() } else { s.to_string() };
    let _ = writeln!(
        o,
        "CONF     maintenant: ppd={} plat={} {} gov={} epp={} turbo={}{mhz}",
        or_q(&cf.ppd),
        or_q(&cf.plat),
        cf.drv,
        cf.gov,
        cf.epp,
        if cf.turbo { "on" } else { "off" }
    );
    let onoff = |v: Option<bool>| match v {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    };
    let _ = writeln!(
        o,
        "  écran {}@{} vrr={} lum={}% · clavier={}/{} · wifi ps={} · bt={} · aspm={} · usb_as={:.0}s · wb={:.0}s{}{}",
        or_q(&cf.res),
        cf.hz,
        or_q(&cf.vrr),
        cf.bright,
        cf.kbd,
        cf.kbdmax,
        or_q(&cf.wifi),
        onoff(cf.bt),
        or_q(&cf.aspm),
        cf.usbas,
        cf.dirty,
        if cf.xe.is_empty() { "" } else { " · " },
        cf.xe
    );
    if !cf.pci.is_empty() {
        let _ = writeln!(o, "  pci pm=on ({}): {}", cf.pci.len(), cf.pci.join(" "));
    }
    if !cf.procs.is_empty() {
        let _ = writeln!(o, "  démons: {}", cf.procs.join(" "));
    }
    let _ = writeln!(o, "  cmdline: {}", cf.cmd);

    // tableau des applis, les deux contextes côte à côte
    let mut wsum = [0.0; 2];
    let mut dyn_w = [None, None];
    let mut irq = [0.0; 2];
    let mut rest = [0.0; 2];
    for i in 0..2 {
        if !has[i] {
            continue;
        }
        let x = &c[i];
        irq[i] = x.irq as f64 / hz;
        rest[i] = (x.busy as f64 - x.attr as f64 - x.irq as f64).max(0.0) / hz;
        wsum[i] = irq[i] + rest[i];
        let base = hist_q(&x.h_pkg, 0.05);
        dyn_w[i] = match (pkgw[i], base) {
            (Some(p), Some(b)) => Some((p - b).max(0.0)),
            _ if i == 0 => match (batw, hist_q(&x.h_bat, 0.05)) {
                (Some(w), Some(b)) => Some((w - b).max(0.0)),
                _ => None,
            },
            _ => None,
        };
    }
    let mut rows: Vec<Row> = Vec::new();
    for (ai, a) in apps.list.iter().enumerate() {
        let mut r = Row { i: ai, w: [0.0; 2], cpu: [0.0; 2], gpu: [0.0; 2], wk: [0.0; 2], score: 0.0 };
        for i in 0..2 {
            if !has[i] {
                continue;
            }
            let t = c[i].t;
            r.w[i] = a.cpu[i] as f64 / hz + a.gpu[i] + a.vid[i];
            r.cpu[i] = a.cpu[i] as f64 * 100.0 / hz / t;
            r.gpu[i] = (a.gpu[i] + a.vid[i]) * 100.0 / t;
            r.wk[i] = a.wk[i] as f64 / t;
            wsum[i] += r.w[i];
        }
        if r.w.iter().any(|&w| w > 0.0) {
            rows.push(r);
        }
    }
    for r in &mut rows {
        r.score = (0..2).filter(|&i| wsum[i] > 0.0).map(|i| r.w[i] / wsum[i]).sum();
    }
    rows.sort_by(|a, b| b.score.total_cmp(&a.score));
    o.push_str("\nTOP applis                ──────── BATTERIE ──────  ──────── SECTEUR ───────\n");
    o.push_str("                             W~  CPU%  GPU%  rév/s     W~  CPU%  GPU%  rév/s\n");
    for (k, r) in rows.iter().take(15).enumerate() {
        let name: String = apps.list[r.i].name.chars().take(22).collect();
        let courts = name.starts_with("[courts]");
        let cells: Vec<String> = (0..2)
            .map(|i| ctx_cell(has[i], dyn_w[i], wsum[i], r.w[i], r.cpu[i], r.gpu[i], (!courts).then_some(r.wk[i])))
            .collect();
        let _ = writeln!(o, "{:2} {}{} {}", k + 1, pad(&name, 22), cells[0], cells[1]);
    }
    for (label, v) in [("[irq/softirq]", irq), ("[autres]", rest)] {
        let cells: Vec<String> = (0..2)
            .map(|i| ctx_cell(has[i], dyn_w[i], wsum[i], v[i], if has[i] { v[i] * 100.0 / c[i].t } else { 0.0 }, 0.0, None))
            .collect();
        let _ = writeln!(o, "   {}{} {}", pad(label, 22), cells[0], cells[1]);
    }
    let gparts: Vec<String> = (0..2)
        .filter(|&i| has[i] && c[i].gt_den > 0.0 && !acc.gt_names.is_empty())
        .map(|i| {
            let att: f64 = rows.iter().map(|r| r.gpu[i]).sum();
            format!(
                "{} : GPU 3D occupé {:.1}%, attribué {:.1}%",
                if i == 1 { "secteur" } else { "batterie" },
                100.0 - c[i].gt[0] / c[i].gt_den * 100.0,
                att
            )
        })
        .collect();
    if !gparts.is_empty() {
        let _ = writeln!(o, "   {} (reste = kwin, illisible sans root)", gparts.join(" · "));
    }
    let vids: Vec<String> = apps
        .list
        .iter()
        .filter_map(|a| {
            let v0 = if has[0] { a.vid[0] * 100.0 / c[0].t } else { 0.0 };
            let v1 = if has[1] { a.vid[1] * 100.0 / c[1].t } else { 0.0 };
            (v0 >= 0.1 || v1 >= 0.1).then(|| format!("{} {v0:.1}/{v1:.1}%", a.name))
        })
        .collect();
    if !vids.is_empty() {
        let _ = writeln!(o, "   vidéo matérielle (bat/sect): {}", vids.join(", "));
    }

    let mut hints: Vec<String> = Vec::new();
    if !has[0] {
        hints.push("pas de données batterie : débranche ≥1 h".into());
    }
    if cf.hz >= 90 {
        hints.push(format!("écran {}Hz → 60Hz sur batterie (≈0,5–1 W)", cf.hz));
    }
    if let Some(i) = (0..2).find(|&i| has[i] && c[i].br_t > 0.0 && c[i].br_sum / c[i].br_t > 55.0) {
        hints.push(format!(
            "luminosité moy {:.0}% ({}) : -15 pts ≈ -0,3 à -0,8 W",
            c[i].br_sum / c[i].br_t,
            if i == 1 { "secteur" } else { "batterie" }
        ));
    }
    if let Some(i) = (0..2).find(|&i| has[i] && c[i].kbd_t > 0.0 && c[i].kbd_on / c[i].kbd_t > 0.2) {
        hints.push(format!("clavier rétroéclairé {:.0}% du temps (≈0,1–0,3 W)", c[i].kbd_on / c[i].kbd_t * 100.0));
    }
    if has[0] {
        let perf: f64 = acc.modes.iter().enumerate().filter(|(_, m)| m.starts_with("performance")).map(|(i, _)| c[0].m_t[i]).sum();
        if perf / c[0].t > 0.2 {
            hints.push(format!("mode perf {:.0}% du temps sur batterie → éco", perf / c[0].t * 100.0));
        }
    }
    if let Some(i) = (0..2).find(|&i| has[i] && c[i].rad_t > 0.0 && c[i].bt_on / c[i].rad_t > 0.5) {
        hints.push(format!("Bluetooth allumé {:.0}% du temps : couper si inutile", c[i].bt_on / c[i].rad_t * 100.0));
    }
    if acc.sleep_s > 3600.0 && acc.sleep_hw_s / acc.sleep_s < 0.8 {
        hints.push(format!("veille : S0ix seulement {:.0}% du temps → un périphérique empêche le sommeil profond", acc.sleep_hw_s / acc.sleep_s * 100.0));
    }
    if !cf.pci.is_empty() {
        hints.push(format!("{} périph. PCI runtime-PM=on → auto", cf.pci.len()));
    }
    if let Some(r) = rows.first().filter(|r| r.score > 0.5) {
        let n = (0..2).filter(|&i| wsum[i] > 0.0).count().max(1);
        hints.push(format!("{} = {:.0}% de l'activité", apps.list[r.i].name, r.score / n as f64 * 100.0));
    }
    if cf.wifi == "off" {
        hints.push("wifi power_save off → on".into());
    }
    if !hints.is_empty() {
        let shown: Vec<&String> = hints.iter().take(6).collect();
        let first: Vec<&str> = shown.iter().take(3).map(|s| s.as_str()).collect();
        let second: Vec<&str> = shown.iter().skip(3).map(|s| s.as_str()).collect();
        let _ = writeln!(o, "PISTES   {}", first.join(" · "));
        if !second.is_empty() {
            let _ = writeln!(o, "         {}", second.join(" · "));
        }
    }
    o.push_str("W~ = part du package (CPU+GPU+RAM) au-dessus du repos · psys = plateforme entière · rév/s = réveils/s\n");
    o.push_str("[courts] = processus trop brefs pour être vus, retrouvés par cgroup (konsole = commandes lancées au terminal)\n");
    o.push_str("### fin\n");
    o
}
