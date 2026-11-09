use std::fmt::Write as _;

use crate::hw::{Hw, MAXCST};
use crate::procs::Apps;
use crate::util::*;

pub const NHIST: usize = 600;
pub const MAXMODE: usize = 8;
pub const MAXSESS: usize = 8;

#[derive(Clone)]
pub struct Ctx {
    pub t: f64,
    pub chg_t: f64,
    pub wh: f64,
    pub j_bat: f64,
    pub t_bat: f64,
    pub j_pkg: f64,
    pub t_pkg: f64,
    pub j_psys: f64,
    pub t_psys: f64,
    pub j_psys_chg: f64,
    pub t_psys_chg: f64,
    pub h_bat: Vec<u32>,
    pub h_pkg: Vec<u32>,
    pub h_psys: Vec<u32>,
    pub busy: u64,
    pub tot: u64,
    pub attr: u64,
    pub irq: u64,
    pub ctxt: u64,
    pub intr: u64,
    pub cst: Vec<f64>,
    pub cst_den: f64,
    pub lpi: f64,
    pub gt: [f64; 2],
    pub gt_den: f64,
    pub br_sum: f64,
    pub br_t: f64,
    pub scr_off: f64,
    pub kbd_sum: f64,
    pub kbd_t: f64,
    pub kbd_on: f64,
    pub wifi_on: f64,
    pub wifi_up: f64,
    pub bt_on: f64,
    pub rad_t: f64,
    pub m_t: [f64; MAXMODE],
    pub m_jsys: [f64; MAXMODE],
    pub m_tsys: [f64; MAXMODE],
    pub m_jpkg: [f64; MAXMODE],
    pub m_tpkg: [f64; MAXMODE],
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx {
            t: 0.0,
            chg_t: 0.0,
            wh: 0.0,
            j_bat: 0.0,
            t_bat: 0.0,
            j_pkg: 0.0,
            t_pkg: 0.0,
            j_psys: 0.0,
            t_psys: 0.0,
            j_psys_chg: 0.0,
            t_psys_chg: 0.0,
            h_bat: vec![0; NHIST],
            h_pkg: vec![0; NHIST],
            h_psys: vec![0; NHIST],
            busy: 0,
            tot: 0,
            attr: 0,
            irq: 0,
            ctxt: 0,
            intr: 0,
            cst: vec![0.0; MAXCST],
            cst_den: 0.0,
            lpi: 0.0,
            gt: [0.0; 2],
            gt_den: 0.0,
            br_sum: 0.0,
            br_t: 0.0,
            scr_off: 0.0,
            kbd_sum: 0.0,
            kbd_t: 0.0,
            kbd_on: 0.0,
            wifi_on: 0.0,
            wifi_up: 0.0,
            bt_on: 0.0,
            rad_t: 0.0,
            m_t: [0.0; MAXMODE],
            m_jsys: [0.0; MAXMODE],
            m_tsys: [0.0; MAXMODE],
            m_jpkg: [0.0; MAXMODE],
            m_tpkg: [0.0; MAXMODE],
        }
    }
}

#[derive(Clone, Default)]
pub struct Sess {
    pub start: i64,
    pub act_s: f64,
    pub act_wh: f64,
    pub susp_s: f64,
    pub p0: i32,
    pub p1: i32,
}

#[derive(Default)]
pub struct Acc {
    pub since: i64,
    pub c: [Ctx; 2],
    pub cst_names: Vec<String>,
    pub gt_names: Vec<String>,
    pub modes: Vec<String>,
    pub susp_s: f64,
    pub susp_wh: f64,
    pub sleep_s: f64,
    pub sleep_hw_s: f64,
    pub sess: Vec<Sess>,
    pub sess_open: bool,
}

pub fn hist_add(h: &mut [u32], w: f64) {
    if w < 0.0 {
        return;
    }
    let i = ((w * 10.0) as usize).min(NHIST - 1);
    h[i] += 1;
}

/// Quantile d'un histogramme de puissance (W), None s'il est vide.
pub fn hist_q(h: &[u32], q: f64) -> Option<f64> {
    let t: u64 = h.iter().map(|&x| x as u64).sum();
    if t == 0 {
        return None;
    }
    let mut c = 0u64;
    for (i, &x) in h.iter().enumerate() {
        c += x as u64;
        if c > 0 && c as f64 >= q * t as f64 {
            return Some(i as f64 * 0.1 + 0.05);
        }
    }
    Some(NHIST as f64 * 0.1)
}

fn put_hist(out: &mut String, ci: usize, key: &str, h: &[u32]) {
    let _ = write!(out, "c{ci}\t{key}\t");
    let mut first = true;
    for (i, &x) in h.iter().enumerate() {
        if x > 0 {
            let _ = write!(out, "{}{i}:{x}", if first { "" } else { " " });
            first = false;
        }
    }
    out.push('\n');
}

