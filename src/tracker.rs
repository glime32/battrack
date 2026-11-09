use std::fmt::Write as _;

use crate::hw::{mode_short, rapl_delta, Hw, Sys};
use crate::procs::{Apps, Scanner, RING};
use crate::state::*;
use crate::util::*;

pub struct Tracker {
    pub hw: Hw,
    pub apps: Apps,
    pub scan: Scanner,
    pub acc: Acc,
    pub paths: Paths,
    pub interval: u64,
    pub nowrite: bool,
    pub need_save: bool,
    prev: Option<Sys>,
    pwr: Vec<f64>,
    last_pkg_w: Option<f64>,
    last_psys_w: Option<f64>,
    statbuf: Vec<u8>,
    hz: f64,
    browser_file: bool,
}

pub fn browsers_from_env() -> Vec<String> {
    let s = std::env::var("BATTRACK_BROWSERS").unwrap_or_default();
    let s = if s.is_empty() { "helium".to_string() } else { s };
    s.split(',').filter(|x| !x.is_empty()).take(8).map(String::from).collect()
}

impl Tracker {
    pub fn new(interval: u64) -> Self {
        let hz = sysconf(libc::_SC_CLK_TCK).max(1) as u64;
        Tracker {
            hw: Hw::discover(),
            apps: Apps::default(),
            scan: Scanner::new(browsers_from_env(), hz),
            acc: Acc::default(),
            paths: Paths::new(),
            interval,
            nowrite: false,
            need_save: false,
            prev: None,
            pwr: Vec::new(),
            last_pkg_w: None,
            last_psys_w: None,
            statbuf: Vec::with_capacity(65536),
            hz: hz as f64,
            browser_file: false,
        }
    }

    pub fn on_battery(&self) -> bool {
        self.prev.as_ref().is_some_and(|s| s.on_bat)
    }

    pub fn save(&self) {
        if !self.nowrite {
            self.acc.save(&self.apps, &self.paths.state);
        }
    }

