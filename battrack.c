// battrack — traqueur de conso ultra léger (Linux, sans root) : batterie ET secteur
//
// Un réveil par minute (SCHED_IDLE, nice 19, timer slack 2 s). Lit /proc et /sys,
// cumule par appli et par contexte (0 = batterie, 1 = secteur), écrit l'état rarement.
// Puissance : batterie (energy_now/power_now) + RAPL package et psys si lisibles.
//
//   battrack daemon [-i SEC]   démon (lancé par systemd --user)
//   battrack report            rapport court à coller à Claude
//   battrack copy              pareil + copie dans le presse-papier
//   battrack now               dernière minute
//   battrack reset             remet les compteurs à zéro
//   battrack bench             coût d'un échantillon + puissance instantanée
//   battrack chrome-extension://…   hôte natif de l'extension Helium
//
// Build : cc -O2 -Wall -Wextra -Wno-format-truncation -o battrack battrack.c

#define _GNU_SOURCE
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

#define VERSION "1.1"
#define MAXAPP 1024
#define HSIZE 2048
#define MAXBR 256
#define RING 60
#define MAXDRM 128
#define MAXCST 10
#define MAXSESS 8
#define MAXMODE 8
#define NHIST 600 /* cases de 0,1 W → 0..60 W */
#define PF_KTHREAD 0x00200000

typedef unsigned long long u64;
typedef uint32_t u32;
typedef uint16_t u16;
typedef uint8_t u8;

static long HZ = 100;
static int INTERVAL = 60;
static long PAGEKB = 4;
static int NOWRITE; /* bench : n'écrit rien */

/* ================= petits outils ================= */

static ssize_t rd(const char *p, char *b, size_t n) {
  int fd = open(p, O_RDONLY | O_CLOEXEC);
  if (fd < 0) return -1;
  ssize_t k = read(fd, b, n - 1);
  close(fd);
  if (k < 0) return -1;
  b[k] = 0;
  return k;
}

static ssize_t rdat(int dfd, const char *p, char *b, size_t n) {
  int fd = openat(dfd, p, O_RDONLY | O_CLOEXEC);
  if (fd < 0) return -1;
  ssize_t k = read(fd, b, n - 1);
  close(fd);
  if (k < 0) return -1;
  b[k] = 0;
  return k;
}

static double rdnum(const char *p, double def) {
  char b[64];
  if (rd(p, b, sizeof b) <= 0) return def;
  char *e;
  double v = strtod(b, &e);
  return e == b ? def : v;
}

static int rdline(const char *p, char *o, size_t n) {
  ssize_t k = rd(p, o, n);
  if (k <= 0) { o[0] = 0; return 0; }
  while (k > 0 && (o[k - 1] == '\n' || o[k - 1] == ' ')) o[--k] = 0;
  return 1;
}

