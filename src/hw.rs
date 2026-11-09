use crate::util::*;

pub const MAXCST: usize = 10;

#[derive(Default)]
pub struct Hw {
    pub bat: String,
    pub backlight: String,
    pub dpms: String,
    pub gt: Vec<(String, String)>,
    pub cst_names: Vec<String>,
    cst_paths: Vec<String>,
    pub ncpu: usize,
    pub pkg: String,
    pub psys: String,
    pub rmax_pkg: f64,
    pub rmax_psys: f64,
    pub kbd: String,
    pub kbd_max: f64,
    pub wifi_oper: String,
}

#[derive(Clone, Default)]
pub struct Sys {
    pub mono: f64,
    pub boot: f64,
    pub wall: i64,
    pub on_bat: bool,
    pub pct: i32,
    pub status: String,
    pub pw: f64,
    pub ewh: Option<f64>,
    pub busy: u64,
    pub tot: u64,
    pub irq: u64,
    pub ctxt: u64,
    pub intr: u64,
    pub cst: Vec<f64>,
    pub ncpu_ok: usize,
    pub lpi: Option<f64>,
    pub hw_sleep: Option<f64>,
    pub gt: Vec<f64>,
    pub bright: Option<f64>,
    pub scr_off: bool,
    pub kbd: Option<f64>,
    pub pkg: Option<f64>,
    pub psys: Option<f64>,
    pub wifi: Option<bool>,
    pub wifi_up: Option<bool>,
    pub bt: Option<bool>,
    pub mode: String,
}

impl Sys {
    pub fn charging(&self) -> bool {
        self.status == "Charging"
    }
}

impl Hw {
    pub fn discover() -> Self {
        let mut h = Hw::default();
        for n in dir_names("/sys/class/power_supply") {
            if read_line(&format!("/sys/class/power_supply/{n}/type")).as_deref() == Some("Battery") {
                h.bat = format!("/sys/class/power_supply/{n}");
                break;
            }
        }
        if let Some(n) = dir_names("/sys/class/backlight").into_iter().next() {
            h.backlight = format!("/sys/class/backlight/{n}");
        }
        if let Some(n) = dir_names("/sys/class/drm").into_iter().find(|n| n.contains("-eDP-")) {
            h.dpms = format!("/sys/class/drm/{n}/dpms");
        }
        'cards: for c in 0..4 {
            for g in 0..4 {
                if h.gt.len() >= 2 {
                    break 'cards;
                }
                let xe = format!("/sys/class/drm/card{c}/device/tile0/gt{g}/gtidle/idle_residency_ms");
                let i915 = format!("/sys/class/drm/card{c}/gt/gt{g}/rc6_residency_ms");
                if exists(&xe) {
                    h.gt.push((xe, if g == 0 { "rc" } else { "mc" }.to_string()));
                } else if exists(&i915) {
                    h.gt.push((i915, format!("gt{g}")));
                }
            }
        }
        for i in 0..MAXCST {
            match read_line(&format!("/sys/devices/system/cpu/cpu0/cpuidle/state{i}/name")) {
                Some(n) => h.cst_names.push(n),
                None => break,
            }
        }
        h.ncpu = sysconf(libc::_SC_NPROCESSORS_CONF).max(1) as usize;
        for c in 0..h.ncpu {
            for j in 0..h.cst_names.len() {
                h.cst_paths.push(format!("/sys/devices/system/cpu/cpu{c}/cpuidle/state{j}/time"));
            }
        }
        for i in 0..4 {
            let base = format!("/sys/class/powercap/intel-rapl:{i}");
            let name = read_line(&format!("{base}/name")).unwrap_or_default();
            let max = read_num(&format!("{base}/max_energy_range_uj")).unwrap_or(0.0);
            match name.as_str() {
                "package-0" => (h.pkg, h.rmax_pkg) = (format!("{base}/energy_uj"), max),
                "psys" => (h.psys, h.rmax_psys) = (format!("{base}/energy_uj"), max),
                _ => {}
            }
        }
        if let Some(n) = dir_names("/sys/class/leds").into_iter().find(|n| n.contains("kbd_backlight")) {
            h.kbd = format!("/sys/class/leds/{n}");
            h.kbd_max = read_num(&format!("{}/max_brightness", h.kbd)).unwrap_or(0.0);
        }
        if let Some(n) = dir_names("/sys/class/net")
            .into_iter()
            .find(|n| exists(&format!("/sys/class/net/{n}/wireless")))
        {
            h.wifi_oper = format!("/sys/class/net/{n}/operstate");
        }
        h
    }

    pub fn bat_num(&self, name: &str) -> Option<f64> {
        if self.bat.is_empty() {
            return None;
        }
        read_num(&format!("{}/{name}", self.bat))
    }

    pub fn rapl(path: &str) -> Option<f64> {
        if path.is_empty() {
            None
        } else {
            read_num(path)
        }
    }

    pub fn read_bat(&self, s: &mut Sys) {
        s.pct = -1;
        if self.bat.is_empty() {
            return;
        }
        s.status = read_line(&format!("{}/status", self.bat)).unwrap_or_default();
        s.on_bat = s.status == "Discharging";
        let mut pw = self.bat_num("power_now").unwrap_or(-1.0);
        if pw <= 0.0 {
            pw = match (self.bat_num("current_now"), self.bat_num("voltage_now")) {
                (Some(c), Some(v)) if c > 0.0 && v > 0.0 => c * v / 1e6,
                _ => 0.0,
            };
        }
        s.pw = pw / 1e6;
        s.ewh = match self.bat_num("energy_now") {
            Some(e) => Some(e / 1e6),
            None => match (self.bat_num("charge_now"), self.bat_num("voltage_min_design")) {
                (Some(c), Some(v)) if v > 0.0 => Some(c * v / 1e12),
                _ => None,
            },
        };
        s.pct = self.bat_num("capacity").map(|v| v as i32).unwrap_or(-1);
    }

    pub fn sample(&self, buf: &mut Vec<u8>) -> Sys {
        let mut s = Sys { mono: mono(), boot: boottime(), wall: unix_now(), ..Default::default() };
        self.read_bat(&mut s);
        if read_all("/proc/stat", buf) {
            for line in buf.split(|&c| c == b'\n') {
                if line.starts_with(b"cpu ") {
                    let v: Vec<u64> = line.split(|&c| c == b' ').filter(|t| !t.is_empty()).skip(1).take(8).map(num).collect();
                    if v.len() == 8 {
                        s.busy = v[0] + v[1] + v[2] + v[5] + v[6] + v[7];
                        s.tot = s.busy + v[3] + v[4];
                        s.irq = v[5] + v[6];
                    }
                } else if let Some(r) = line.strip_prefix(b"ctxt ") {
                    s.ctxt = num(r);
                } else if let Some(r) = line.strip_prefix(b"intr ") {
                    s.intr = num(r);
                }
            }
        }
        let ncst = self.cst_names.len();
        s.cst = vec![0.0; MAXCST];
        if ncst > 0 {
            for chunk in self.cst_paths.chunks(ncst) {
                let mut ok = false;
                for (j, p) in chunk.iter().enumerate() {
                    if let Some(v) = read_num(p) {
                        s.cst[j] += v;
                        ok = true;
                    }
                }
                s.ncpu_ok += ok as usize;
            }
        }
        s.lpi = read_num("/sys/devices/system/cpu/cpuidle/low_power_idle_system_residency_us");
        s.hw_sleep = read_num("/sys/power/suspend_stats/total_hw_sleep");
        s.gt = self.gt.iter().map(|(p, _)| read_num(p).unwrap_or(0.0)).collect();
        if !self.backlight.is_empty() {
            let v = read_num(&format!("{}/actual_brightness", self.backlight));
            let m = read_num(&format!("{}/max_brightness", self.backlight));
            if let (Some(v), Some(m)) = (v, m) {
                if m > 0.0 {
                    s.bright = Some(v / m * 100.0);
                }
            }
        }
        if !self.dpms.is_empty() {
            s.scr_off = read_line(&self.dpms).as_deref() == Some("Off");
        }
        s.pkg = Hw::rapl(&self.pkg);
        s.psys = Hw::rapl(&self.psys);
        if !self.kbd.is_empty() && self.kbd_max > 0.0 {
            s.kbd = read_num(&format!("{}/brightness", self.kbd)).map(|v| v / self.kbd_max * 100.0);
        }
        (s.wifi, s.bt) = radios();
        if !self.wifi_oper.is_empty() {
            s.wifi_up = read_line(&self.wifi_oper).map(|t| t == "up");
        }
        s.mode = mode();
        s
    }
}