fn get_hist(s: &str, h: &mut [u32]) {
    for t in s.split(' ') {
        if let Some((i, c)) = t.split_once(':') {
            if let (Ok(i), Ok(c)) = (i.parse::<usize>(), c.parse::<u32>()) {
                if i < NHIST {
                    h[i] = c;
                }
            }
        }
    }
}

impl Acc {
    pub fn mode_index(&mut self, m: &str) -> usize {
        if let Some(i) = self.modes.iter().position(|x| x == m) {
            return i;
        }
        if self.modes.len() < MAXMODE {
            self.modes.push(m.to_string());
            return self.modes.len() - 1;
        }
        MAXMODE - 1
    }

    pub fn reset(&mut self, hw: &Hw) {
        *self = Acc { since: unix_now(), ..Default::default() };
        self.align(hw);
    }

    /// Les C-états et GT enregistrés doivent correspondre au matériel actuel.
    pub fn align(&mut self, hw: &Hw) {
        if self.cst_names != hw.cst_names {
            for c in &mut self.c {
                c.cst = vec![0.0; MAXCST];
                c.cst_den = 0.0;
            }
            self.cst_names = hw.cst_names.clone();
        }
        let gtn: Vec<String> = hw.gt.iter().map(|(_, n)| n.clone()).collect();
        if self.gt_names != gtn {
            for c in &mut self.c {
                c.gt = [0.0; 2];
                c.gt_den = 0.0;
            }
            self.gt_names = gtn;
        }
    }

    pub fn save(&self, apps: &Apps, path: &str) {
        let mut o = String::with_capacity(16384);
        o.push_str("battrack-state\t2\n");
        let _ = writeln!(o, "since\t{}", self.since);
        for (i, m) in self.modes.iter().enumerate() {
            let _ = writeln!(o, "mode\t{i}\t{m}");
        }
        for (i, n) in self.cst_names.iter().enumerate() {
            let _ = writeln!(o, "cstn\t{i}\t{n}");
        }
        for (i, n) in self.gt_names.iter().enumerate() {
            let _ = writeln!(o, "gtn\t{i}\t{n}");
        }
        let _ = writeln!(o, "susp\t{:.1}\t{:.5}", self.susp_s, self.susp_wh);
        let _ = writeln!(o, "sleep\t{:.1}\t{:.1}", self.sleep_s, self.sleep_hw_s);
        for s in &self.sess {
            let _ = writeln!(o, "sess\t{}\t{:.0}\t{:.5}\t{:.0}\t{}\t{}", s.start, s.act_s, s.act_wh, s.susp_s, s.p0, s.p1);
        }
        for (ci, c) in self.c.iter().enumerate() {
            let _ = writeln!(o, "c{ci}\tt\t{:.1}\t{:.1}", c.t, c.chg_t);
            let _ = writeln!(
                o,
                "c{ci}\te\t{:.5}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
                c.wh, c.j_bat, c.t_bat, c.j_pkg, c.t_pkg, c.j_psys, c.t_psys
            );
            let _ = writeln!(o, "c{ci}\tpchg\t{:.1}\t{:.1}", c.j_psys_chg, c.t_psys_chg);
            let _ = writeln!(o, "c{ci}\tcpu\t{}\t{}\t{}\t{}\t{}", c.busy, c.tot, c.attr, c.ctxt, c.intr);
            let _ = writeln!(o, "c{ci}\tirq\t{}", c.irq);
            let _ = write!(o, "c{ci}\tcst\t{:.0}", c.cst_den);
            for v in c.cst.iter().take(self.cst_names.len()) {
                let _ = write!(o, "\t{v:.0}");
            }
            o.push('\n');
            let _ = writeln!(o, "c{ci}\tlpi\t{:.0}", c.lpi);
            let _ = writeln!(o, "c{ci}\tgt\t{:.0}\t{:.0}\t{:.0}", c.gt_den, c.gt[0], c.gt[1]);
            let _ = writeln!(
                o,
                "c{ci}\tenv\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
                c.br_sum, c.br_t, c.scr_off, c.kbd_sum, c.kbd_t, c.kbd_on, c.wifi_on, c.wifi_up, c.bt_on, c.rad_t
            );
            for i in 0..self.modes.len() {
                if c.m_t[i] > 0.0 {
                    let _ = writeln!(
                        o,
                        "c{ci}\tm\t{i}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
                        c.m_t[i], c.m_jsys[i], c.m_tsys[i], c.m_jpkg[i], c.m_tpkg[i]
                    );
                }
            }
            put_hist(&mut o, ci, "hb", &c.h_bat);
            put_hist(&mut o, ci, "hp", &c.h_pkg);
            put_hist(&mut o, ci, "hs", &c.h_psys);
        }
        for a in &apps.list {
            if a.cpu == [0; 2] && a.gpu.iter().chain(&a.vid).sum::<f64>() < 0.001 {
                continue;
            }
            let _ = writeln!(
                o,
                "app\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}",
                a.name, a.cpu[0], a.cpu[1], a.gpu[0], a.gpu[1], a.vid[0], a.vid[1], a.wk[0], a.wk[1]
            );
        }
        write_atomic(path, &o);
    }