static double tsec(clockid_t c) {
  struct timespec t;
  clock_gettime(c, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}

static void mkdir_p(const char *path) {
  char t[512];
  snprintf(t, sizeof t, "%s", path);
  for (char *p = t + 1; *p; p++)
    if (*p == '/') { *p = 0; mkdir(t, 0755); *p = '/'; }
  mkdir(t, 0755);
}

static int exists(const char *p) { return access(p, F_OK) == 0; }

static int split_tabs(char *l, char **v, int max) {
  int n = 0;
  v[n++] = l;
  for (char *c = l; *c && n < max; c++)
    if (*c == '\t') { *c = 0; v[n++] = c + 1; }
  return n;
}

/* ================= chemins ================= */

static char D_STATE[400], D_RUN[400], F_STATE[460], F_PID[460], F_BR[460], F_NOW[460];

static void init_paths(void) {
  const char *home = getenv("HOME"), *xs = getenv("XDG_STATE_HOME"), *xr = getenv("XDG_RUNTIME_DIR");
  if (!home) home = "/tmp";
  if (xs && *xs) snprintf(D_STATE, sizeof D_STATE, "%s/battrack", xs);
  else snprintf(D_STATE, sizeof D_STATE, "%s/.local/state/battrack", home);
  if (xr && *xr) snprintf(D_RUN, sizeof D_RUN, "%s/battrack", xr);
  else snprintf(D_RUN, sizeof D_RUN, "/tmp/battrack-%u", getuid());
  snprintf(F_STATE, sizeof F_STATE, "%s/state.tsv", D_STATE);
  snprintf(F_PID, sizeof F_PID, "%s/daemon.pid", D_RUN);
  snprintf(F_BR, sizeof F_BR, "%s/browser.tsv", D_RUN);
  snprintf(F_NOW, sizeof F_NOW, "%s/now.txt", D_RUN);
}

/* ================= matériel ================= */

static char P_BAT[256], P_BL[256], P_DPMS[300], P_GT[2][300], GTN[2][8], P_PKG[128], P_PSYS[128], P_KBD[300],
    P_WUP[300];
static double RMAX_PKG, RMAX_PSYS, KBD_MAX;
static int NGT, NCST, NCPU;
static char CSTN[MAXCST][16];

static void discover(void) {
  DIR *d = opendir("/sys/class/power_supply");
  struct dirent *e;
  char p[512], b[64];
  if (d) {
    while ((e = readdir(d))) {
      if (e->d_name[0] == '.') continue;
      snprintf(p, sizeof p, "/sys/class/power_supply/%s/type", e->d_name);
      if (rdline(p, b, sizeof b) && !strcmp(b, "Battery")) {
        snprintf(P_BAT, sizeof P_BAT, "/sys/class/power_supply/%s", e->d_name);
        break;
      }
    }
    closedir(d);
  }
  if ((d = opendir("/sys/class/backlight"))) {
    while ((e = readdir(d)))
      if (e->d_name[0] != '.') { snprintf(P_BL, sizeof P_BL, "/sys/class/backlight/%s", e->d_name); break; }
    closedir(d);
  }
  if ((d = opendir("/sys/class/drm"))) {
    while ((e = readdir(d)))
      if (strstr(e->d_name, "-eDP-")) { snprintf(P_DPMS, sizeof P_DPMS, "/sys/class/drm/%s/dpms", e->d_name); break; }
    closedir(d);
  }
  for (int c = 0; c < 4 && NGT < 2; c++)
    for (int g = 0; g < 4 && NGT < 2; g++) {
      snprintf(p, sizeof p, "/sys/class/drm/card%d/device/tile0/gt%d/gtidle/idle_residency_ms", c, g);
      if (exists(p)) {
        snprintf(P_GT[NGT], sizeof P_GT[0], "%s", p);
        snprintf(GTN[NGT], sizeof GTN[0], g == 0 ? "rc" : "mc");
        NGT++;
        continue;
      }
      snprintf(p, sizeof p, "/sys/class/drm/card%d/gt/gt%d/rc6_residency_ms", c, g);
      if (exists(p)) {
        snprintf(P_GT[NGT], sizeof P_GT[0], "%s", p);
        snprintf(GTN[NGT], sizeof GTN[0], "gt%d", g);
        NGT++;
      }
    }
  for (NCST = 0; NCST < MAXCST; NCST++) {
    snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu0/cpuidle/state%d/name", NCST);
    if (!rdline(p, CSTN[NCST], sizeof CSTN[0])) break;
  }
  NCPU = (int)sysconf(_SC_NPROCESSORS_CONF);
  if (NCPU < 1) NCPU = 1;
  /* RAPL : package (CPU+GPU+RAM) et psys (plateforme entière) */
  for (int i = 0; i < 4; i++) {
    snprintf(p, sizeof p, "/sys/class/powercap/intel-rapl:%d/name", i);
    if (!rdline(p, b, sizeof b)) continue;
    int pk = !strcmp(b, "package-0"), ps = !strcmp(b, "psys");
    if (!pk && !ps) continue;
    snprintf(pk ? P_PKG : P_PSYS, 128, "/sys/class/powercap/intel-rapl:%d/energy_uj", i);
    snprintf(p, sizeof p, "/sys/class/powercap/intel-rapl:%d/max_energy_range_uj", i);
    if (pk) RMAX_PKG = rdnum(p, 0); else RMAX_PSYS = rdnum(p, 0);
  }
  if ((d = opendir("/sys/class/leds"))) {
    while ((e = readdir(d)))
      if (strstr(e->d_name, "kbd_backlight")) {
        snprintf(P_KBD, sizeof P_KBD, "/sys/class/leds/%s", e->d_name);
        snprintf(p, sizeof p, "%s/max_brightness", P_KBD);
        KBD_MAX = rdnum(p, 0);
        break;
      }
    closedir(d);
  }
  if ((d = opendir("/sys/class/net"))) {
    while ((e = readdir(d))) {
      if (e->d_name[0] == '.') continue;
      snprintf(p, sizeof p, "/sys/class/net/%s/wireless", e->d_name);
      if (exists(p)) { snprintf(P_WUP, sizeof P_WUP, "/sys/class/net/%s/operstate", e->d_name); break; }
    }
    closedir(d);
  }
}

/* ================= navigateurs suivis ================= */

static char BROW[8][16];
static int NBROW;

static void init_browsers(void) {
  const char *s = getenv("BATTRACK_BROWSERS");
  if (!s || !*s) s = "helium";
  char t[160];
  snprintf(t, sizeof t, "%s", s);
  char *sv = NULL;
  for (char *tok = strtok_r(t, ",", &sv); tok && NBROW < 8; tok = strtok_r(NULL, ",", &sv))
    snprintf(BROW[NBROW++], 16, "%s", tok);
}

static int is_browser(const char *comm) {
  for (int i = 0; i < NBROW; i++)
    if (!strcmp(comm, BROW[i])) return 1;
  return 0;
}

static int is_interp(const char *c) {
  static const char *L[] = {"python", "node", "java", "perl", "ruby", "bash", "sh", "dash", "lua", "php", "deno", "bun", 0};
  for (int i = 0; L[i]; i++) {
    size_t n = strlen(L[i]);
    if (!strncmp(c, L[i], n) && (c[n] == 0 || isdigit((u8)c[n]) || c[n] == '.')) return 1;
  }
  return 0;
}

/* ================= applis ================= */

typedef struct {
  char name[40];
  u64 cpu[2], wk[2];   /* ticks CPU, tranches de temps (≈ réveils) — [0] batterie, [1] secteur */
  double gpu[2], vid[2]; /* secondes moteur GPU 3D / vidéo */
  u64 l_tk;            /* dernière période (non sauvé) */
  double l_gpu;
} App;

static App A[MAXAPP];
static int NA;
static int16_t H[HSIZE];

static u32 fnv(const char *s) {
  u32 h = 2166136261u;
  while (*s) { h ^= (u8)*s++; h *= 16777619u; }
  return h;
}

static void apps_init(void) { NA = 0; memset(H, 0xff, sizeof H); }

static int app_get(const char *name) {
  u32 h = fnv(name) & (HSIZE - 1);
  for (;;) {
    int i = H[h];
    if (i < 0) break;
    if (!strcmp(A[i].name, name)) return i;
    h = (h + 1) & (HSIZE - 1);
  }
  if (NA >= MAXAPP - 1 && strcmp(name, "[divers]")) return app_get("[divers]");
  if (NA >= MAXAPP) return MAXAPP - 1;
  int i = NA++;
  memset(&A[i], 0, sizeof A[i]);
  snprintf(A[i].name, sizeof A[i].name, "%s", name);
  for (char *c = A[i].name; *c; c++)
    if (*c == '\t' || *c == '\n' || *c == '\r') *c = '_';
  H[h] = (int16_t)i;
  return i;
}

/* ================= processus ================= */

typedef struct {
  int pid;
  int16_t app, br;
  u8 kth, nofd, ndrm, pad;
  u64 start, tk, ws;
  char comm[16];
  int dfd[4];
  u32 dcid[4];
} Proc;

static Proc *PA, *PB;
static int NPA, NPB, CAPA, CAPB;

/* historique par processus du navigateur (pour l'extension) */
typedef struct {
  int pid, cid;
  u64 start;
  u32 gen;
  u16 n;
  char ty[12];
  u16 r[RING];
} Br;
static Br B[MAXBR];
static double DTS[RING];
static u64 GS; /* échantillons complets */
static u32 SC; /* passages */

/* clients GPU (DRM) */
typedef struct {
  u32 cid, gen;
  u8 used;
  double last;
  u64 c[5], t[5];
} Drm;
static Drm D[MAXDRM];

static int br_alloc(int pid, u64 start, int cid, const char *ty) {
  for (int i = 0; i < MAXBR; i++)
    if (!B[i].pid) {
      memset(&B[i], 0, sizeof B[i]);
      B[i].pid = pid; B[i].start = start; B[i].cid = cid; B[i].gen = SC;
      snprintf(B[i].ty, sizeof B[i].ty, "%s", ty[0] ? ty : "?");
      return i;
    }
  return -1;
}

static void make_key(Proc *p, char *out, size_t on, int *cid, char *ty, size_t tyn) {
  *cid = 0;
  ty[0] = 0;
  if (p->kth) {
    char t[16];
    if (!strncmp(p->comm, "irq/", 4)) {
      const char *m = strchr(p->comm, '-');
      snprintf(out, on, "[k]irq:%s", m ? m + 1 : p->comm + 4);
      return;
    }
    size_t i = 0;
    while (p->comm[i] && p->comm[i] != '/' && i < sizeof t - 1) { t[i] = p->comm[i]; i++; }
    t[i] = 0;
    while (i > 1 && (isdigit((u8)t[i - 1]) || t[i - 1] == ':' || t[i - 1] == '-')) t[--i] = 0;
    snprintf(out, on, "[k]%s", t);
    return;
  }
  char cl[4096], path[64];
  /* conteneur Android (Waydroid) / LXC : on regroupe sous un préfixe */
  const char *pre = "";
  snprintf(path, sizeof path, "/proc/%d/cgroup", p->pid);
  if (rd(path, cl, sizeof cl) > 0 && strstr(cl, "lxc")) pre = strstr(cl, "waydroid") ? "[android]" : "[lxc]";
  snprintf(path, sizeof path, "/proc/%d/cmdline", p->pid);
  ssize_t n = rd(path, cl, sizeof cl);
  const char *av[64];
  int na = 0;
  for (char *s = cl; n > 0 && s < cl + n && na < 64; s += strlen(s) + 1) av[na++] = s;
  /* Chromium/Electron réécrivent leur titre : un seul argument séparé par des espaces */
  if (na == 1 && strchr(cl, ' ')) {
    na = 0;
    char *sv = NULL;
    for (char *t = strtok_r(cl, " ", &sv); t && na < 64; t = strtok_r(NULL, " ", &sv)) av[na++] = t;
  }
  if (*pre) {
    snprintf(out, on, "%s%s", pre, p->comm);
    return;
  }
  const char *type = NULL, *sub = NULL;
  int ext = 0;
  for (int i = 1; i < na; i++) {
    if (!strncmp(av[i], "--type=", 7)) type = av[i] + 7;
    else if (!strncmp(av[i], "--utility-sub-type=", 19)) sub = av[i] + 19;
    else if (!strcmp(av[i], "--extension-process")) ext = 1;
    else if (!strncmp(av[i], "--renderer-client-id=", 21)) *cid = atoi(av[i] + 21);
  }
  if (type) { /* chromium / electron */
    char t[24];
    if (!strcmp(type, "renderer")) snprintf(t, sizeof t, "%s", ext ? "ext" : "renderer");
    else if (!strcmp(type, "gpu-process")) snprintf(t, sizeof t, "gpu");
    else if (!strcmp(type, "utility") && sub) {
      size_t i = 0;
      while (sub[i] && sub[i] != '.' && i < sizeof t - 1) { t[i] = sub[i]; i++; }
      t[i] = 0;
    } else snprintf(t, sizeof t, "%s", type);
    snprintf(out, on, "%s:%s", p->comm, t);
    snprintf(ty, tyn, "%s", t);
    return;
  }
  if (is_browser(p->comm)) {
    snprintf(out, on, "%s", p->comm);
    snprintf(ty, tyn, "main");
    return;
  }
  if (is_interp(p->comm) && na > 1) {
    const char *a = NULL;
    for (int i = 1; i < na; i++) {
      if (av[i][0] == '-') {
        if (!strcmp(av[i], "-m") && i + 1 < na) { a = av[i + 1]; break; }
        if (!strcmp(av[i], "-c")) break;
        continue;
      }
      a = av[i];
      break;
    }
    if (a && *a) {
      const char *b = strrchr(a, '/');
      snprintf(out, on, "%s:%s", p->comm, b ? b + 1 : a);
      return;
    }
  }
  snprintf(out, on, "%s", p->comm);
}

static u64 third(const char *b) {
  char *e;
  strtoull(b, &e, 10);
  strtoull(e, &e, 10);
  return strtoull(e, NULL, 10);
}

/* somme des "timeslices" (3e champ de schedstat) de tous les threads ≈ réveils */
static u64 wake_sum(int pid, int kth) {
  char p[64], b[128];
  u64 s = 0;
  if (kth) {
    snprintf(p, sizeof p, "/proc/%d/schedstat", pid);
    return rd(p, b, sizeof b) > 0 ? third(b) : 0;
  }
  snprintf(p, sizeof p, "/proc/%d/task", pid);
  int dfd = open(p, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (dfd < 0) return 0;
  DIR *d = fdopendir(dfd);
  if (!d) { close(dfd); return 0; }
  struct dirent *e;
  while ((e = readdir(d))) {
    if (!isdigit((u8)e->d_name[0])) continue;
    char q[48];
    snprintf(q, sizeof q, "%s/schedstat", e->d_name);
    if (rdat(dfd, q, b, sizeof b) > 0) s += third(b);
  }
  closedir(d);
  return s;
}

/* ---- GPU par client DRM (xe : cycles, i915/amdgpu : ns) ---- */

static int eng_idx(const char *e, size_t n) {
#define EQ(s) (n == sizeof(s) - 1 && !memcmp(e, s, n))
  if (EQ("rcs") || EQ("render") || EQ("gfx")) return 0;
  if (EQ("ccs") || EQ("compute")) return 1;
  if (EQ("bcs") || EQ("copy") || EQ("dma")) return 2;
  if (EQ("vcs") || EQ("video") || EQ("dec") || EQ("enc") || EQ("jpeg")) return 3;
  if (EQ("vecs") || EQ("video-enhance")) return 4;
  return -1;
#undef EQ
}

typedef struct { u32 cid; int ok, xe; u64 c[5], t[5]; } FdI;

static int parse_fdinfo(int pid, int fd, FdI *f) {
  char p[64], b[4096];
  snprintf(p, sizeof p, "/proc/%d/fdinfo/%d", pid, fd);
  memset(f, 0, sizeof *f);
  if (rd(p, b, sizeof b) <= 0) return 0;
  for (char *l = b; l && *l;) {
    char *nl = strchr(l, '\n');
    if (nl) *nl = 0;
    if (!strncmp(l, "drm-client-id:", 14)) { f->cid = (u32)strtoul(l + 14, NULL, 10); f->ok = 1; }
    else if (!strncmp(l, "drm-total-cycles-", 17) || !strncmp(l, "drm-cycles-", 11) || !strncmp(l, "drm-engine-", 11)) {
      int tot = l[4] == 't', cyc = tot || l[4] == 'c';
      const char *nm = l + (tot ? 17 : 11);
      const char *c = strchr(nm, ':');
      if (c) {
        int e = eng_idx(nm, (size_t)(c - nm));
        if (e >= 0) {
          u64 v = strtoull(c + 1, NULL, 10);
          if (tot) f->t[e] += v; else f->c[e] += v;
          if (cyc) f->xe = 1;
        }
      }
    }
    l = nl ? nl + 1 : NULL;
  }
  return f->ok;
}

static void fd_scan(Proc *p) {
  char path[64];
  snprintf(path, sizeof path, "/proc/%d/fd", p->pid);
  int dfd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (dfd < 0) { if (errno == EACCES || errno == EPERM) p->nofd = 1; return; }
  DIR *d = fdopendir(dfd);
  if (!d) { close(dfd); return; }
  p->ndrm = 0;
  struct dirent *e;
  while ((e = readdir(d))) {
    if (!isdigit((u8)e->d_name[0])) continue;
    char l[32];
    ssize_t n = readlinkat(dfd, e->d_name, l, sizeof l - 1);
    if (n < 9 || memcmp(l, "/dev/dri/", 9)) continue;
    FdI f;
    int fd = atoi(e->d_name);
    if (!parse_fdinfo(p->pid, fd, &f)) continue;
    int dup = 0;
    for (int i = 0; i < p->ndrm; i++) dup |= p->dcid[i] == f.cid;
    if (dup || p->ndrm >= 4) continue;
    p->dfd[p->ndrm] = fd;
    p->dcid[p->ndrm++] = f.cid;
  }
  closedir(d);
}

static Drm *drm_get(u32 cid) {
  Drm *fr = NULL;
  for (int i = 0; i < MAXDRM; i++) {
    if (D[i].used && D[i].cid == cid) return &D[i];
    if (!D[i].used && !fr) fr = &D[i];
  }
  if (fr) { memset(fr, 0, sizeof *fr); fr->cid = cid; fr->used = 1; }
  return fr;
}

static void drm_proc(Proc *p, double now, int ci) {
  App *a = &A[p->app];
  for (int i = 0; i < p->ndrm;) {
    FdI f;
    if (!parse_fdinfo(p->pid, p->dfd[i], &f) || f.cid != p->dcid[i]) {
      p->ndrm--;
      p->dfd[i] = p->dfd[p->ndrm];
      p->dcid[i] = p->dcid[p->ndrm];
      continue;
    }
    Drm *d = drm_get(f.cid);
    if (d) {
      if (d->used == 2) {
        double el = now - d->last, g3 = 0, gv = 0;
        for (int e = 0; e < 5; e++) {
          u64 dc = f.c[e] >= d->c[e] ? f.c[e] - d->c[e] : 0;
          double busy = 0;
          if (f.xe) {
            u64 dt = f.t[e] > d->t[e] ? f.t[e] - d->t[e] : 0;
            busy = dt ? (double)dc / (double)dt * el : 0;
            if (busy > el) busy = el;
          } else busy = dc / 1e9;
          if (e < 3) g3 += busy; else gv += busy;
        }
        a->l_gpu += g3 + gv;
        if (ci >= 0) { a->gpu[ci] += g3; a->vid[ci] += gv; }
      }
      memcpy(d->c, f.c, sizeof f.c);
      memcpy(d->t, f.t, sizeof f.t);
      d->last = now; d->gen = SC; d->used = 2;
    }
    i++;
  }
}

static Proc *old_find(int pid) {
  int lo = 0, hi = NPA - 1;
  while (lo <= hi) {
    int m = (lo + hi) / 2;
    if (PA[m].pid == pid) return &PA[m];
    if (PA[m].pid < pid) lo = m + 1; else hi = m - 1;
  }
  return NULL;
}

static int cmp_pid(const void *a, const void *b) { return ((const Proc *)a)->pid - ((const Proc *)b)->pid; }

/* un passage sur /proc : renvoie les ticks CPU attribués. ci = contexte (-1 = on ne compte pas) */
static u64 scan(double now, double prev_boot, int first, int ci, int fdall, int gi) {
  DIR *d = opendir("/proc");
  if (!d) return 0;
  SC++;
  for (int i = 0; i < NA; i++) { A[i].l_tk = 0; A[i].l_gpu = 0; }
  NPB = 0;
  int sorted = 1, lastpid = 0;
  u64 attr = 0;
  struct dirent *e;
  char path[64], b[1024];
  while ((e = readdir(d))) {
    if (e->d_name[0] < '1' || e->d_name[0] > '9') continue;
    int pid = atoi(e->d_name);
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    if (rd(path, b, sizeof b) <= 0) continue;
    char *lp = strchr(b, '('), *rp = strrchr(b, ')');
    if (!lp || !rp || rp < lp || !rp[1]) continue;
    char comm[16];
    size_t cl = (size_t)(rp - lp - 1);
    if (cl > 15) cl = 15;
    memcpy(comm, lp + 1, cl);
    comm[cl] = 0;
    u64 f[25] = {0};
    char *q = rp + 3; /* après ") S" */
    for (int k = 4; k <= 24 && *q; k++) f[k] = strtoull(q, &q, 10);
    u64 start = f[22], tk = f[14] + f[15];

    if (NPB >= CAPB) {
      CAPB = CAPB ? CAPB * 2 : 1024;
      PB = realloc(PB, sizeof *PB * (size_t)CAPB);
      if (!PB) exit(1);
    }
    Proc *o = old_find(pid);
    if (o && o->start != start) o = NULL;
    Proc *p = &PB[NPB++];
    if (o) *p = *o;
    else {
      memset(p, 0, sizeof *p);
      p->pid = pid; p->start = start; p->app = -1; p->br = -1;
      p->kth = (f[9] & PF_KTHREAD) != 0;
    }
    if (pid < lastpid) sorted = 0;
    lastpid = pid;
    int born = !o && !first && (double)start / HZ > prev_boot - 0.5;

    if (p->app < 0 || strcmp(p->comm, comm)) { /* nouveau ou exec() */
      memcpy(p->comm, comm, 16);
      char key[64], ty[12];
      int cid;
      make_key(p, key, sizeof key, &cid, ty, sizeof ty);
      p->app = (int16_t)app_get(key);
      if (p->br >= 0) { B[p->br].pid = 0; p->br = -1; }
      if (is_browser(comm)) p->br = (int16_t)br_alloc(pid, start, cid, ty);
    }
    u64 dd = 0;
    if (o) dd = tk >= o->tk ? tk - o->tk : 0;
    else if (born) dd = tk;
    p->tk = tk;
    App *a = &A[p->app];
    if (dd) { a->l_tk += dd; if (ci >= 0) a->cpu[ci] += dd; attr += dd; }
    if (dd || born) {
      u64 w = wake_sum(pid, p->kth);
      if (w) {
        u64 dw = 0;
        if (p->ws && w >= p->ws) dw = w - p->ws;
        else if (born) dw = w;
        p->ws = w;
        if (ci >= 0) a->wk[ci] += dw;
      }
    }
    if (!p->kth && !p->nofd && (!o || (fdall && dd))) fd_scan(p);
    if (p->ndrm && (dd || !o)) drm_proc(p, now, ci);
    if (p->br >= 0) {
      Br *x = &B[p->br];
      x->gen = SC;
      if (o || born) {
        x->r[gi] = dd > 65535 ? 65535 : (u16)dd;
        if (x->n < RING) x->n++;
      }
    }
  }
  closedir(d);
  if (!sorted) qsort(PB, (size_t)NPB, sizeof *PB, cmp_pid);
  for (int i = 0; i < MAXBR; i++)
    if (B[i].pid && B[i].gen != SC) B[i].pid = 0;
  for (int i = 0; i < MAXDRM; i++)
    if (D[i].used && SC - D[i].gen > 30) D[i].used = 0;
  Proc *t = PA; PA = PB; PB = t;
  int c = CAPA; CAPA = CAPB; CAPB = c;
  NPA = NPB;
  return attr;
}

/* ================= système ================= */

typedef struct {
  double mono, boot;
  time_t wall;
  int on_bat, pct, ncpu_ok, scr_off, wifi, wifi_up, bt; /* radios : -1 = inconnu */
  double pw, ewh, lpi, bright, kbd, pkg, psys;          /* pkg/psys : µJ cumulés, -1 si illisible */
  char st[24], mode[48];
  u64 busy, tot, ctxt, intr;
  double cst[MAXCST], gt[2];
} Sys;

static double bnum(const char *name, double def) {
  char p[320];
  snprintf(p, sizeof p, "%s/%s", P_BAT, name);
  return rdnum(p, def);
}

static void read_bat(Sys *s) {
  s->ewh = -1; s->pct = -1;
  if (!*P_BAT) return;
  char p[320];
  snprintf(p, sizeof p, "%s/status", P_BAT);
  rdline(p, s->st, sizeof s->st);
  s->on_bat = !strcmp(s->st, "Discharging");
  double pw = bnum("power_now", -1);
  if (pw <= 0) {
    double c = bnum("current_now", -1), v = bnum("voltage_now", -1);
    pw = (c > 0 && v > 0) ? c * v / 1e6 : 0;
  }
  s->pw = pw / 1e6;
  double e = bnum("energy_now", -1);
  if (e < 0) {
    double c = bnum("charge_now", -1), v = bnum("voltage_min_design", -1);
    e = (c >= 0 && v > 0) ? c * v / 1e6 : -1;
  }
  s->ewh = e >= 0 ? e / 1e6 : -1;
  s->pct = (int)bnum("capacity", -1);
}

static void read_radios(int *wifi, int *bt) {
  *wifi = *bt = -1;
  DIR *d = opendir("/sys/class/rfkill");
  if (!d) return;
  struct dirent *e;
  char p[300], t[32];
  while ((e = readdir(d))) {
    if (strncmp(e->d_name, "rfkill", 6)) continue;
    snprintf(p, sizeof p, "/sys/class/rfkill/%s/type", e->d_name);
    if (!rdline(p, t, sizeof t)) continue;
    int *dst = !strcmp(t, "wlan") ? wifi : !strcmp(t, "bluetooth") ? bt : NULL;
    if (!dst) continue;
    snprintf(p, sizeof p, "/sys/class/rfkill/%s/soft", e->d_name);
    int so = (int)rdnum(p, 0);
    snprintf(p, sizeof p, "/sys/class/rfkill/%s/hard", e->d_name);
    int ha = (int)rdnum(p, 0);
    int on = !so && !ha;
    *dst = *dst < 0 ? on : (*dst && on); /* éteint si un seul interrupteur bloque */
  }
  closedir(d);
}

static void read_mode(char *o, size_t n) {
  char pp[24], ep[24];
  if (!rdline("/sys/firmware/acpi/platform_profile", pp, sizeof pp)) snprintf(pp, sizeof pp, "?");
  if (!rdline("/sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference", ep, sizeof ep))
    snprintf(ep, sizeof ep, "?");
  snprintf(o, n, "%s/%s", pp, ep);
}

static void read_sys(Sys *s) {
  static char b[65536];
  memset(s, 0, sizeof *s);
  s->mono = tsec(CLOCK_MONOTONIC);
  s->boot = tsec(CLOCK_BOOTTIME);
  s->wall = time(NULL);
  read_bat(s);
  if (rd("/proc/stat", b, sizeof b) > 0) {
    u64 v[8] = {0};
    char *q = b + 4;
    for (int i = 0; i < 8; i++) v[i] = strtoull(q, &q, 10);
    s->busy = v[0] + v[1] + v[2] + v[5] + v[6] + v[7];
    s->tot = s->busy + v[3] + v[4];
    char *c = strstr(b, "\nctxt ");
    if (c) s->ctxt = strtoull(c + 6, NULL, 10);
    c = strstr(b, "\nintr ");
    if (c) s->intr = strtoull(c + 6, NULL, 10);
  }
  char p[400];
  for (int c = 0; c < NCPU; c++) {
    int ok = 0;
    for (int j = 0; j < NCST; j++) {
      snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu%d/cpuidle/state%d/time", c, j);
      double v = rdnum(p, -1);
      if (v >= 0) { s->cst[j] += v; ok = 1; }
    }
    s->ncpu_ok += ok;
  }
  s->lpi = rdnum("/sys/devices/system/cpu/cpuidle/low_power_idle_system_residency_us", -1);
  for (int i = 0; i < NGT; i++) s->gt[i] = rdnum(P_GT[i], 0);
  s->bright = -1;
  if (*P_BL) {
    snprintf(p, sizeof p, "%s/actual_brightness", P_BL);
    double v = rdnum(p, -1);
    snprintf(p, sizeof p, "%s/max_brightness", P_BL);
    double m = rdnum(p, -1);
    if (v >= 0 && m > 0) s->bright = v / m * 100;
  }
  if (*P_DPMS) {
    char t[16];
    rdline(P_DPMS, t, sizeof t);
    s->scr_off = !strcmp(t, "Off");
  }
  s->pkg = *P_PKG ? rdnum(P_PKG, -1) : -1;
  s->psys = *P_PSYS ? rdnum(P_PSYS, -1) : -1;
  s->kbd = -1;
  if (*P_KBD && KBD_MAX > 0) {
    snprintf(p, sizeof p, "%s/brightness", P_KBD);
    double v = rdnum(p, -1);
    if (v >= 0) s->kbd = v / KBD_MAX * 100;
  }
  read_radios(&s->wifi, &s->bt);
  s->wifi_up = -1;
  if (*P_WUP) {
    char t[16];
    if (rdline(P_WUP, t, sizeof t)) s->wifi_up = !strcmp(t, "up");
  }
  read_mode(s->mode, sizeof s->mode);
}

/* énergie RAPL entre deux lectures (J), -1 si illisible ; gère le rebouclage du compteur */
static double rapl_d(double cur, double prev, double max) {
  if (cur < 0 || prev < 0) return -1;
  double d = cur - prev;
  if (d < 0) d += max;
  return d < 0 ? -1 : d / 1e6;
}

/* ================= cumuls par contexte ================= */

typedef struct { long start; double act_s, act_wh, susp_s; int p0, p1; } Sess;

typedef struct {
  double t, chg_t;                          /* s actifs, s en charge */
  double wh, j_bat, t_bat;                  /* batterie */
  double j_pkg, t_pkg, j_psys, t_psys;      /* RAPL */
  u32 h_bat[NHIST], h_pkg[NHIST], h_psys[NHIST];
  u64 busy, tot, attr, ctxt, intr;
  double cst[MAXCST], cst_den, lpi;
  double gt[2], gt_den;
  double br_sum, br_t, scr_off, kbd_sum, kbd_t, kbd_on;
  double wifi_on, wifi_up, bt_on, rad_t;
  double m_t[MAXMODE], m_jsys[MAXMODE], m_tsys[MAXMODE], m_jpkg[MAXMODE], m_tpkg[MAXMODE];
} Ctx;

typedef struct {
  long since;
  Ctx c[2];
  char cstn[MAXCST][16];
  int ncst;
  char gtn[2][8];
  int ngt;
  char modes[MAXMODE][48];
  int nmode;
  double susp_s, susp_wh;
  Sess sess[MAXSESS];
  int nsess, sess_open;
} Acc;

static Acc G;
static Sys PS;
static int HAVE_PS;
static double PWR[10];
static int NPWR, IPWR;
static int NEED_SAVE;
static double LW_PKG = -1, LW_PSYS = -1; /* W sur la dernière période */

static int mode_get(const char *m) {
  for (int i = 0; i < G.nmode; i++)
    if (!strcmp(G.modes[i], m)) return i;
  if (G.nmode < MAXMODE) {
    snprintf(G.modes[G.nmode], sizeof G.modes[0], "%s", m);
    return G.nmode++;
  }
  return MAXMODE - 1;
}

static void hist_add(u32 *h, double w) {
  if (w < 0) return;
  int i = (int)(w * 10);
  h[i < NHIST ? i : NHIST - 1]++;
}

static void put_hist(FILE *f, int ci, const char *k, const u32 *h) {
  fprintf(f, "c%d\t%s\t", ci, k);
  int first = 1;
  for (int i = 0; i < NHIST; i++)
    if (h[i]) { fprintf(f, "%s%d:%u", first ? "" : " ", i, h[i]); first = 0; }
  fprintf(f, "\n");
}

static void get_hist(char *s, u32 *h) {
  char *sv = NULL;
  for (char *t = strtok_r(s, " ", &sv); t; t = strtok_r(NULL, " ", &sv)) {
    int i;
    unsigned c;
    if (sscanf(t, "%d:%u", &i, &c) == 2 && i >= 0 && i < NHIST) h[i] = c;
  }
}

static void save_state(void) {
  if (NOWRITE) return;
  char tmp[480];
  snprintf(tmp, sizeof tmp, "%s.tmp", F_STATE);
  FILE *f = fopen(tmp, "w");
  if (!f) return;
  fprintf(f, "battrack-state\t2\n");
  fprintf(f, "since\t%ld\n", G.since);
  for (int i = 0; i < G.nmode; i++) fprintf(f, "mode\t%d\t%s\n", i, G.modes[i]);
  for (int i = 0; i < G.ncst; i++) fprintf(f, "cstn\t%d\t%s\n", i, G.cstn[i]);
  for (int i = 0; i < G.ngt; i++) fprintf(f, "gtn\t%d\t%s\n", i, G.gtn[i]);
  fprintf(f, "susp\t%.1f\t%.5f\n", G.susp_s, G.susp_wh);
  for (int i = 0; i < G.nsess; i++)
    fprintf(f, "sess\t%ld\t%.0f\t%.5f\t%.0f\t%d\t%d\n", G.sess[i].start, G.sess[i].act_s, G.sess[i].act_wh,
            G.sess[i].susp_s, G.sess[i].p0, G.sess[i].p1);
  for (int ci = 0; ci < 2; ci++) {
    Ctx *c = &G.c[ci];
    fprintf(f, "c%d\tt\t%.1f\t%.1f\n", ci, c->t, c->chg_t);
    fprintf(f, "c%d\te\t%.5f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\n", ci, c->wh, c->j_bat, c->t_bat, c->j_pkg, c->t_pkg,
            c->j_psys, c->t_psys);
    fprintf(f, "c%d\tcpu\t%llu\t%llu\t%llu\t%llu\t%llu\n", ci, c->busy, c->tot, c->attr, c->ctxt, c->intr);
    fprintf(f, "c%d\tcst\t%.0f", ci, c->cst_den);
    for (int i = 0; i < G.ncst; i++) fprintf(f, "\t%.0f", c->cst[i]);
    fprintf(f, "\n");
    fprintf(f, "c%d\tlpi\t%.0f\n", ci, c->lpi);
    fprintf(f, "c%d\tgt\t%.0f\t%.0f\t%.0f\n", ci, c->gt_den, c->gt[0], c->gt[1]);
    fprintf(f, "c%d\tenv\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\n", ci, c->br_sum, c->br_t,
            c->scr_off, c->kbd_sum, c->kbd_t, c->kbd_on, c->wifi_on, c->wifi_up, c->bt_on, c->rad_t);
    for (int i = 0; i < G.nmode; i++)
      if (c->m_t[i] > 0)
        fprintf(f, "c%d\tm\t%d\t%.1f\t%.1f\t%.1f\t%.1f\t%.1f\n", ci, i, c->m_t[i], c->m_jsys[i], c->m_tsys[i],
                c->m_jpkg[i], c->m_tpkg[i]);
    put_hist(f, ci, "hb", c->h_bat);
    put_hist(f, ci, "hp", c->h_pkg);
    put_hist(f, ci, "hs", c->h_psys);
  }
  for (int i = 0; i < NA; i++) {
    App *a = &A[i];
    if (!a->cpu[0] && !a->cpu[1] && a->gpu[0] + a->gpu[1] + a->vid[0] + a->vid[1] < 0.001) continue;
    fprintf(f, "app\t%s\t%llu\t%llu\t%.3f\t%.3f\t%.3f\t%.3f\t%llu\t%llu\n", a->name, a->cpu[0], a->cpu[1], a->gpu[0],
            a->gpu[1], a->vid[0], a->vid[1], a->wk[0], a->wk[1]);
  }
  if (fclose(f) == 0) rename(tmp, F_STATE);
}

/* migrate = 1 (démon) : un état d'ancien format est mis de côté */
static void load_state(int migrate) {
  FILE *f = fopen(F_STATE, "r");
  if (!f) return;
  static char l[16384];
  if (!fgets(l, sizeof l, f) || strncmp(l, "battrack-state\t2", 16)) {
    fclose(f);
    if (migrate) {
      char bak[480];
      snprintf(bak, sizeof bak, "%s.ancien", F_STATE);
      rename(F_STATE, bak);
    }
    return;
  }
  while (fgets(l, sizeof l, f)) {
    l[strcspn(l, "\n")] = 0;
    char *v[24];
    int n = split_tabs(l, v, 24);
    if (n < 2) continue;
#define D(i) strtod(v[i], NULL)
#define U(i) strtoull(v[i], NULL, 10)
    if (!strcmp(v[0], "since")) G.since = atol(v[1]);
    else if (!strcmp(v[0], "mode") && n >= 3) {
      int i = atoi(v[1]);
      if (i >= 0 && i < MAXMODE) {
        snprintf(G.modes[i], sizeof G.modes[0], "%s", v[2]);
        if (i + 1 > G.nmode) G.nmode = i + 1;
      }
    } else if (!strcmp(v[0], "cstn") && n >= 3) {
      int i = atoi(v[1]);
      if (i >= 0 && i < MAXCST) {
        snprintf(G.cstn[i], 16, "%s", v[2]);
        if (i + 1 > G.ncst) G.ncst = i + 1;
      }
    } else if (!strcmp(v[0], "gtn") && n >= 3) {
      int i = atoi(v[1]);
      if (i >= 0 && i < 2) {
        snprintf(G.gtn[i], 8, "%s", v[2]);
        if (i + 1 > G.ngt) G.ngt = i + 1;
      }
    } else if (!strcmp(v[0], "susp") && n >= 3) { G.susp_s = D(1); G.susp_wh = D(2); }
    else if (!strcmp(v[0], "sess") && n >= 7 && G.nsess < MAXSESS) {
      Sess *s = &G.sess[G.nsess++];
      s->start = atol(v[1]); s->act_s = D(2); s->act_wh = D(3); s->susp_s = D(4); s->p0 = atoi(v[5]); s->p1 = atoi(v[6]);
    } else if (v[0][0] == 'c' && (v[0][1] == '0' || v[0][1] == '1') && !v[0][2] && n >= 3) {
      Ctx *c = &G.c[v[0][1] - '0'];
      const char *k = v[1];
      if (!strcmp(k, "t")) { c->t = D(2); if (n > 3) c->chg_t = D(3); }
      else if (!strcmp(k, "e") && n >= 9) {
        c->wh = D(2); c->j_bat = D(3); c->t_bat = D(4); c->j_pkg = D(5); c->t_pkg = D(6); c->j_psys = D(7); c->t_psys = D(8);
      } else if (!strcmp(k, "cpu") && n >= 7) {
        c->busy = U(2); c->tot = U(3); c->attr = U(4); c->ctxt = U(5); c->intr = U(6);
      } else if (!strcmp(k, "cst")) {
        c->cst_den = D(2);
        for (int j = 0; j < MAXCST && 3 + j < n; j++) c->cst[j] = D(3 + j);
      } else if (!strcmp(k, "lpi")) c->lpi = D(2);
      else if (!strcmp(k, "gt") && n >= 5) { c->gt_den = D(2); c->gt[0] = D(3); c->gt[1] = D(4); }
      else if (!strcmp(k, "env") && n >= 12) {
        c->br_sum = D(2); c->br_t = D(3); c->scr_off = D(4); c->kbd_sum = D(5); c->kbd_t = D(6); c->kbd_on = D(7);
        c->wifi_on = D(8); c->wifi_up = D(9); c->bt_on = D(10); c->rad_t = D(11);
      } else if (!strcmp(k, "m") && n >= 8) {
        int i = atoi(v[2]);
        if (i >= 0 && i < MAXMODE) {
          c->m_t[i] = D(3); c->m_jsys[i] = D(4); c->m_tsys[i] = D(5); c->m_jpkg[i] = D(6); c->m_tpkg[i] = D(7);
        }
      } else if (!strcmp(k, "hb")) get_hist(v[2], c->h_bat);
      else if (!strcmp(k, "hp")) get_hist(v[2], c->h_pkg);
      else if (!strcmp(k, "hs")) get_hist(v[2], c->h_psys);
    } else if (!strcmp(v[0], "app") && n >= 10) {
      App *a = &A[app_get(v[1])];
      a->cpu[0] = U(2); a->cpu[1] = U(3); a->gpu[0] = D(4); a->gpu[1] = D(5); a->vid[0] = D(6); a->vid[1] = D(7);
      a->wk[0] = U(8); a->wk[1] = U(9);
    }
#undef D
#undef U
  }
  fclose(f);
}

/* noms des C-états / GT : on aligne l'état chargé sur le matériel actuel */
static void align_names(void) {
  int same = G.ncst == NCST;
  for (int i = 0; same && i < NCST; i++) same = !strcmp(G.cstn[i], CSTN[i]);
  if (!same) {
    for (int ci = 0; ci < 2; ci++) { memset(G.c[ci].cst, 0, sizeof G.c[ci].cst); G.c[ci].cst_den = 0; }
    G.ncst = NCST;
    for (int i = 0; i < NCST; i++) snprintf(G.cstn[i], 16, "%s", CSTN[i]);
  }
  if (G.ngt != NGT) {
    for (int ci = 0; ci < 2; ci++) { memset(G.c[ci].gt, 0, sizeof G.c[ci].gt); G.c[ci].gt_den = 0; }
    G.ngt = NGT;
    for (int i = 0; i < NGT; i++) snprintf(G.gtn[i], 8, "%s", GTN[i]);
  }
}

static void reset_acc(void) {
  memset(&G, 0, sizeof G);
  G.since = time(NULL);
  align_names();
  for (int i = 0; i < NA; i++) {
    memset(A[i].cpu, 0, sizeof A[i].cpu);
    memset(A[i].wk, 0, sizeof A[i].wk);
    memset(A[i].gpu, 0, sizeof A[i].gpu);
    memset(A[i].vid, 0, sizeof A[i].vid);
  }
}

/* "performance/balance_power" → "perf/epp:bpw" */
static void mode_abbr(const char *m, char *o, size_t n) {
  char a[48];
  snprintf(a, sizeof a, "%s", m);
  char *sl = strchr(a, '/');
  const char *pp = a, *ep = "";
  if (sl) { *sl = 0; ep = sl + 1; }
  const char *P = !strcmp(pp, "low-power") ? "éco" : !strcmp(pp, "balanced") ? "équil" : !strcmp(pp, "performance") ? "perf"
                  : !strcmp(pp, "quiet") ? "silence" : pp;
  const char *E = !strcmp(ep, "power") ? "pw" : !strcmp(ep, "balance_power") ? "bpw"
                  : !strcmp(ep, "balance_performance") ? "bperf" : !strcmp(ep, "performance") ? "perf"
                  : !strcmp(ep, "default") ? "def" : ep;
  snprintf(o, n, "%s/epp:%s", P, E);
}

static int cmp_last(const void *a, const void *b) {
  const App *x = &A[*(const int *)a], *y = &A[*(const int *)b];
  double vx = x->l_tk + x->l_gpu * HZ, vy = y->l_tk + y->l_gpu * HZ;
  return vx < vy ? 1 : vx > vy ? -1 : 0;
}

static const char *onoff(int v) { return v > 0 ? "on" : v == 0 ? "off" : "?"; }

static void write_now(const Sys *s, double dt, u64 dbusy, u64 dtot) {
  if (NOWRITE) return;
  char tmp[480];
  snprintf(tmp, sizeof tmp, "%s.tmp", F_NOW);
  FILE *f = fopen(tmp, "w");
  if (!f) return;
  struct tm tm;
  localtime_r(&s->wall, &tm);
  fprintf(f, "battrack · %02d:%02d · %s %d%% (%s)", tm.tm_hour, tm.tm_min, s->on_bat ? "batterie" : "secteur", s->pct, s->st);
  if (s->on_bat && s->pw > 0) {
    fprintf(f, " · batt %.1f W", s->pw);
    if (NPWR) {
      double m = 0;
      for (int i = 0; i < NPWR; i++) m += PWR[i];
      fprintf(f, " (moy 10 min %.1f)", m / NPWR);
    }
  }
  if (LW_PSYS >= 0) fprintf(f, " · psys %.1f W", LW_PSYS);
  if (LW_PKG >= 0) fprintf(f, " · pkg %.1f W", LW_PKG);
  fprintf(f, "\n");
  char ma[64];
  mode_abbr(s->mode, ma, sizeof ma);
  fprintf(f, "mode %s · écran %.0f%%%s · clavier %.0f%% · wifi %s%s · bt %s\n", ma, s->bright,
          s->scr_off ? " (éteint)" : "", s->kbd, onoff(s->wifi), s->wifi_up > 0 ? " (connecté)" : "", onoff(s->bt));
  if (dt > 0 && dtot) {
    fprintf(f, "CPU occupé %.1f%% (%d cœurs) · %d processus\n", 100.0 * dbusy / dtot, NCPU, NPA);
    static int idx[MAXAPP];
    for (int i = 0; i < NA; i++) idx[i] = i;
    qsort(idx, (size_t)NA, sizeof idx[0], cmp_last);
    fprintf(f, "dernière minute :  CPU%% (1 cœur)  GPU%%  appli\n");
    for (int k = 0; k < NA && k < 10; k++) {
      App *a = &A[idx[k]];
      if (!a->l_tk && a->l_gpu < 0.01) break;
      fprintf(f, "  %8.1f %9.1f   %s\n", a->l_tk * 100.0 / HZ / dt, a->l_gpu * 100 / dt, a->name);
    }
  }
  if (fclose(f) == 0) rename(tmp, F_NOW);
}

static void write_browser(int gi) {
  if (NOWRITE) return;
  static int had;
  int any = 0;
  for (int i = 0; i < MAXBR; i++) any |= B[i].pid != 0;
  if (!any) {
    if (had) { unlink(F_BR); had = 0; }
    return;
  }
  char tmp[480];
  snprintf(tmp, sizeof tmp, "%s.tmp", F_BR);
  FILE *f = fopen(tmp, "w");
  if (!f) return;
  fprintf(f, "#battrack-browser\t1\t%ld\t%d\n", (long)time(NULL), INTERVAL);
  static const int W[3] = {1, 10, 60};
  for (int i = 0; i < MAXBR; i++) {
    Br *x = &B[i];
    if (!x->pid) continue;
    fprintf(f, "%d\t%d\t%s", x->pid, x->cid, x->ty);
    for (int w = 0; w < 3; w++) {
      int k = W[w] < x->n ? W[w] : x->n;
      double sr = 0, st = 0;
      for (int j = 0; j < k; j++) {
        int id = (gi - j + RING) % RING;
        sr += x->r[id];
        st += DTS[id];
      }
      if (k && st > 0) fprintf(f, "\t%.2f", sr * 100.0 / HZ / st);
      else fprintf(f, "\t-");
    }
    fprintf(f, "\t%d\n", x->n);
  }
  if (fclose(f) == 0) { rename(tmp, F_BR); had = 1; }
}

static void step(void) {
  Sys s;
  read_sys(&s);
  int first = !HAVE_PS;
  double dt = first ? 0 : s.mono - PS.mono;
  double susp = first ? 0 : (s.boot - PS.boot) - dt;
  /* contexte de l'intervalle : 0 batterie, 1 secteur, -1 (bascule ou veille) ignoré */
  int ci = -1;
  if (!first && susp < 5 && dt > 0) ci = (s.on_bat && PS.on_bat) ? 0 : (!s.on_bat && !PS.on_bat) ? 1 : -1;
  int gi = (int)(GS % RING);
  if (!first) DTS[gi] = dt;
  u64 attr = scan(s.mono, first ? 0 : PS.boot, first, ci, (SC % 10) == 0, gi);
  u64 dbusy = 0, dtot = 0;
  LW_PKG = LW_PSYS = -1;
  if (!first) {
    dbusy = s.busy - PS.busy;
    dtot = s.tot - PS.tot;
    double jp = rapl_d(s.pkg, PS.pkg, RMAX_PKG), js = rapl_d(s.psys, PS.psys, RMAX_PSYS);
    if (dt > 0) {
      if (jp >= 0) LW_PKG = jp / dt;
      if (js >= 0) LW_PSYS = js / dt;
    }
    if (ci >= 0) {
      Ctx *c = &G.c[ci];
      c->t += dt;
      if (!strcmp(s.st, "Charging")) c->chg_t += dt;
      c->busy += dbusy; c->tot += dtot; c->attr += attr;
      c->ctxt += s.ctxt - PS.ctxt;
      c->intr += s.intr - PS.intr;
      double wsys = -1; /* puissance "système" : batterie sur batterie, psys sur secteur */
      if (ci == 0) {
        double dwh = (s.ewh >= 0 && PS.ewh >= s.ewh) ? PS.ewh - s.ewh : 0;
        c->wh += dwh;
        double pw = s.pw > 0 ? s.pw : dwh * 3600 / dt;
        if (pw > 0) {
          c->j_bat += pw * dt;
          c->t_bat += dt;
          hist_add(c->h_bat, pw);
          wsys = pw;
          PWR[IPWR] = pw; IPWR = (IPWR + 1) % 10; if (NPWR < 10) NPWR++;
        }
        if (G.sess_open && G.nsess) {
          Sess *x = &G.sess[G.nsess - 1];
          x->act_s += dt; x->act_wh += dwh; x->p1 = s.pct;
        }
      } else if (LW_PSYS >= 0) wsys = LW_PSYS;
      if (jp >= 0) { c->j_pkg += jp; c->t_pkg += dt; hist_add(c->h_pkg, LW_PKG); }
      if (js >= 0) { c->j_psys += js; c->t_psys += dt; hist_add(c->h_psys, LW_PSYS); }
      for (int j = 0; j < NCST; j++) c->cst[j] += s.cst[j] - PS.cst[j];
      c->cst_den += dt * 1e6 * s.ncpu_ok;
      if (s.lpi >= 0 && PS.lpi >= 0 && s.lpi >= PS.lpi) c->lpi += s.lpi - PS.lpi;
      for (int i = 0; i < NGT; i++) c->gt[i] += s.gt[i] - PS.gt[i];
      c->gt_den += dt * 1000;
      if (s.bright >= 0) { c->br_sum += s.bright * dt; c->br_t += dt; }
      if (s.scr_off) c->scr_off += dt;
      if (s.kbd >= 0) { c->kbd_sum += s.kbd * dt; c->kbd_t += dt; if (s.kbd > 0) c->kbd_on += dt; }
      if (s.wifi >= 0 || s.bt >= 0) {
        c->rad_t += dt;
        if (s.wifi > 0) c->wifi_on += dt;
        if (s.wifi_up > 0) c->wifi_up += dt;
        if (s.bt > 0) c->bt_on += dt;
      }
      int mi = mode_get(s.mode);
      c->m_t[mi] += dt;
      if (wsys >= 0) { c->m_jsys[mi] += wsys * dt; c->m_tsys[mi] += dt; }
      if (jp >= 0) { c->m_jpkg[mi] += jp; c->m_tpkg[mi] += dt; }
    } else if (s.on_bat && PS.on_bat && susp >= 5) { /* veille pendant l'intervalle */
      G.susp_s += susp;
      if (s.ewh >= 0 && PS.ewh >= s.ewh) G.susp_wh += PS.ewh - s.ewh;
      if (G.sess_open && G.nsess) { G.sess[G.nsess - 1].susp_s += susp; G.sess[G.nsess - 1].p1 = s.pct; }
    }
  }
  if (s.on_bat && !G.sess_open) {
    if (G.nsess == MAXSESS) { memmove(G.sess, G.sess + 1, sizeof G.sess[0] * (MAXSESS - 1)); G.nsess--; }
    Sess *x = &G.sess[G.nsess++];
    memset(x, 0, sizeof *x);
    x->start = s.wall; x->p0 = x->p1 = s.pct;
    G.sess_open = 1;
    NEED_SAVE = 1;
  } else if (!s.on_bat && G.sess_open) {
    G.sess_open = 0;
    if (G.nsess && G.sess[G.nsess - 1].act_s < 120) G.nsess--; /* débranché 1 min : on oublie */
    NPWR = 0;
    NEED_SAVE = 1;
  }
  write_now(&s, dt, dbusy, dtot);
  write_browser(gi);
  PS = s;
  HAVE_PS = 1;
  if (!first) GS++;
}

/* ================= démon ================= */

static volatile sig_atomic_t SIG_TERM, SIG_FLUSH, SIG_RESET;
static void on_sig(int s) {
  if (s == SIGUSR1) SIG_FLUSH = 1;
  else if (s == SIGUSR2) SIG_RESET = 1;
  else SIG_TERM = 1;
}

static int handle_sigs(void) {
  if (SIG_RESET) { SIG_RESET = 0; reset_acc(); save_state(); }
  if (SIG_FLUSH) { SIG_FLUSH = 0; save_state(); }
  if (SIG_TERM) { save_state(); unlink(F_NOW); unlink(F_BR); return 1; }
  return 0;
}

static int daemon_main(void) {
  mkdir_p(D_RUN);
  mkdir_p(D_STATE);
  int lf = open(F_PID, O_RDWR | O_CREAT | O_CLOEXEC, 0644);
  if (lf < 0 || flock(lf, LOCK_EX | LOCK_NB)) { fprintf(stderr, "battrack: démon déjà lancé\n"); return 1; }
  if (ftruncate(lf, 0) == 0) dprintf(lf, "%d\n", getpid());

  /* priorité minimale partout */
  setpriority(PRIO_PROCESS, 0, 19);
  struct sched_param sp = {0};
  sched_setscheduler(0, SCHED_IDLE, &sp);
  syscall(SYS_ioprio_set, 1, 0, 3 << 13);
  prctl(PR_SET_TIMERSLACK, 2000000000UL); /* le noyau peut grouper notre réveil avec d'autres */

  apps_init();
  load_state(1);
  if (!G.since) G.since = time(NULL);
  align_names();
  G.sess_open = 0;

  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_sig;
  sigaction(SIGTERM, &sa, NULL);
  sigaction(SIGINT, &sa, NULL);
  sigaction(SIGHUP, &sa, NULL);
  sigaction(SIGUSR1, &sa, NULL);
  sigaction(SIGUSR2, &sa, NULL);

  struct timespec next;
  clock_gettime(CLOCK_MONOTONIC, &next);
  int since_save = 0;
  for (;;) {
    step();
    if (++since_save >= (PS.on_bat ? 15 : 60) || NEED_SAVE) { save_state(); since_save = 0; NEED_SAVE = 0; }
    if (handle_sigs()) return 0;
    next.tv_sec += INTERVAL;
    struct timespec nw;
    clock_gettime(CLOCK_MONOTONIC, &nw);
    if (next.tv_sec <= nw.tv_sec) { next = nw; next.tv_sec += INTERVAL; }
    while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &next, NULL) == EINTR)
      if (handle_sigs()) return 0;
  }
}