    pub fn step(&mut self) {
        let s = self.hw.sample(&mut self.statbuf);
        let prev = self.prev.take();
        let first = prev.is_none();
        let p = prev.unwrap_or_else(|| s.clone());
        let dt = if first { 0.0 } else { s.mono - p.mono };
        let susp = if first { 0.0 } else { (s.boot - p.boot) - dt };
        let ci = match (first, susp < 5.0 && dt > 0.0, s.on_bat, p.on_bat) {
            (false, true, true, true) => Some(0),
            (false, true, false, false) => Some(1),
            _ => None,
        };
        let gi = (self.scan.gs as usize) % RING;
        if !first {
            self.scan.dts[gi] = dt;
        }
        let fdall = self.scan.sc.is_multiple_of(10);
        let attr = self.scan.scan(&mut self.apps, s.mono, p.boot, first, ci, fdall, gi);
        let (mut dbusy, mut dtot) = (0, 0);
        self.last_pkg_w = None;
        self.last_psys_w = None;
        if !first {
            dbusy = s.busy.saturating_sub(p.busy);
            dtot = s.tot.saturating_sub(p.tot);
            let jp = rapl_delta(s.pkg, p.pkg, self.hw.rmax_pkg);
            let js = rapl_delta(s.psys, p.psys, self.hw.rmax_psys);
            if dt > 0.0 {
                self.last_pkg_w = jp.map(|j| j / dt);
                self.last_psys_w = js.map(|j| j / dt);
            }
            if susp >= 5.0 {
                if let (Some(a), Some(b)) = (s.hw_sleep, p.hw_sleep) {
                    self.acc.sleep_s += susp;
                    self.acc.sleep_hw_s += ((a - b) / 1e6).clamp(0.0, susp);
                }
            }
            if let Some(ci) = ci {
                self.account(ci, &s, &p, dt, dbusy, dtot, attr, jp, js);
            } else if s.on_bat && p.on_bat && susp >= 5.0 {
                self.acc.susp_s += susp;
                if let (Some(a), Some(b)) = (s.ewh, p.ewh) {
                    if b >= a {
                        self.acc.susp_wh += b - a;
                    }
                }
                if self.acc.sess_open {
                    if let Some(x) = self.acc.sess.last_mut() {
                        x.susp_s += susp;
                        x.p1 = s.pct;
                    }
                }
            }
        }
        if s.on_bat && !self.acc.sess_open {
            if self.acc.sess.len() == MAXSESS {
                self.acc.sess.remove(0);
            }
            self.acc.sess.push(Sess { start: s.wall, p0: s.pct, p1: s.pct, ..Default::default() });
            self.acc.sess_open = true;
            self.need_save = true;
        } else if !s.on_bat && self.acc.sess_open {
            self.acc.sess_open = false;
            if self.acc.sess.last().is_some_and(|x| x.act_s < 120.0) {
                self.acc.sess.pop();
            }
            self.pwr.clear();
            self.need_save = true;
        }
        self.write_now(&s, dt, dbusy, dtot);
        self.write_browser(gi);
        self.prev = Some(s);
        if !first {
            self.scan.gs += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn account(&mut self, ci: usize, s: &Sys, p: &Sys, dt: f64, dbusy: u64, dtot: u64, attr: u64, jp: Option<f64>, js: Option<f64>) {
        let charging = s.charging();
        let c = &mut self.acc.c[ci];
        c.t += dt;
        if charging {
            c.chg_t += dt;
        }
        c.busy += dbusy;
        c.tot += dtot;
        c.attr += attr;
        c.irq += s.irq.saturating_sub(p.irq);
        c.ctxt += s.ctxt.saturating_sub(p.ctxt);
        c.intr += s.intr.saturating_sub(p.intr);
        // puissance « système » : batterie quand on est dessus, psys sur secteur hors charge
        let mut wsys = None;
        if ci == 0 {
            let dwh = match (s.ewh, p.ewh) {
                (Some(a), Some(b)) if b >= a => b - a,
                _ => 0.0,
            };
            c.wh += dwh;
            let pw = if s.pw > 0.0 { s.pw } else { dwh * 3600.0 / dt };
            if pw > 0.0 {
                c.j_bat += pw * dt;
                c.t_bat += dt;
                hist_add(&mut c.h_bat, pw);
                wsys = Some(pw);
                self.pwr.push(pw);
                if self.pwr.len() > 10 {
                    self.pwr.remove(0);
                }
            }
            if self.acc.sess_open {
                if let Some(x) = self.acc.sess.last_mut() {
                    x.act_s += dt;
                    x.act_wh += dwh;
                    x.p1 = s.pct;
                }
            }
        }
        let c = &mut self.acc.c[ci];
        if let Some(j) = jp {
            c.j_pkg += j;
            c.t_pkg += dt;
            hist_add(&mut c.h_pkg, j / dt);
        }
        if let Some(j) = js {
            if ci == 1 && charging {
                c.j_psys_chg += j;
                c.t_psys_chg += dt;
            } else {
                c.j_psys += j;
                c.t_psys += dt;
                hist_add(&mut c.h_psys, j / dt);
                if ci == 1 {
                    wsys = Some(j / dt);
                }
            }
        }
        for j in 0..self.hw.cst_names.len() {
            c.cst[j] += s.cst[j] - p.cst[j];
        }
        c.cst_den += dt * 1e6 * s.ncpu_ok as f64;
        if let (Some(a), Some(b)) = (s.lpi, p.lpi) {
            if a >= b {
                c.lpi += a - b;
            }
        }
        for i in 0..s.gt.len().min(2) {
            c.gt[i] += s.gt[i] - p.gt.get(i).copied().unwrap_or(s.gt[i]);
        }
        c.gt_den += dt * 1000.0;
        if let Some(b) = s.bright {
            c.br_sum += b * dt;
            c.br_t += dt;
        }
        if s.scr_off {
            c.scr_off += dt;
        }
        if let Some(k) = s.kbd {
            c.kbd_sum += k * dt;
            c.kbd_t += dt;
            if k > 0.0 {
                c.kbd_on += dt;
            }
        }
        if s.wifi.is_some() || s.bt.is_some() {
            c.rad_t += dt;
            if s.wifi == Some(true) {
                c.wifi_on += dt;
            }
            if s.wifi_up == Some(true) {
                c.wifi_up += dt;
            }
            if s.bt == Some(true) {
                c.bt_on += dt;
            }
        }
        let mi = self.acc.mode_index(&s.mode);
        let c = &mut self.acc.c[ci];
        c.m_t[mi] += dt;
        if let Some(w) = wsys {
            c.m_jsys[mi] += w * dt;
            c.m_tsys[mi] += dt;
        }
        if let Some(j) = jp {
            c.m_jpkg[mi] += j;
            c.m_tpkg[mi] += dt;
        }
    }

    fn write_now(&self, s: &Sys, dt: f64, dbusy: u64, dtot: u64) {
        if self.nowrite {
            return;
        }
        let tm = local(s.wall);
        let mut o = format!(
            "battrack · {:02}:{:02} · {} {}% ({})",
            tm.hour,
            tm.min,
            if s.on_bat { "batterie" } else { "secteur" },
            s.pct,
            s.status
        );
        if s.on_bat && s.pw > 0.0 {
            let _ = write!(o, " · batt {:.1} W", s.pw);
            if !self.pwr.is_empty() {
                let _ = write!(o, " (moy 10 min {:.1})", self.pwr.iter().sum::<f64>() / self.pwr.len() as f64);
            }
        }
        if let Some(w) = self.last_psys_w {
            let _ = write!(o, " · psys {w:.1} W");
        }
        if let Some(w) = self.last_pkg_w {
            let _ = write!(o, " · pkg {w:.1} W");
        }
        o.push('\n');
        let onoff = |v: Option<bool>| match v {
            Some(true) => "on",
            Some(false) => "off",
            None => "?",
        };
        let _ = writeln!(
            o,
            "mode {} · écran {:.0}%{} · clavier {:.0}% · wifi {}{} · bt {}",
            mode_short(&s.mode),
            s.bright.unwrap_or(-1.0),
            if s.scr_off { " (éteint)" } else { "" },
            s.kbd.unwrap_or(-1.0),
            onoff(s.wifi),
            if s.wifi_up == Some(true) { " (connecté)" } else { "" },
            onoff(s.bt)
        );
        if dt > 0.0 && dtot > 0 {
            let _ = writeln!(
                o,
                "CPU occupé {:.1}% ({} cœurs) · {} processus",
                100.0 * dbusy as f64 / dtot as f64,
                self.hw.ncpu,
                self.scan.nprocs()
            );
            let mut idx: Vec<usize> = (0..self.apps.list.len()).collect();
            let score = |i: &usize| {
                let a = &self.apps.list[*i];
                a.last_tk as f64 + a.last_gpu * self.hz
            };
            idx.sort_by(|a, b| score(b).total_cmp(&score(a)));
            o.push_str("dernière minute :  CPU% (1 cœur)  GPU%  appli\n");
            for &i in idx.iter().take(10) {
                let a = &self.apps.list[i];
                if a.last_tk == 0 && a.last_gpu < 0.01 {
                    break;
                }
                let _ = writeln!(o, "  {:8.1} {:9.1}   {}", a.last_tk as f64 * 100.0 / self.hz / dt, a.last_gpu * 100.0 / dt, a.name);
            }
        }
        write_atomic(&self.paths.now, &o);
    }

    fn write_browser(&mut self, gi: usize) {
        if self.nowrite {
            return;
        }
        match self.scan.browser_table(gi, self.interval) {
            Some(t) => self.browser_file = write_atomic(&self.paths.browser, &t),
            None if self.browser_file => {
                let _ = std::fs::remove_file(&self.paths.browser);
                self.browser_file = false;
            }
            None => {}
        }
    }
}