/// Une radio est éteinte dès qu'un de ses interrupteurs rfkill la bloque.
pub fn radios() -> (Option<bool>, Option<bool>) {
    let (mut wifi, mut bt) = (None, None);
    for n in dir_names("/sys/class/rfkill") {
        if !n.starts_with("rfkill") {
            continue;
        }
        let base = format!("/sys/class/rfkill/{n}");
        let slot = match read_line(&format!("{base}/type")).as_deref() {
            Some("wlan") => &mut wifi,
            Some("bluetooth") => &mut bt,
            _ => continue,
        };
        let soft = read_num(&format!("{base}/soft")).unwrap_or(0.0) != 0.0;
        let hard = read_num(&format!("{base}/hard")).unwrap_or(0.0) != 0.0;
        let on = !soft && !hard;
        *slot = Some(slot.unwrap_or(true) && on);
    }
    (wifi, bt)
}

pub fn mode() -> String {
    let pp = read_line("/sys/firmware/acpi/platform_profile").unwrap_or_else(|| "?".into());
    let ep = read_line("/sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference")
        .unwrap_or_else(|| "?".into());
    format!("{pp}/{ep}")
}

pub fn mode_short(m: &str) -> String {
    let (pp, ep) = m.split_once('/').unwrap_or((m, ""));
    let p = match pp {
        "low-power" => "éco",
        "balanced" => "équil",
        "performance" => "perf",
        "quiet" => "silence",
        x => x,
    };
    let e = match ep {
        "power" => "pw",
        "balance_power" => "bpw",
        "balance_performance" => "bperf",
        "performance" => "perf",
        "default" => "def",
        x => x,
    };
    format!("{p}/epp:{e}")
}

/// Énergie RAPL entre deux lectures (J) ; le compteur reboucle à `max`.
pub fn rapl_delta(cur: Option<f64>, prev: Option<f64>, max: f64) -> Option<f64> {
    let (c, p) = (cur?, prev?);
    let mut d = c - p;
    if d < 0.0 {
        d += max;
    }
    (d >= 0.0).then_some(d / 1e6)
}