/* ================= rapport ================= */

static int daemon_pid(void) {
  char b[32], p[64], c[32];
  if (rd(F_PID, b, sizeof b) <= 0) return 0;
  int pid = atoi(b);
  if (pid <= 0) return 0;
  snprintf(p, sizeof p, "/proc/%d/comm", pid);
  if (!rdline(p, c, sizeof c) || strcmp(c, "battrack")) return 0;
  int fd = open(F_PID, O_RDONLY | O_CLOEXEC); /* vivant = verrou tenu */
  if (fd < 0) return 0;
  int held = flock(fd, LOCK_EX | LOCK_NB) != 0;
  close(fd);
  return held ? pid : 0;
}

static void flush_daemon(int pid) {
  struct stat a, b;
  int had = stat(F_STATE, &a) == 0;
  kill(pid, SIGUSR1);
  for (int i = 0; i < 80; i++) {
    struct timespec t = {0, 25000000};
    nanosleep(&t, NULL);
    if (stat(F_STATE, &b) == 0 && (!had || b.st_mtim.tv_sec != a.st_mtim.tv_sec || b.st_mtim.tv_nsec != a.st_mtim.tv_nsec)) return;
  }
}

static void fdur(double s, char *o, size_t n) {
  long m = (long)(s / 60 + 0.5);
  if (m >= 1440) snprintf(o, n, "%ldj%02ldh", m / 1440, (m % 1440) / 60);
  else snprintf(o, n, "%ldh%02ld", m / 60, m % 60);
}

