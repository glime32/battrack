mod host;
mod hw;
mod procs;
mod report;
mod state;
mod tracker;
mod util;

use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use hw::{mode_short, rapl_delta, Hw, Sys};
use tracker::Tracker;
use util::*;

static TERM: AtomicBool = AtomicBool::new(false);
static FLUSH: AtomicBool = AtomicBool::new(false);
static RESET: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    match sig {
        libc::SIGUSR1 => FLUSH.store(true, Ordering::Relaxed),
        libc::SIGUSR2 => RESET.store(true, Ordering::Relaxed),
        _ => TERM.store(true, Ordering::Relaxed),
    }
}

fn install_signals() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGUSR1, libc::SIGUSR2] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            // pas de SA_RESTART : le signal doit couper le sommeil
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// true = il faut s'arrêter
fn handle_signals(t: &mut Tracker) -> bool {
    if RESET.swap(false, Ordering::Relaxed) {
        let hw = std::mem::take(&mut t.hw);
        t.acc.reset(&hw);
        t.hw = hw;
        t.apps.clear_counters();
        t.save();
    }
    if FLUSH.swap(false, Ordering::Relaxed) {
        t.save();
    }
    if TERM.load(Ordering::Relaxed) {
        t.save();
        let _ = std::fs::remove_file(&t.paths.now);
        let _ = std::fs::remove_file(&t.paths.browser);
        return true;
    }
    false
}

fn lower_priority() {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        let sp = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &sp);
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
        // le noyau peut grouper notre réveil avec ceux des autres
        libc::prctl(libc::PR_SET_TIMERSLACK, 2_000_000_000u64);
    }
}

fn daemon(interval: u64) -> i32 {
    let mut t = Tracker::new(interval);
    let _ = std::fs::create_dir_all(&t.paths.run_dir);
    let _ = std::fs::create_dir_all(&t.paths.state_dir);
    let Ok(mut lock) = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&t.paths.pid) else {
        eprintln!("battrack: impossible d'ouvrir {}", t.paths.pid);
        return 1;
    };
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("battrack: démon déjà lancé");
        return 1;
    }
    let _ = lock.set_len(0);
    let _ = writeln!(lock, "{}", std::process::id());

    lower_priority();
    let state = t.paths.state.clone();
    t.acc.load(&mut t.apps, &state, true);
    if t.acc.since == 0 {
        t.acc.since = unix_now();
    }
    let hw = std::mem::take(&mut t.hw);
    t.acc.align(&hw);
    t.hw = hw;
    t.acc.sess_open = false;
    install_signals();

    let mut next = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut next) };
    let mut since_save = 0;
    loop {
        t.step();
        since_save += 1;
        if since_save >= if t.on_battery() { 15 } else { 60 } || t.need_save {
            t.save();
            since_save = 0;
            t.need_save = false;
        }
        if handle_signals(&mut t) {
            return 0;
        }
        next.tv_sec += interval as libc::time_t;
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        if next.tv_sec <= now.tv_sec {
            next = now;
            next.tv_sec += interval as libc::time_t;
        }
        while unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &next, std::ptr::null_mut()) } == libc::EINTR {
            if handle_signals(&mut t) {
                return 0;
            }
        }
    }
}

fn copy() -> i32 {
    let r = report::report();
    print!("{r}");
    for (cmd, args) in [("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
        let child = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        if let Ok(mut c) = child {
            if let Some(mut si) = c.stdin.take() {
                let _ = si.write_all(r.as_bytes());
            }
            if c.wait().is_ok_and(|s| s.success()) {
                break;
            }
        }
    }
    let _ = Command::new("notify-send")
        .args(["-a", "battrack", "-i", "battery", "Rapport batterie copié", "Colle-le dans Claude (Ctrl+V)"])
        .stderr(Stdio::null())
        .status();
    0
}

fn bench() -> i32 {
    let mut t = Tracker::new(60);
    t.nowrite = true;
    t.step();
    let cpu = || clock(libc::CLOCK_PROCESS_CPUTIME_ID);
    let a = cpu();
    for _ in 0..20 {
        t.step();
    }
    let ms = (cpu() - a) * 1000.0 / 20.0;
    println!(
        "1 échantillon = {ms:.2} ms CPU · {} processus · {} applis · {} clients GPU suivis",
        t.scan.nprocs(),
        t.apps.list.len(),
        t.scan.ndrm()
    );
    println!("à 1/min → {:.4}% d'un cœur", ms / 600.0);
    let (p0, s0, t0) = (Hw::rapl(&t.hw.pkg), Hw::rapl(&t.hw.psys), mono());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (p1, s1, dt) = (Hw::rapl(&t.hw.pkg), Hw::rapl(&t.hw.psys), mono() - t0);
    let mut s = Sys::default();
    t.hw.read_bat(&mut s);
    println!("puissance (2 s) · batterie {:.2} W ({}) · mode {}", s.pw, s.status, mode_short(&hw::mode()));
    match (rapl_delta(p1, p0, t.hw.rmax_pkg), rapl_delta(s1, s0, t.hw.rmax_psys)) {
        (None, None) => println!("  RAPL illisible (règle udev ?)"),
        (jp, js) => {
            if let Some(j) = jp {
                println!("  pkg  = {:.2} W", j / dt);
            }
            if let Some(j) = js {
                println!("  psys = {:.2} W", j / dt);
            }
        }
    }
    0
}

fn usage() {
    println!(
        "battrack {} — traqueur de conso ultra léger (batterie + secteur)
  battrack report   rapport court (à coller à Claude)
  battrack copy     rapport → presse-papier
  battrack now      dernière minute
  battrack reset    remet les compteurs à zéro
  battrack bench    coût d'un échantillon + puissance instantanée
  battrack daemon [-i SEC]   (lancé par systemd --user)",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    // `battrack report | head` doit s'arrêter en silence, pas paniquer
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    let code = match cmd {
        c if c.starts_with("chrome-extension://") => host::run(),
        "daemon" => {
            let interval = match (args.get(2).map(|s| s.as_str()), args.get(3)) {
                (Some("-i"), Some(v)) => v.parse().ok().filter(|&n: &u64| n > 0).unwrap_or(60),
                _ => 60,
            };
            daemon(interval)
        }
        "report" => {
            print!("{}", report::report());
            0
        }
        "copy" => copy(),
        "now" => match std::fs::read_to_string(Paths::new().now) {
            Ok(s) => {
                print!("{s}");
                0
            }
            Err(_) => {
                println!("pas de données (démon lancé ? systemctl --user status battrack)");
                1
            }
        },
        "reset" => {
            let paths = Paths::new();
            match report::daemon_pid(&paths) {
                Some(pid) => {
                    unsafe { libc::kill(pid, libc::SIGUSR2) };
                    println!("compteurs remis à zéro");
                }
                None => {
                    let _ = std::fs::remove_file(&paths.state);
                    println!("état effacé (démon arrêté)");
                }
            }
            0
        }
        "bench" => bench(),
        _ => {
            usage();
            0
        }
    };
    std::process::exit(code);
}