    /// `migrate` (démon) : un état d'un ancien format est mis de côté.
    pub fn load(&mut self, apps: &mut Apps, path: &str, migrate: bool) {
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let mut lines = text.lines();
        if lines.next() != Some("battrack-state\t2") {
            if migrate {
                let _ = std::fs::rename(path, format!("{path}.ancien"));
            }
            return;
        }
        let mut saw_pchg = false;
        for line in lines {
            let v: Vec<&str> = line.split('\t').collect();
            if v.len() < 2 {
                continue;
            }
            let d = |i: usize| v.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let u = |i: usize| v.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let idx = |s: &str| s.parse::<usize>().ok();
            match v[0] {
                "since" => self.since = v[1].parse().unwrap_or(0),
                "mode" if v.len() >= 3 => {
                    if let Some(i) = idx(v[1]).filter(|&i| i < MAXMODE) {
                        if self.modes.len() <= i {
                            self.modes.resize(i + 1, String::new());
                        }
                        self.modes[i] = v[2].to_string();
                    }
                }
                "cstn" if v.len() >= 3 => {
                    if let Some(i) = idx(v[1]).filter(|&i| i < MAXCST) {
                        if self.cst_names.len() <= i {
                            self.cst_names.resize(i + 1, String::new());
                        }
                        self.cst_names[i] = v[2].to_string();
                    }
                }
                "gtn" if v.len() >= 3 => {
                    if let Some(i) = idx(v[1]).filter(|&i| i < 2) {
                        if self.gt_names.len() <= i {
                            self.gt_names.resize(i + 1, String::new());
                        }
                        self.gt_names[i] = v[2].to_string();
                    }
                }
                "susp" => (self.susp_s, self.susp_wh) = (d(1), d(2)),
                "sleep" => (self.sleep_s, self.sleep_hw_s) = (d(1), d(2)),
                "sess" if v.len() >= 7 && self.sess.len() < MAXSESS => self.sess.push(Sess {
                    start: v[1].parse().unwrap_or(0),
                    act_s: d(2),
                    act_wh: d(3),
                    susp_s: d(4),
                    p0: v[5].parse().unwrap_or(0),
                    p1: v[6].parse().unwrap_or(0),
                }),
                "app" if v.len() >= 10 => {
                    let i = apps.get(v[1]);
                    let a = &mut apps.list[i];
                    a.cpu = [u(2), u(3)];
                    a.gpu = [d(4), d(5)];
                    a.vid = [d(6), d(7)];
                    a.wk = [u(8), u(9)];
                }
                k if v.len() >= 3 && (k == "c0" || k == "c1") => {
                    let c = &mut self.c[(k == "c1") as usize];
                    match v[1] {
                        "t" => (c.t, c.chg_t) = (d(2), d(3)),
                        "e" => {
                            (c.wh, c.j_bat, c.t_bat) = (d(2), d(3), d(4));
                            (c.j_pkg, c.t_pkg, c.j_psys, c.t_psys) = (d(5), d(6), d(7), d(8));
                        }
                        "pchg" => {
                            (c.j_psys_chg, c.t_psys_chg) = (d(2), d(3));
                            saw_pchg = true;
                        }
                        "cpu" => (c.busy, c.tot, c.attr, c.ctxt, c.intr) = (u(2), u(3), u(4), u(5), u(6)),
                        "irq" => c.irq = u(2),
                        "cst" => {
                            c.cst_den = d(2);
                            for j in 0..MAXCST.min(v.len().saturating_sub(3)) {
                                c.cst[j] = d(3 + j);
                            }
                        }
                        "lpi" => c.lpi = d(2),
                        "gt" => (c.gt_den, c.gt[0], c.gt[1]) = (d(2), d(3), d(4)),
                        "env" => {
                            (c.br_sum, c.br_t, c.scr_off, c.kbd_sum, c.kbd_t) = (d(2), d(3), d(4), d(5), d(6));
                            (c.kbd_on, c.wifi_on, c.wifi_up, c.bt_on, c.rad_t) = (d(7), d(8), d(9), d(10), d(11));
                        }
                        "m" if v.len() >= 8 => {
                            if let Some(i) = idx(v[2]).filter(|&i| i < MAXMODE) {
                                (c.m_t[i], c.m_jsys[i], c.m_tsys[i], c.m_jpkg[i], c.m_tpkg[i]) = (d(3), d(4), d(5), d(6), d(7));
                            }
                        }
                        "hb" => get_hist(v[2], &mut c.h_bat),
                        "hp" => get_hist(v[2], &mut c.h_pkg),
                        "hs" => get_hist(v[2], &mut c.h_psys),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        // Avant la 1.2, psys sur secteur comptait aussi la charge de la batterie : ces chiffres sont faux.
        if !saw_pchg {
            let c = &mut self.c[1];
            c.j_psys = 0.0;
            c.t_psys = 0.0;
            c.h_psys = vec![0; NHIST];
            c.m_jsys = [0.0; MAXMODE];
            c.m_tsys = [0.0; MAXMODE];
        }
    }
}