/* quantile d'un histogramme (W), -1 si vide */
static double hq(const u32 *h, double q) {
  u64 t = 0, c = 0;
  for (int i = 0; i < NHIST; i++) t += h[i];
  if (!t) return -1;
  for (int i = 0; i < NHIST; i++) {
    c += h[i];
    if (c > 0 && c >= q * t) return i * 0.1 + 0.05;
  }
  return NHIST * 0.1;
}

static int run(const char *cmd, char *o, size_t n) {
  o[0] = 0;
  FILE *p = popen(cmd, "r");
  if (!p) return 0;
  size_t k = fread(o, 1, n - 1, p);
  o[k] = 0;
  pclose(p);
  /* enlève les codes couleur */
  char *w = o;
  for (char *r = o; *r; r++) {
    if (*r == 0x1b) { while (*r && *r != 'm') r++; if (!*r) break; continue; }
    *w++ = *r;
  }
  *w = 0;
  return (int)k;
}

static const char *pci_tag(unsigned long cls) {
  switch (cls >> 8) {
  case 0x0108: return "nvme";
  case 0x0500: return "ram";
  case 0x0700: return "uart";
  case 0x0805: return "sd";
  case 0x0280: return "wifi";
  case 0x0300: case 0x0380: return "gpu";
  case 0x0401: case 0x0403: return "audio";
  case 0x0480: return "ipu";
  case 0x0600: return "host";
  case 0x0601: return "isa";
  case 0x0604: return "pcie";
  case 0x0780: return "mei";
  case 0x0880: return "sys";
  case 0x0c03: return "usb";
  case 0x0c05: return "smbus";
  case 0x0c80: return "serial";
  case 0x1180: return "therm";
  case 0x1200: return "npu";
  }
  return "?";
}

typedef struct {
  char ppd[32], plat[32], drv[32], gov[24], epp[64], res[24], vrr[24], wifi[16], aspm[24], pci[400], procs[300],
      cmd[300], xe[80], kern[64];
  int hz, bright, npci, turbo, bt, maxmhz, capmhz, kbd, kbdmax;
  double usbas, dirty;
} Conf;

static void read_conf(Conf *c) {
  memset(c, 0, sizeof *c);
  char b[8192], p[320], t[128];
  run("powerprofilesctl get 2>/dev/null", c->ppd, sizeof c->ppd);
  c->ppd[strcspn(c->ppd, "\n")] = 0;
  rdline("/sys/firmware/acpi/platform_profile", c->plat, sizeof c->plat);
  rdline("/sys/devices/system/cpu/cpufreq/policy0/scaling_driver", c->drv, sizeof c->drv);
  rdline("/sys/devices/system/cpu/intel_pstate/status", t, sizeof t);
  if (*t) { size_t l = strlen(c->drv); snprintf(c->drv + l, sizeof c->drv - l, "/%s", t); }
  rdline("/sys/devices/system/cpu/cpufreq/policy0/scaling_governor", c->gov, sizeof c->gov);
  /* EPP de toutes les policies (valeurs distinctes) */
  for (int i = 0; i < 64; i++) {
    snprintf(p, sizeof p, "/sys/devices/system/cpu/cpufreq/policy%d/energy_performance_preference", i);
    if (!rdline(p, t, sizeof t)) { if (i > 0) break; continue; }
    if (!strstr(c->epp, t)) {
      size_t l = strlen(c->epp);
      snprintf(c->epp + l, sizeof c->epp - l, "%s%s", l ? "/" : "", t);
    }
  }
  c->turbo = rdnum("/sys/devices/system/cpu/intel_pstate/no_turbo", 0) == 0;
  c->maxmhz = (int)(rdnum("/sys/devices/system/cpu/cpufreq/policy0/cpuinfo_max_freq", 0) / 1000);
  c->capmhz = (int)(rdnum("/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq", 0) / 1000);
  /* écran (KDE) — kscreen-doctor répond parfois vide : 3 essais */
  for (int tries = 0; tries < 3 && !c->hz; tries++) {
    if (!run("kscreen-doctor -o 2>/dev/null", b, sizeof b)) continue;
    char *m = strstr(b, "Modes:");
    if (m) {
      char *star = strchr(m, '*');
      if (star) {
        char *s = star;
        while (s > m && s[-1] != ' ' && s[-1] != ':') s--;
        char mode[48];
        size_t n = (size_t)(star - s) < sizeof mode - 1 ? (size_t)(star - s) : sizeof mode - 1;
        memcpy(mode, s, n);
        mode[n] = 0;
        char *at = strchr(mode, '@');
        if (at) {
          c->hz = (int)(strtod(at + 1, NULL) + 0.5);
          *at = 0;
        }
        char *bang = strchr(mode, '!');
        if (bang) *bang = 0;
        snprintf(c->res, sizeof c->res, "%s", mode);
      }
    }
    char *v = strstr(b, "Vrr: ");
    if (v) { sscanf(v + 5, "%23s", c->vrr); }
  }
  c->bright = -1;
  if (*P_BL) {
    snprintf(p, sizeof p, "%s/brightness", P_BL);
    double v = rdnum(p, -1);
    snprintf(p, sizeof p, "%s/max_brightness", P_BL);
    double m = rdnum(p, -1);
    if (v >= 0 && m > 0) c->bright = (int)(v / m * 100 + 0.5);
  }
  c->kbd = -1;
  if (*P_KBD) {
    snprintf(p, sizeof p, "%s/brightness", P_KBD);
    c->kbd = (int)rdnum(p, -1);
    c->kbdmax = (int)KBD_MAX;
  }
  /* wifi */
  DIR *d = opendir("/sys/class/net");
  struct dirent *e;
  if (d) {
    while ((e = readdir(d))) {
      if (e->d_name[0] == '.') continue;
      snprintf(p, sizeof p, "/sys/class/net/%s/wireless", e->d_name);
      if (!exists(p)) continue;
      snprintf(p, sizeof p, "iw dev %s get power_save 2>/dev/null", e->d_name);
      run(p, t, sizeof t);
      char *o = strstr(t, ": ");
      snprintf(c->wifi, sizeof c->wifi, "%s", o ? o + 2 : "?");
      c->wifi[strcspn(c->wifi, "\n")] = 0;
      break;
    }
    closedir(d);
  }
  int w, bt;
  read_radios(&w, &bt);
  c->bt = bt;
  rdline("/sys/module/pcie_aspm/parameters/policy", b, sizeof b);
  {
    char *l = strchr(b, '['), *r = l ? strchr(l, ']') : NULL;
    if (l && r) { *r = 0; snprintf(c->aspm, sizeof c->aspm, "%s", l + 1); }
  }
  /* PCI dont le runtime PM est bloqué sur "on" */
  if ((d = opendir("/sys/bus/pci/devices"))) {
    while ((e = readdir(d))) {
      if (e->d_name[0] == '.') continue;
      snprintf(p, sizeof p, "/sys/bus/pci/devices/%s/power/control", e->d_name);
      if (!rdline(p, t, sizeof t) || strcmp(t, "on")) continue;
      snprintf(p, sizeof p, "/sys/bus/pci/devices/%s/class", e->d_name);
      rdline(p, t, sizeof t);
      const char *nm = e->d_name;
      if (!strncmp(nm, "0000:00:", 8)) nm += 8;
      else if (!strncmp(nm, "0000:", 5)) nm += 5;
      size_t l = strlen(c->pci);
      snprintf(c->pci + l, sizeof c->pci - l, "%s%s(%s)", l ? " " : "", nm, pci_tag(strtoul(t, NULL, 16)));
      c->npci++;
    }
    closedir(d);
  }
  c->usbas = rdnum("/sys/module/usbcore/parameters/autosuspend", -1);
  c->dirty = rdnum("/proc/sys/vm/dirty_writeback_centisecs", -1) / 100;
  {
    static const char *X[] = {"enable_psr", "enable_fbc", "enable_dc", "enable_panel_replay", 0};
    static const char *S[] = {"psr", "fbc", "dc", "pr"};
    for (int i = 0; X[i]; i++) {
      snprintf(p, sizeof p, "/sys/module/xe/parameters/%s", X[i]);
      if (!rdline(p, t, sizeof t)) snprintf(p, sizeof p, "/sys/module/i915/parameters/%s", X[i]), rdline(p, t, sizeof t);
      if (!*t) continue;
      size_t l = strlen(c->xe);
      snprintf(c->xe + l, sizeof c->xe - l, "%s%s=%s", l ? " " : "", S[i], t);
    }
  }
  /* démons connus pour peser sur la batterie */
  static const char *K[] = {"baloo_file", "baloo_file_extr", "tracker-miner-f", "kdeconnectd", "ananicy-cpp", "bpftune",
                            "intel_lpmd", "tlp", "auto-cpufreq", "thermald", "tuned", "system76-power", "syncthing",
                            "dropbox", "nextcloud", "packagekitd", "dockerd", "containerd", "waydroid", "lxc-start",
                            "ollama", "steam", "Discord", "discord", "spotify", "easyeffects", "zoom", "teams", "slack",
                            "akonadi_control", "kwalletd6", "geoclue", "fwupd", "snapd", "cupsd", "1password", 0};
  int found[64] = {0};
  if ((d = opendir("/proc"))) {
    while ((e = readdir(d))) {
      if (!isdigit((u8)e->d_name[0])) continue;
      snprintf(p, sizeof p, "/proc/%s/comm", e->d_name);
      if (!rdline(p, t, sizeof t)) continue;
      for (int i = 0; K[i]; i++)
        if (!strcmp(t, K[i])) found[i] = 1;
    }
    closedir(d);
  }
  for (int i = 0; K[i]; i++)
    if (found[i]) {
      size_t l = strlen(c->procs);
      snprintf(c->procs + l, sizeof c->procs - l, "%s%s", l ? " " : "", K[i]);
    }
  /* ligne de commande noyau sans le bruit */
  rdline("/proc/cmdline", b, sizeof b);
  char *sv = NULL;
  for (char *tok = strtok_r(b, " ", &sv); tok; tok = strtok_r(NULL, " ", &sv)) {
    if (!strncmp(tok, "root=", 5) || !strncmp(tok, "rootflags=", 10) || !strcmp(tok, "rw") || !strcmp(tok, "ro") ||
        !strncmp(tok, "initrd=", 7) || !strncmp(tok, "BOOT_IMAGE=", 11) || !strncmp(tok, "cryptdevice=", 12) ||
        !strncmp(tok, "resume=", 7))
      continue;
    size_t l = strlen(c->cmd);
    snprintf(c->cmd + l, sizeof c->cmd - l, "%s%s", l ? " " : "", tok);
  }
  struct utsname u;
  if (!uname(&u)) snprintf(c->kern, sizeof c->kern, "%s", u.release);
}

/* largeur d'affichage en caractères (UTF-8) */
static int u8len(const char *s) {
  int n = 0;
  for (; *s; s++)
    if (((u8)*s & 0xC0) != 0x80) n++;
  return n;
}

static void putpad(FILE *o, const char *s, int w) {
  fputs(s, o);
  for (int n = u8len(s); n < w; n++) fputc(' ', o);
}

static void row2(FILE *o, const char *label, const char *a, const char *b) {
  putpad(o, label, 13);
  fputc(' ', o);
  putpad(o, a, 34);
  fputc(' ', o);
  fputs(b, o);
  fputc('\n', o);
}

typedef struct { int i; double w[2], cpu[2], gpu[2], wk[2], score; } Row;
static int cmp_row(const void *a, const void *b) {
  double x = ((const Row *)a)->score, y = ((const Row *)b)->score;
  return x < y ? 1 : x > y ? -1 : 0;
}

static void ctx_cell(FILE *o, int has, double dyn, double wsum, const Row *r, int ci) {
  if (!has) { fprintf(o, "     -     -     -      -"); return; }
  char w[16];
  if (dyn >= 0 && wsum > 0) snprintf(w, sizeof w, "%5.2f", dyn * r->w[ci] / wsum);
  else snprintf(w, sizeof w, "    -");
  fprintf(o, " %s %5.1f %5.1f %6.0f", w, r->cpu[ci], r->gpu[ci], r->wk[ci]);
}

static void report(FILE *o) {
  int pid = daemon_pid();
  if (pid) flush_daemon(pid);
  apps_init();
  discover();
  load_state(0);
  Conf cf;
  read_conf(&cf);
  Sys s;
  memset(&s, 0, sizeof s);
  read_bat(&s);

  char a1[48], a2[48];
  time_t now = time(NULL);
  struct tm tn, ts;
  localtime_r(&now, &tn);
  time_t since = G.since ? G.since : now;
  localtime_r(&since, &ts);
  fdur((double)(now - since), a1, sizeof a1);
  fprintf(o, "### battrack %s · %02d/%02d %02d:%02d · depuis %02d/%02d %02d:%02d (%s)", VERSION, tn.tm_mday,
          tn.tm_mon + 1, tn.tm_hour, tn.tm_min, ts.tm_mday, ts.tm_mon + 1, ts.tm_hour, ts.tm_min, a1);
  if (pid) {
    char p[64], b[1024];
    snprintf(p, sizeof p, "/proc/%d/stat", pid);
    if (rd(p, b, sizeof b) > 0) {
      char *q = strrchr(b, ')');
      u64 f[25] = {0};
      if (q && q[1]) {
        q += 3;
        for (int k = 4; k <= 24 && *q; k++) f[k] = strtoull(q, &q, 10);
      }
      double cpu = (double)(f[14] + f[15]) / HZ, age = tsec(CLOCK_BOOTTIME) - (double)f[22] / HZ;
      fprintf(o, " · démon %.1fs CPU (%.4f%%)", cpu, age > 0 ? cpu / age * 100 : 0);
    }
  } else fprintf(o, " · DÉMON ARRÊTÉ (systemctl --user start battrack)");
  fprintf(o, "\n");

  double full = bnum("energy_full", -1) / 1e6, design = bnum("energy_full_design", -1) / 1e6;
  int cyc = (int)bnum("cycle_count", -1);
  int rapl = (*P_PKG && rdnum(P_PKG, -1) >= 0) + (*P_PSYS && rdnum(P_PSYS, -1) >= 0);
  fprintf(o, "MACHINE  bat %.1f/%.1f Wh (%.0f%%) · %d cycles · %d%% %s · noyau %s · RAPL %s\n", full, design,
          design > 0 ? full / design * 100 : 0, cyc, s.pct, s.st, cf.kern,
          rapl == 2 ? "pkg+psys" : rapl ? "partiel" : "illisible");

  Ctx *C = G.c;
  int has[2] = {C[0].t >= 60, C[1].t >= 60};
  char c0[128], c1[128];
  char *cc[2] = {c0, c1};
  row2(o, "", "── BATTERIE ──", "── SECTEUR ──");

  for (int ci = 0; ci < 2; ci++) {
    fdur(C[ci].t, a1, sizeof a1);
    if (ci == 1 && C[1].chg_t > 60) snprintf(cc[ci], 128, "%s · en charge %.0f%%", a1, C[1].chg_t / C[1].t * 100);
    else snprintf(cc[ci], 128, "%s", a1);
  }
  row2(o, "temps actif", c0, c1);

  double batw = -1, psysw[2] = {-1, -1}, pkgw[2] = {-1, -1};
  if (C[0].t_bat >= 60) batw = C[0].wh > 0.05 ? C[0].wh / (C[0].t / 3600) : C[0].j_bat / C[0].t_bat;
  for (int ci = 0; ci < 2; ci++) {
    if (C[ci].t_psys >= 60) psysw[ci] = C[ci].j_psys / C[ci].t_psys;
    if (C[ci].t_pkg >= 60) pkgw[ci] = C[ci].j_pkg / C[ci].t_pkg;
  }
  if (batw >= 0) snprintf(c0, 128, "%.2f (p10 %.1f · p90 %.1f)", batw, hq(C[0].h_bat, .1), hq(C[0].h_bat, .9));
  else snprintf(c0, 128, "%s", has[0] ? "?" : "—");
  row2(o, "P batterie W", c0, "— (branché)");
  for (int ci = 0; ci < 2; ci++) {
    if (psysw[ci] >= 0) snprintf(cc[ci], 128, "%.2f (p10 %.1f · p90 %.1f)", psysw[ci], hq(C[ci].h_psys, .1), hq(C[ci].h_psys, .9));
    else snprintf(cc[ci], 128, "—");
  }
  row2(o, "P psys W", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (pkgw[ci] >= 0) snprintf(cc[ci], 128, "%.2f (repos %.2f · p90 %.1f)", pkgw[ci], hq(C[ci].h_pkg, .05), hq(C[ci].h_pkg, .9));
    else snprintf(cc[ci], 128, "—");
  }
  row2(o, "P package W", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    double sys = ci == 0 && batw >= 0 ? batw : psysw[ci];
    if (sys >= 0 && pkgw[ci] >= 0) snprintf(cc[ci], 128, "%.2f (écran, wifi, ssd…)", sys - pkgw[ci]);
    else snprintf(cc[ci], 128, "—");
  }
  row2(o, "hors package", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci]) { snprintf(cc[ci], 128, "—"); continue; }
    snprintf(cc[ci], 128, "%.1f%% · %.1fk ctx/s · %.1fk irq/s", C[ci].tot ? 100.0 * C[ci].busy / C[ci].tot : 0,
             C[ci].ctxt / C[ci].t / 1000, C[ci].intr / C[ci].t / 1000);
  }
  row2(o, "CPU occupé", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci] || C[ci].cst_den <= 0) { snprintf(cc[ci], 128, "—"); continue; }
    size_t l = 0;
    cc[ci][0] = 0;
    for (int i = 0; i < G.ncst && l < 100; i++) {
      double v = C[ci].cst[i] / C[ci].cst_den * 100;
      if (!strncmp(G.cstn[i], "POLL", 4) && v < 0.5) continue;
      char nm[16];
      snprintf(nm, sizeof nm, "%s", G.cstn[i]);
      char *u = strstr(nm, "_ACPI");
      if (u) *u = 0;
      l += (size_t)snprintf(cc[ci] + l, 128 - l, "%s%s %.0f", l ? " " : "", nm, v);
    }
    if (l < 110) snprintf(cc[ci] + l, 128 - l, " · S0ix %.0f", C[ci].lpi / (C[ci].t * 1e6) * 100);
  }
  row2(o, "C-états %", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci] || C[ci].gt_den <= 0 || !G.ngt) { snprintf(cc[ci], 128, "—"); continue; }
    size_t l = 0;
    cc[ci][0] = 0;
    for (int i = 0; i < G.ngt; i++)
      l += (size_t)snprintf(cc[ci] + l, 128 - l, "%s%s %.0f%%", l ? " · " : "", G.gtn[i], C[ci].gt[i] / C[ci].gt_den * 100);
  }
  row2(o, "GPU au repos", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci] || C[ci].br_t <= 0) { snprintf(cc[ci], 128, "—"); continue; }
    snprintf(cc[ci], 128, "lum %.0f%% · éteint %.0f%%", C[ci].br_sum / C[ci].br_t, C[ci].scr_off / C[ci].t * 100);
  }
  row2(o, "écran", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci] || C[ci].kbd_t <= 0) { snprintf(cc[ci], 128, "—"); continue; }
    snprintf(cc[ci], 128, "moy %.0f%% · allumé %.0f%% du temps", C[ci].kbd_sum / C[ci].kbd_t, C[ci].kbd_on / C[ci].kbd_t * 100);
  }
  row2(o, "clavier", c0, c1);
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci] || C[ci].rad_t <= 0) { snprintf(cc[ci], 128, "—"); continue; }
    snprintf(cc[ci], 128, "wifi %.0f%% (co %.0f%%) · bt %.0f%%", C[ci].wifi_on / C[ci].rad_t * 100,
             C[ci].wifi_up / C[ci].rad_t * 100, C[ci].bt_on / C[ci].rad_t * 100);
  }
  row2(o, "radios", c0, c1);
  {
    double cap = full > 0 ? full : 50;
    if (batw > 0.1) { fdur(cap / batw * 3600, a1, sizeof a1); snprintf(c0, 128, "~%s (%.0f Wh)", a1, cap); }
    else snprintf(c0, 128, "—");
    if (psysw[1] > 0.1) { fdur(cap / psysw[1] * 3600, a2, sizeof a2); snprintf(c1, 128, "si débranché ~%s", a2); }
    else snprintf(c1, 128, "—");
    row2(o, "autonomie", c0, c1);
  }
  /* modes énergie : part du temps, W système, W package */
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci] || !G.nmode) continue;
    fprintf(o, "modes %s :", ci ? "secteur " : "batterie");
    int k = 0;
    for (int i = 0; i < G.nmode; i++) {
      if (C[ci].m_t[i] < 1) continue;
      char ma[64];
      mode_abbr(G.modes[i], ma, sizeof ma);
      fprintf(o, "%s %s %.0f%%", k++ ? " ·" : "", ma, C[ci].m_t[i] / C[ci].t * 100);
      if (C[ci].m_tsys[i] > 30) fprintf(o, " %.1fW", C[ci].m_jsys[i] / C[ci].m_tsys[i]);
      if (C[ci].m_tpkg[i] > 30) fprintf(o, " (pkg %.1f)", C[ci].m_jpkg[i] / C[ci].m_tpkg[i]);
    }
    fprintf(o, "\n");
  }
  if (G.susp_s > 60) {
    fdur(G.susp_s, a1, sizeof a1);
    fprintf(o, "veille   %s · %.2f Wh · %.2f W\n", a1, G.susp_wh, G.susp_wh / (G.susp_s / 3600));
  }
  if (G.nsess) {
    fprintf(o, "sessions batterie:");
    int k0 = G.nsess > 4 ? G.nsess - 4 : 0;
    for (int k = k0; k < G.nsess; k++) {
      Sess *x = &G.sess[k];
      time_t st = x->start;
      struct tm tm;
      localtime_r(&st, &tm);
      fdur(x->act_s, a1, sizeof a1);
      fprintf(o, "%s %02d/%02d %02d:%02d %s %.1fW %d→%d%%", k > k0 ? " |" : "", tm.tm_mday, tm.tm_mon + 1, tm.tm_hour,
              tm.tm_min, a1, x->act_s > 60 ? x->act_wh / (x->act_s / 3600) : 0, x->p0, x->p1);
    }
    fprintf(o, "\n");
  }

  char mhz[32] = "";
  if (cf.capmhz && cf.maxmhz && cf.capmhz < cf.maxmhz) snprintf(mhz, sizeof mhz, " fmax=%d/%dMHz", cf.capmhz, cf.maxmhz);
  fprintf(o, "CONF     maintenant: ppd=%s plat=%s %s gov=%s epp=%s turbo=%s%s\n", *cf.ppd ? cf.ppd : "?",
          *cf.plat ? cf.plat : "?", cf.drv, cf.gov, cf.epp, cf.turbo ? "on" : "off", mhz);
  fprintf(o, "  écran %s@%d vrr=%s lum=%d%% · clavier=%d/%d · wifi ps=%s · bt=%s · aspm=%s · usb_as=%.0fs · wb=%.0fs%s%s\n",
          *cf.res ? cf.res : "?", cf.hz, *cf.vrr ? cf.vrr : "?", cf.bright, cf.kbd, cf.kbdmax, *cf.wifi ? cf.wifi : "?",
          onoff(cf.bt), *cf.aspm ? cf.aspm : "?", cf.usbas, cf.dirty, *cf.xe ? " · " : "", cf.xe);
  if (cf.npci) fprintf(o, "  pci pm=on (%d): %s\n", cf.npci, cf.pci);
  if (*cf.procs) fprintf(o, "  démons: %s\n", cf.procs);
  fprintf(o, "  cmdline: %s\n", cf.cmd);

  /* tableau des applis, deux contextes côte à côte */
  static Row R[MAXAPP + 1];
  int nr = 0;
  double wsum[2] = {0, 0}, dyn[2] = {-1, -1}, unat[2] = {0, 0};
  for (int ci = 0; ci < 2; ci++) {
    if (!has[ci]) continue;
    unat[ci] = C[ci].busy > C[ci].attr ? (double)(C[ci].busy - C[ci].attr) / HZ : 0;
    wsum[ci] = unat[ci];
    double base = hq(C[ci].h_pkg, .05);
    if (pkgw[ci] >= 0 && base >= 0) dyn[ci] = pkgw[ci] > base ? pkgw[ci] - base : 0;
    else if (ci == 0 && batw >= 0 && hq(C[0].h_bat, .05) >= 0) dyn[0] = batw > hq(C[0].h_bat, .05) ? batw - hq(C[0].h_bat, .05) : 0;
  }
  for (int i = 0; i < NA; i++) {
    App *a = &A[i];
    Row r;
    memset(&r, 0, sizeof r);
    r.i = i;
    int any = 0;
    for (int ci = 0; ci < 2; ci++) {
      if (!has[ci]) continue;
      double T = C[ci].t;
      r.w[ci] = (double)a->cpu[ci] / HZ + a->gpu[ci] + a->vid[ci];
      r.cpu[ci] = (double)a->cpu[ci] * 100 / HZ / T;
      r.gpu[ci] = (a->gpu[ci] + a->vid[ci]) * 100 / T;
      r.wk[ci] = a->wk[ci] / T;
      wsum[ci] += r.w[ci];
      any |= r.w[ci] > 0;
    }
    if (any) R[nr++] = r;
  }
  for (int k = 0; k < nr; k++)
    for (int ci = 0; ci < 2; ci++)
      if (wsum[ci] > 0) R[k].score += R[k].w[ci] / wsum[ci];
  qsort(R, (size_t)nr, sizeof R[0], cmp_row);
  fprintf(o, "\nTOP applis                ──────── BATTERIE ──────  ──────── SECTEUR ───────\n");
  fprintf(o, "                             W~  CPU%%  GPU%%  rév/s     W~  CPU%%  GPU%%  rév/s\n");
  for (int k = 0; k < nr && k < 15; k++) {
    fprintf(o, "%2d %-22.22s", k + 1, A[R[k].i].name);
    ctx_cell(o, has[0], dyn[0], wsum[0], &R[k], 0);
    fprintf(o, " ");
    ctx_cell(o, has[1], dyn[1], wsum[1], &R[k], 1);
    fprintf(o, "\n");
  }
  {
    Row u;
    memset(&u, 0, sizeof u);
    for (int ci = 0; ci < 2; ci++) {
      u.w[ci] = unat[ci];
      if (has[ci]) u.cpu[ci] = unat[ci] * 100 / C[ci].t;
    }
    fprintf(o, "   %-22s", "[courts/irq/autres]");
    ctx_cell(o, has[0], dyn[0], wsum[0], &u, 0);
    fprintf(o, " ");
    ctx_cell(o, has[1], dyn[1], wsum[1], &u, 1);
    fprintf(o, "\n");
  }
  /* GPU : occupé total vs attribué ; décodage vidéo matériel */
  {
    int k = 0;
    for (int ci = 0; ci < 2; ci++) {
      if (!has[ci] || C[ci].gt_den <= 0 || !G.ngt) continue;
      double att = 0;
      for (int j = 0; j < nr; j++) att += R[j].gpu[ci];
      fprintf(o, "%s%s : GPU 3D occupé %.1f%%, attribué %.1f%%", k++ ? " · " : "   ", ci ? "secteur" : "batterie",
              100 - C[ci].gt[0] / C[ci].gt_den * 100, att);
    }
    if (k) fprintf(o, " (reste = kwin, illisible sans root)\n");
  }
  {
    int k = 0;
    for (int i = 0; i < NA; i++) {
      double v0 = has[0] ? A[i].vid[0] * 100 / C[0].t : 0, v1 = has[1] ? A[i].vid[1] * 100 / C[1].t : 0;
      if (v0 < 0.1 && v1 < 0.1) continue;
      fprintf(o, "%s %s %.1f/%.1f%%", k++ ? "," : "   vidéo matérielle (bat/sect):", A[i].name, v0, v1);
    }
    if (k) fprintf(o, "\n");
  }

  /* pistes automatiques */
  char hints[8][128];
  int nh = 0;
  if (!has[0]) snprintf(hints[nh++], 128, "pas de données batterie : débranche ≥1 h");
  if (cf.hz >= 90) snprintf(hints[nh++], 128, "écran %dHz → 60Hz sur batterie (≈0,5–1 W)", cf.hz);
  for (int ci = 0; ci < 2 && nh < 8; ci++)
    if (has[ci] && C[ci].br_t > 0 && C[ci].br_sum / C[ci].br_t > 55) {
      snprintf(hints[nh++], 128, "luminosité moy %.0f%% (%s) : -15 pts ≈ -0,3 à -0,8 W", C[ci].br_sum / C[ci].br_t,
               ci ? "secteur" : "batterie");
      break;
    }
  for (int ci = 0; ci < 2 && nh < 8; ci++)
    if (has[ci] && C[ci].kbd_t > 0 && C[ci].kbd_on / C[ci].kbd_t > 0.2) {
      snprintf(hints[nh++], 128, "clavier rétroéclairé %.0f%% du temps (≈0,1–0,3 W)", C[ci].kbd_on / C[ci].kbd_t * 100);
      break;
    }
  if (has[0] && nh < 8)
    for (int i = 0; i < G.nmode; i++)
      if (!strncmp(G.modes[i], "performance", 11) && C[0].m_t[i] / C[0].t > 0.2) {
        snprintf(hints[nh++], 128, "mode perf %.0f%% du temps sur batterie → éco", C[0].m_t[i] / C[0].t * 100);
        break;
      }
  for (int ci = 0; ci < 2 && nh < 8; ci++)
    if (has[ci] && C[ci].rad_t > 0 && C[ci].bt_on / C[ci].rad_t > 0.5) {
      snprintf(hints[nh++], 128, "Bluetooth allumé %.0f%% du temps : couper si inutile", C[ci].bt_on / C[ci].rad_t * 100);
      break;
    }
  if (cf.npci && nh < 8) snprintf(hints[nh++], 128, "%d périph. PCI runtime-PM=on → auto", cf.npci);
  if (nr && nh < 8 && R[0].score > 0.5) {
    double sh = 0;
    int n = 0;
    for (int ci = 0; ci < 2; ci++)
      if (wsum[ci] > 0) { sh += R[0].w[ci] / wsum[ci]; n++; }
    if (n) snprintf(hints[nh++], 128, "%s = %.0f%% de l'activité", A[R[0].i].name, sh / n * 100);
  }
  if (!strcmp(cf.wifi, "off") && nh < 8) snprintf(hints[nh++], 128, "wifi power_save off → on");
  if (nh) {
    fprintf(o, "PISTES   ");
    for (int i = 0; i < nh && i < 6; i++) fprintf(o, "%s%s", i ? (i == 3 ? "\n         " : " · ") : "", hints[i]);
    fprintf(o, "\n");
  }
  fprintf(o, "W~ = part du package (CPU+GPU+RAM) au-dessus du repos · psys = plateforme entière · rév/s = réveils/s\n");
  fprintf(o, "### fin\n");
}

/* ================= hôte natif (extension Helium) ================= */

static int readn(int fd, void *b, size_t n) {
  size_t k = 0;
  while (k < n) {
    ssize_t r = read(fd, (char *)b + k, n - k);
    if (r <= 0) return -1;
    k += (size_t)r;
  }
  return 0;
}

static void writen(int fd, const void *b, size_t n) {
  size_t k = 0;
  while (k < n) {
    ssize_t r = write(fd, (const char *)b + k, n - k);
    if (r <= 0) return;
    k += (size_t)r;
  }
}

static size_t jesc(char *o, size_t n, const char *s) {
  size_t k = 0;
  for (; *s && k + 7 < n; s++) {
    u8 ch = (u8)*s;
    if (ch == '"' || ch == '\\') { o[k++] = '\\'; o[k++] = (char)ch; }
    else if (ch < 0x20) k += (size_t)snprintf(o + k, n - k, "\\u%04x", ch);
    else o[k++] = (char)ch;
  }
  o[k] = 0;
  return k;
}

typedef struct { int pid; u64 start; int cid; char ty[12]; } NC;

static int nm_main(void) {
  char bcomm[32] = "helium", p[128];
  snprintf(p, sizeof p, "/proc/%d/comm", getppid());
  static const char *FAM[] = {"helium", "chrome", "chromium", "brave", "vivaldi-bin", "msedge", "opera", "thorium", 0};
  int okp = rdline(p, bcomm, sizeof bcomm), fam = 0;
  for (int i = 0; okp && FAM[i]; i++) fam |= !strcmp(bcomm, FAM[i]);
  if (!fam) snprintf(bcomm, sizeof bcomm, "%s", BROW[0]); /* lancé à la main : 1er navigateur suivi */
  discover();
  static NC cache[512];
  int nc = 0;
  static char out[1 << 18], flags[600];
  for (;;) {
    u32 len;
    if (readn(0, &len, 4)) return 0;
    char tmp[256];
    while (len) {
      size_t k = len < sizeof tmp ? len : sizeof tmp;
      if (readn(0, tmp, k)) return 0;
      len -= (u32)k;
    }
    /* historique du démon */
    struct { int pid; char m[3][12]; } hist[MAXBR];
    int nhist = 0, dmn = 0;
    struct stat st;
    if (stat(F_BR, &st) == 0 && time(NULL) - st.st_mtime < 3 * INTERVAL + 30) {
      FILE *f = fopen(F_BR, "r");
      char l[256];
      while (f && fgets(l, sizeof l, f)) {
        if (l[0] == '#') { dmn = 1; continue; }
        char ty[16];
        int pid, cid;
        if (nhist < MAXBR && sscanf(l, "%d\t%d\t%15s\t%11s\t%11s\t%11s", &pid, &cid, ty, hist[nhist].m[0], hist[nhist].m[1],
                                    hist[nhist].m[2]) == 6) {
          hist[nhist].pid = pid;
          nhist++;
        }
      }
      if (f) fclose(f);
    }
    Sys s;
    memset(&s, 0, sizeof s);
    read_bat(&s);
    double pkg = *P_PKG ? rdnum(P_PKG, -1) : -1, psys = *P_PSYS ? rdnum(P_PSYS, -1) : -1;
    double boot = tsec(CLOCK_BOOTTIME);
    size_t k = (size_t)snprintf(out, sizeof out,
                                "{\"v\":2,\"hz\":%ld,\"t\":%.0f,\"daemon\":%d,\"bat\":%d,\"pw\":%.2f,\"pct\":%d,"
                                "\"pkg\":%.0f,\"psys\":%.0f,\"rmax\":%.0f,\"procs\":[",
                                HZ, tsec(CLOCK_MONOTONIC) * 1000, dmn, s.on_bat, s.pw, s.pct, pkg, psys,
                                RMAX_PSYS > 0 ? RMAX_PSYS : RMAX_PKG);
    flags[0] = 0;
    int first = 1;
    DIR *d = opendir("/proc");
    struct dirent *e;
    char b[1024];
    while (d && (e = readdir(d))) {
      if (!isdigit((u8)e->d_name[0])) continue;
      int pid = atoi(e->d_name);
      snprintf(p, sizeof p, "/proc/%d/stat", pid);
      if (rd(p, b, sizeof b) <= 0) continue;
      char *lp = strchr(b, '('), *rp = strrchr(b, ')');
      if (!lp || !rp || !rp[1]) continue;
      *rp = 0;
      if (strcmp(lp + 1, bcomm)) continue;
      u64 f[25] = {0};
      char *q = rp + 3;
      for (int j = 4; j <= 24 && *q; j++) f[j] = strtoull(q, &q, 10);
      NC *c = NULL;
      for (int j = 0; j < nc; j++)
        if (cache[j].pid == pid && cache[j].start == f[22]) { c = &cache[j]; break; }
      if (!c) {
        c = &cache[nc < 512 ? nc++ : 511];
        memset(c, 0, sizeof *c);
        c->pid = pid;
        c->start = f[22];
        Proc tp;
        memset(&tp, 0, sizeof tp);
        tp.pid = pid;
        snprintf(tp.comm, sizeof tp.comm, "%s", bcomm);
        char key[64];
        NBROW = 1;
        snprintf(BROW[0], 16, "%s", bcomm);
        make_key(&tp, key, sizeof key, &c->cid, c->ty, sizeof c->ty);
        if (!c->ty[0]) snprintf(c->ty, sizeof c->ty, "?");
      }
      /* drapeaux utiles (gpu-process : features, angle…) */
      if (!flags[0] && !strcmp(c->ty, "gpu")) {
        char cl[8192];
        snprintf(p, sizeof p, "/proc/%d/cmdline", pid);
        ssize_t n = rd(p, cl, sizeof cl);
        size_t fl = 0;
        for (ssize_t j = 0; j < n; j++)
          if (!cl[j]) cl[j] = ' '; /* NUL ou espaces : même traitement */
        char *sv = NULL;
        for (char *a = n > 0 ? strtok_r(cl, " ", &sv) : NULL; a && fl < sizeof flags - 1; a = strtok_r(NULL, " ", &sv))
          if (!strncmp(a, "--enable-features=", 18) || !strncmp(a, "--disable-features=", 19) || !strncmp(a, "--use-", 6) ||
              !strncmp(a, "--ignore-gpu", 12) || !strncmp(a, "--enable-gpu", 12) || !strncmp(a, "--disable-gpu", 13))
            fl += (size_t)snprintf(flags + fl, sizeof flags - fl, "%s%s", fl ? " " : "", a);
        if (!flags[0]) snprintf(flags, sizeof flags, "-");
      }
      const char *m[3] = {"null", "null", "null"};
      for (int j = 0; j < nhist; j++)
        if (hist[j].pid == pid) {
          for (int w = 0; w < 3; w++) m[w] = strcmp(hist[j].m[w], "-") ? hist[j].m[w] : "null";
          break;
        }
      if (k + 300 >= sizeof out) break;
      k += (size_t)snprintf(out + k, sizeof out - k,
                            "%s{\"pid\":%d,\"cid\":%d,\"ty\":\"%s\",\"tk\":%llu,\"age\":%.1f,\"rss\":%llu,\"m1\":%s,\"m10\":%s,\"m60\":%s}",
                            first ? "" : ",", pid, c->cid, c->ty, f[14] + f[15], boot - (double)f[22] / HZ, f[24] * (u64)PAGEKB,
                            m[0], m[1], m[2]);
      first = 0;
    }
    if (d) closedir(d);
    char ef[1300];
    jesc(ef, sizeof ef, strcmp(flags, "-") ? flags : "");
    k += (size_t)snprintf(out + k, sizeof out - k, "],\"flags\":\"%s\"}", ef);
    if (k >= sizeof out) k = sizeof out - 1;
    u32 ol = (u32)k;
    writen(1, &ol, 4);
    writen(1, out, k);
  }
}

/* ================= main ================= */

static void usage(void) {
  puts("battrack " VERSION " — traqueur de conso ultra léger (batterie + secteur)\n"
       "  battrack report   rapport court (à coller à Claude)\n"
       "  battrack copy     rapport → presse-papier\n"
       "  battrack now      dernière minute\n"
       "  battrack reset    remet les compteurs à zéro\n"
       "  battrack bench    coût d'un échantillon + puissance instantanée\n"
       "  battrack daemon [-i SEC]   (lancé par systemd --user)");
}

int main(int argc, char **argv) {
  HZ = sysconf(_SC_CLK_TCK);
  PAGEKB = sysconf(_SC_PAGESIZE) / 1024;
  init_paths();
  init_browsers();
  const char *cmd = argc > 1 ? argv[1] : "help";
  if (!strncmp(cmd, "chrome-extension://", 19)) return nm_main();
  if (!strcmp(cmd, "daemon")) {
    if (argc > 3 && !strcmp(argv[2], "-i")) INTERVAL = atoi(argv[3]) > 0 ? atoi(argv[3]) : 60;
    discover();
    return daemon_main();
  }
  if (!strcmp(cmd, "report")) { report(stdout); return 0; }
  if (!strcmp(cmd, "copy")) {
    char *buf = NULL;
    size_t n = 0;
    FILE *m = open_memstream(&buf, &n);
    if (!m) return 1;
    report(m);
    fclose(m);
    fwrite(buf, 1, n, stdout);
    FILE *p = popen("wl-copy 2>/dev/null || xclip -selection clipboard 2>/dev/null", "w");
    if (p) { fwrite(buf, 1, n, p); pclose(p); }
    if (system("command -v notify-send >/dev/null && notify-send -a battrack -i battery 'Rapport batterie copié' "
               "'Colle-le dans Claude (Ctrl+V)' >/dev/null 2>&1") != 0) {}
    free(buf);
    return 0;
  }
  if (!strcmp(cmd, "now")) {
    char b[8192];
    if (rd(F_NOW, b, sizeof b) <= 0) { puts("pas de données (démon lancé ? systemctl --user status battrack)"); return 1; }
    fputs(b, stdout);
    return 0;
  }
  if (!strcmp(cmd, "reset")) {
    int pid = daemon_pid();
    if (pid) { kill(pid, SIGUSR2); puts("compteurs remis à zéro"); }
    else { unlink(F_STATE); puts("état effacé (démon arrêté)"); }
    return 0;
  }
  if (!strcmp(cmd, "bench")) {
    NOWRITE = 1;
    discover();
    apps_init();
    struct timespec a, b;
    step();
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &a);
    for (int i = 0; i < 20; i++) step();
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &b);
    double ms = ((b.tv_sec - a.tv_sec) * 1e3 + (b.tv_nsec - a.tv_nsec) / 1e6) / 20;
    int nd = 0;
    for (int i = 0; i < MAXDRM; i++) nd += D[i].used != 0;
    printf("1 échantillon = %.2f ms CPU · %d processus · %d applis · %d clients GPU suivis\n", ms, NPA, NA, nd);
    printf("à 1/min → %.4f%% d'un cœur\n", ms / 600.0);
    /* puissance instantanée sur 2 s */
    double p0 = *P_PKG ? rdnum(P_PKG, -1) : -1, s0 = *P_PSYS ? rdnum(P_PSYS, -1) : -1, t0 = tsec(CLOCK_MONOTONIC);
    struct timespec w = {2, 0};
    nanosleep(&w, NULL);
    double p1 = *P_PKG ? rdnum(P_PKG, -1) : -1, s1 = *P_PSYS ? rdnum(P_PSYS, -1) : -1, dt = tsec(CLOCK_MONOTONIC) - t0;
    double jp = rapl_d(p1, p0, RMAX_PKG), js = rapl_d(s1, s0, RMAX_PSYS);
    Sys s;
    memset(&s, 0, sizeof s);
    read_bat(&s);
    char ma[64], mo[48];
    read_mode(mo, sizeof mo);
    mode_abbr(mo, ma, sizeof ma);
    printf("puissance (2 s) · batterie %.2f W (%s) · mode %s\n", s.pw, s.st, ma);
    if (jp >= 0) printf("  pkg  = %.2f W\n", jp / dt);
    if (js >= 0) printf("  psys = %.2f W\n", js / dt);
    if (jp < 0 && js < 0) printf("  RAPL illisible (règle udev ?)\n");
    return 0;
  }
  usage();
  return 0;
}
