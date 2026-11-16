// SPDX-License-Identifier: MIT
// meow-thermald: thermal supervisor of the dongle. Started by init (rootfs/thermald-session),
// independent of the UI and of appd, so it works when either has crashed or hangs.
//
// What it does (numbers are the defaults; README.md "Thermal protection" has the reasons):
//   - reads the SoC sensor every 3 s (every 1 s from 95 C up);
//   - throttle levels above the kernel's passive trips, with 3 C hysteresis:
//       0 normal, 1 warm (>= 90 C), 2 hot (>= 100 C), 3 severe (>= 104 C);
//     each change runs `thermal-actions level N` and is published in /run/thermal/state,
//     which the launcher reads (it stops optional work);
//   - emergency cooldown at 108 C (two samples in a row) or 111 C (one sample), instead of the
//     kernel's critical trip rebooting the device:
//       prepare (close the app in front) -> notice on screen for 5 s -> `thermal-actions enter`
//       (stop apps, appd and the UI, lowest clocks, video output off) -> 60 s rounds ->
//       at the end of a round: <= 97 C -> `thermal-actions exit` and a notice; otherwise
//       another round. After 5 rounds, or when a round ends more than 2 C hotter than it
//       began, or at 113 C at any moment: `fail_cmd` (sync; reboot -f), as the kernel would.
//   - /run/thermal/cooldown exists while the output is off (the session scripts refuse to
//     start the UI or appd then; a restarted daemon continues the cooldown).
//
// Testing without heat: `echo 109000 > /run/thermal/emul_temp` (the daemon uses the higher of
// the sensor and this file, so emulation can never hide real heat), and/or other thresholds
// in a config file. Config: key=value lines in /etc/meow/thermal.conf, then /data/thermal.conf
// (or the files given with -c), then THERMAL_<KEY> in the environment. SIGHUP re-reads it.
//
//   meow-thermald [-c FILE]... [-n] [-f]     -n: log actions instead of running them
//                                            -f: stay in the foreground, log to stderr too
#define _GNU_SOURCE
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PATH_LEN 160

struct cfg {
    char sensor[PATH_LEN];
    char run_dir[PATH_LEN];
    char actions[PATH_LEN];
    char persist_log[PATH_LEN];
    char fail_cmd[PATH_LEN];
    long warm, hot, severe, level_hyst;
    long emergency, emergency_now, resume, panic, rise_abort;
    long poll_ms, poll_hot_ms, fast_below;
    long confirm, notice_s, cooldown_s, max_rounds, settle_s, action_timeout_s, log_every_s;
    long offline_cores; /* 1: thermal-actions may take CPUs offline (level 3: two, cooldown: three) */
};

static struct cfg cfg;
static const char *conf_files[8] = { "/etc/meow/thermal.conf", "/data/thermal.conf" };
static int conf_count = 2;
static int dry_run, foreground;
static volatile sig_atomic_t got_term, got_hup;

static void defaults(struct cfg *c)
{
    memset(c, 0, sizeof *c);
    strcpy(c->sensor, "/sys/class/thermal/thermal_zone0/temp");
    strcpy(c->run_dir, "/run/thermal");
    strcpy(c->actions, "/bin/thermal-actions");
    strcpy(c->persist_log, "/data/thermal.log");
    strcpy(c->fail_cmd, "sync; reboot -f");
    c->warm = 90000;
    c->hot = 100000;
    c->severe = 104000;
    c->level_hyst = 3000;
    c->emergency = 108000;
    c->emergency_now = 111000;
    c->resume = 97000;
    c->panic = 113000;
    c->rise_abort = 2000;
    c->poll_ms = 3000;
    c->poll_hot_ms = 1000;
    c->fast_below = 13000; /* poll fast within this distance of the emergency threshold */
    c->confirm = 2;
    c->notice_s = 5;
    c->cooldown_s = 60;
    c->max_rounds = 5;
    c->settle_s = 7;
    c->action_timeout_s = 45;
    c->log_every_s = 60;
}

static double mono(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void path(char *out, const char *name)
{
    snprintf(out, PATH_LEN + 32, "%s/%s", cfg.run_dir, name);
}

static void append(const char *file, const char *line, long cap)
{
    struct stat st;
    if (cap > 0 && stat(file, &st) == 0 && st.st_size > cap) {
        char old[PATH_LEN + 40];
        snprintf(old, sizeof old, "%s.1", file);
        rename(file, old);
    }
    int fd = open(file, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
    if (fd < 0)
        return;
    if (write(fd, line, strlen(line)) < 0) { /* nothing to do about it */ }
    close(fd);
}

/* persist: also to the log on flash (emergency transitions only: a few lines per event). */
__attribute__((format(printf, 2, 3)))
static void logf_(int persist, const char *fmt, ...)
{
    char msg[400], line[480], file[PATH_LEN + 32], stamp[32];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(msg, sizeof msg, fmt, ap);
    va_end(ap);
    time_t now = time(NULL);
    struct tm tm;
    gmtime_r(&now, &tm);
    strftime(stamp, sizeof stamp, "%Y-%m-%dT%H:%M:%SZ", &tm);
    snprintf(line, sizeof line, "[%10.1f %s] %s\n", mono(), stamp, msg);
    path(file, "log");
    append(file, line, 64 * 1024);
    if (persist && cfg.persist_log[0])
        append(cfg.persist_log, line, 16 * 1024);
    if (foreground)
        fputs(line, stderr);
}

static int set_key(struct cfg *c, const char *k, const char *v)
{
    struct { const char *name; char *s; } strs[] = {
        { "sensor", c->sensor }, { "run_dir", c->run_dir }, { "actions", c->actions },
        { "persist_log", c->persist_log }, { "fail_cmd", c->fail_cmd },
    };
    struct { const char *name; long *n; } nums[] = {
        { "warm", &c->warm }, { "hot", &c->hot }, { "severe", &c->severe }, { "level_hyst", &c->level_hyst },
        { "emergency", &c->emergency }, { "emergency_now", &c->emergency_now }, { "resume", &c->resume },
        { "panic", &c->panic }, { "rise_abort", &c->rise_abort }, { "poll_ms", &c->poll_ms },
        { "poll_hot_ms", &c->poll_hot_ms }, { "fast_below", &c->fast_below }, { "confirm", &c->confirm },
        { "notice_s", &c->notice_s }, { "cooldown_s", &c->cooldown_s }, { "max_rounds", &c->max_rounds },
        { "settle_s", &c->settle_s }, { "action_timeout_s", &c->action_timeout_s }, { "log_every_s", &c->log_every_s },
        { "offline_cores", &c->offline_cores },
    };
    for (size_t i = 0; i < sizeof strs / sizeof *strs; i++)
        if (!strcmp(k, strs[i].name)) {
            snprintf(strs[i].s, PATH_LEN, "%s", v);
            return 1;
        }
    for (size_t i = 0; i < sizeof nums / sizeof *nums; i++)
        if (!strcmp(k, nums[i].name)) {
            char *end;
            long n = strtol(v, &end, 10);
            if (end == v || *end)
                return 0;
            *nums[i].n = n;
            return 1;
        }
    return 0;
}

static char *trim(char *s)
{
    while (isspace((unsigned char)*s))
        s++;
    char *e = s + strlen(s);
    while (e > s && isspace((unsigned char)e[-1]))
        *--e = 0;
    return s;
}

/* Returns what to complain about once the log is usable (run_dir may come from the config). */
static void load_config(char *warn, size_t warn_len)
{
    struct cfg c;
    defaults(&c);
    warn[0] = 0;
    for (int i = 0; i < conf_count; i++) {
        FILE *f = fopen(conf_files[i], "r");
        if (!f)
            continue;
        char line[320];
        while (fgets(line, sizeof line, f)) {
            char *s = trim(line);
            if (!*s || *s == '#')
                continue;
            char *eq = strchr(s, '=');
            if (!eq || (*eq = 0, !set_key(&c, trim(s), trim(eq + 1))))
                snprintf(warn, warn_len, "%s: bad line \"%s\"", conf_files[i], s);
        }
        fclose(f);
    }
    extern char **environ;
    for (char **e = environ; *e; e++) {
        if (strncmp(*e, "THERMAL_", 8))
            continue;
        char k[64];
        const char *eq = strchr(*e, '=');
        size_t n = eq ? (size_t)(eq - *e - 8) : 0;
        if (!eq || n == 0 || n >= sizeof k)
            continue;
        for (size_t i = 0; i < n; i++)
            k[i] = (char)tolower((unsigned char)(*e)[8 + i]);
        k[n] = 0;
        if (!set_key(&c, k, eq + 1))
            snprintf(warn, warn_len, "environment: bad %s", *e);
    }
    /* A config that cannot work would leave the device without the cooldown: refuse it. */
    struct cfg d;
    defaults(&d);
    if (!(c.resume < c.emergency && c.emergency <= c.emergency_now && c.emergency < c.panic
          && c.warm <= c.hot && c.hot <= c.severe && c.level_hyst >= 0 && c.confirm >= 1
          && c.poll_ms >= 50 && c.poll_hot_ms >= 50 && c.cooldown_s >= 1 && c.max_rounds >= 1
          && c.notice_s >= 0 && c.settle_s >= 0 && c.action_timeout_s >= 1 && c.log_every_s >= 1)) {
        snprintf(warn, warn_len, "inconsistent thresholds in the config: using the defaults");
        snprintf(d.sensor, PATH_LEN, "%s", c.sensor);
        snprintf(d.run_dir, PATH_LEN, "%s", c.run_dir);
        snprintf(d.actions, PATH_LEN, "%s", c.actions);
        snprintf(d.persist_log, PATH_LEN, "%s", c.persist_log);
        snprintf(d.fail_cmd, PATH_LEN, "%s", c.fail_cmd);
        c = d;
    }
    cfg = c;
}

static long read_long(const char *file)
{
    char buf[32];
    int fd = open(file, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0)
        return -1;
    buf[n] = 0;
    char *end;
    long v = strtol(buf, &end, 10);
    return end == buf ? -1 : v;
}

static long last_real = -1;
static int sensor_failures;

/* Millidegrees, or -1 when the sensor cannot be read. The emulation file can only raise it. */
static long read_temp(void)
{
    char file[PATH_LEN + 32];
    long real = read_long(cfg.sensor);
    /* The TSADC reads garbage now and then on some SoCs: ignore impossible values. */
    if (real < -40000 || real > 200000)
        real = -1;
    last_real = real;
    if (real < 0) {
        if (sensor_failures++ % 60 == 0)
            logf_(0, "SENSOR unreadable %s", cfg.sensor);
    } else {
        sensor_failures = 0;
    }
    path(file, "emul_temp");
    long emul = read_long(file);
    if (emul > 0 && emul > real)
        return emul;
    return real;
}

/* A reading at or above `limit` that a second look 250 ms later confirms. The sensor (or a
   burst of load next to it) shows single samples several degrees off: seen 93 C between 86 C
   readings on 15 Nov. One sample must not blank the screen or reboot the device. */
static int still_over(long limit)
{
    struct timespec ts = { 0, 250 * 1000000L };
    nanosleep(&ts, NULL);
    return read_temp() >= limit;
}

static const char *state = "normal";
static int level, round_no;
static unsigned notice_seq;
static char notice_text[120];

static void write_state(long temp)
{
    char file[PATH_LEN + 32], tmp[PATH_LEN + 40];
    path(file, "state");
    snprintf(tmp, sizeof tmp, "%s.new", file);
    FILE *f = fopen(tmp, "w");
    if (!f)
        return;
    fprintf(f, "state=%s\nlevel=%d\ntemp=%ld\nround=%d\nnotice_seq=%u\nnotice=%s\nend=1\n", state, level, temp, round_no, notice_seq, notice_text);
    fclose(f);
    rename(tmp, file);
}

static void notice(const char *text, long temp)
{
    notice_seq++;
    snprintf(notice_text, sizeof notice_text, "%s", text);
    write_state(temp);
}

/* Sleeps unless a signal arrives. */
static void nap_ms(long ms)
{
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

/* Runs `actions VERB [ARG]` with a time limit; the script's output goes to the log. */
static int run_action(const char *verb, const char *arg)
{
    if (dry_run) {
        logf_(0, "ACTION %s %s (dry run)", verb, arg ? arg : "");
        return 0;
    }
    char file[PATH_LEN + 32];
    path(file, "log");
    double t0 = mono();
    pid_t pid = fork();
    if (pid < 0) {
        logf_(0, "ACTION %s: fork: %s", verb, strerror(errno));
        return -1;
    }
    if (pid == 0) {
        setsid();
        signal(SIGTERM, SIG_DFL);
        signal(SIGHUP, SIG_DFL);
        int fd = open(file, O_WRONLY | O_CREAT | O_APPEND, 0644);
        int nul = open("/dev/null", O_RDONLY);
        if (nul >= 0)
            dup2(nul, 0);
        if (fd >= 0) {
            dup2(fd, 1);
            dup2(fd, 2);
        }
        setenv("THERMAL_RUN_DIR", cfg.run_dir, 1);
        setenv("THERMAL_OFFLINE_CORES", cfg.offline_cores ? "1" : "0", 1);
        execl("/bin/sh", "sh", cfg.actions, verb, arg, (char *)NULL);
        _exit(127);
    }
    int status = 0;
    for (;;) {
        pid_t r = waitpid(pid, &status, WNOHANG);
        if (r == pid)
            break;
        if (r < 0 && errno != EINTR) {
            status = -1;
            break;
        }
        if (mono() - t0 > cfg.action_timeout_s) {
            kill(-pid, SIGKILL);
            kill(pid, SIGKILL);
            waitpid(pid, &status, 0);
            logf_(1, "ACTION %s %s: killed after %ld s", verb, arg ? arg : "", cfg.action_timeout_s);
            return -1;
        }
        nap_ms(50);
    }
    int rc = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    logf_(0, "ACTION %s %s: rc=%d in %.1f s", verb, arg ? arg : "", rc, mono() - t0);
    return rc;
}

static void marker(int on)
{
    char file[PATH_LEN + 32];
    path(file, "cooldown");
    if (on) {
        int fd = open(file, O_WRONLY | O_CREAT | O_CLOEXEC, 0644);
        if (fd >= 0)
            close(fd);
    } else {
        unlink(file);
    }
}

static int marker_present(void)
{
    char file[PATH_LEN + 32];
    path(file, "cooldown");
    return access(file, F_OK) == 0;
}

#define C(t) ((t) / 1000), (labs(t) % 1000 / 100)

static void set_level(int next, long temp)
{
    if (next == level)
        return;
    char arg[16];
    snprintf(arg, sizeof arg, "%d", next);
    logf_(0, "LEVEL %d -> %d temp=%ld.%ld", level, next, C(temp));
    int was = level;
    level = next;
    write_state(temp);
    run_action("level", arg);
    if (next == 3 && was < 3 && temp < cfg.emergency) /* (the emergency has its own notice) */
        notice("The device is hot: slowing down", temp);
}

static void update_level(long t)
{
    const long up[4] = { 0, cfg.warm, cfg.hot, cfg.severe };
    int next = level;
    while (next < 3 && t >= up[next + 1])
        next++;
    while (next > 0 && t < up[next] - cfg.level_hyst)
        next--;
    set_level(next, t);
}

static void leave_cooldown(long t, const char *why)
{
    logf_(1, "COOLDOWN exit (%s) temp=%ld.%ld after %d round(s)", why, C(t), round_no);
    state = "resuming";
    write_state(t);
    marker(0);
    run_action("exit", NULL);
    round_no = 0;
    state = "normal";
    write_state(t);
}

static void fail(long t, const char *why)
{
    logf_(1, "COOLDOWN failed: %s; temp=%ld.%ld round=%d: %s", why, C(t), round_no, cfg.fail_cmd);
    state = "failed";
    write_state(t);
    run_action("fail", NULL);
    if (dry_run) {
        logf_(0, "FAIL_CMD %s (dry run)", cfg.fail_cmd);
        exit(3);
    }
    /* The marker stays: if the command does not end the system, nothing starts the UI. */
    execl("/bin/sh", "sh", "-c", cfg.fail_cmd, (char *)NULL);
    exit(3);
}

/* The output is off (or about to be). Returns when the device has cooled down; never when not. */
static void cooldown(long entry_temp)
{
    long start = entry_temp;
    state = "cooldown";
    round_no = 1;
    marker(1);
    write_state(entry_temp);
    run_action("enter", NULL);
    for (;;) {
        double end = mono() + cfg.cooldown_s;
        long t = start, peak = -1;
        while (mono() < end) {
            if (got_term) {
                leave_cooldown(t, "terminated");
                exit(0);
            }
            double left = end - mono();
            nap_ms(left > 5 ? 5000 : (long)(left * 1000) + 1);
            long s = read_temp();
            if (s < 0)
                continue;
            t = s;
            if (t > peak)
                peak = t;
            if (t >= cfg.panic && still_over(cfg.panic))
                fail(t, "panic temperature with everything off");
            write_state(t);
        }
        long s = read_temp();
        if (s >= 0)
            t = s;
        logf_(1, "COOLDOWN round %d/%ld: start=%ld.%ld end=%ld.%ld peak=%ld.%ld resume<=%ld.%ld", round_no, cfg.max_rounds, C(start), C(t), C(peak < 0 ? t : peak), C(cfg.resume));
        if (t <= cfg.resume)
            break;
        if (t > start + cfg.rise_abort)
            fail(t, "still heating up with everything off");
        if (round_no >= cfg.max_rounds)
            fail(t, "not cool enough after the last round");
        round_no++;
        start = t;
        write_state(t);
    }
    long t = read_temp();
    leave_cooldown(t, "cooled down");
    /* The TV needs a few seconds to show a picture again; then say what happened. */
    for (long i = 0; i < cfg.settle_s * 2 && !got_term; i++)
        nap_ms(500);
    notice("Cooled down: back to normal", t);
}

static void emergency(long t, const char *why)
{
    logf_(1, "EMERGENCY %s: temp=%ld.%ld (sensor %ld) level=%d; notice, then cooldown", why, C(t), last_real, level);
    state = "notice";
    marker(1); /* from here nothing new may start */
    write_state(t);
    run_action("prepare", "Too hot: pausing for a minute to cool down");
    notice("Too hot: pausing for a minute to cool down", t);
    for (long i = 0; i < cfg.notice_s * 4 && !got_term; i++)
        nap_ms(250);
    long now = read_temp();
    cooldown(now >= 0 ? now : t);
}

static void on_signal(int sig)
{
    if (sig == SIGHUP)
        got_hup = 1;
    else
        got_term = 1;
}

int main(int argc, char **argv)
{
    int custom = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-n")) {
            dry_run = 1;
        } else if (!strcmp(argv[i], "-f")) {
            foreground = 1;
        } else if (!strcmp(argv[i], "-c") && i + 1 < argc && custom < 8) {
            if (!custom)
                conf_count = 0;
            conf_files[custom++] = argv[++i];
            conf_count = custom;
        } else {
            fprintf(stderr, "usage: meow-thermald [-c FILE]... [-n] [-f]\n");
            return 2;
        }
    }
    char warn[400];
    load_config(warn, sizeof warn);
    mkdir(cfg.run_dir, 0755);
    if (warn[0])
        logf_(0, "CONFIG %s", warn);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGHUP, &sa, NULL);
    signal(SIGPIPE, SIG_IGN);
    /* Never the OOM killer's choice: this is what protects the hardware. */
    int fd = open("/proc/self/oom_score_adj", O_WRONLY | O_CLOEXEC);
    if (fd >= 0) {
        if (write(fd, "-1000", 5) < 0) { /* not fatal */ }
        close(fd);
    }
    logf_(0, "START pid=%d emergency=%ld/%ld resume=%ld panic=%ld levels=%ld/%ld/%ld hyst=%ld cooldown=%lds x%ld%s", (int)getpid(), cfg.emergency, cfg.emergency_now, cfg.resume,
          cfg.panic, cfg.warm, cfg.hot, cfg.severe, cfg.level_hyst, cfg.cooldown_s, cfg.max_rounds, dry_run ? " dry-run" : "");

    long t = read_temp();
    if (marker_present()) {
        /* A previous instance died while the output was off: carry on from there. */
        logf_(1, "START with a cooldown in progress: continuing it, temp=%ld.%ld", C(t));
        cooldown(t);
    }
    write_state(t);

    int over = 0;
    double log_at = mono() + cfg.log_every_s;
    long lo = t, hi = t;
    for (;;) {
        if (got_term) {
            logf_(0, "STOP temp=%ld.%ld level=%d", C(t), level);
            set_level(0, t);
            return 0;
        }
        if (got_hup) {
            got_hup = 0;
            load_config(warn, sizeof warn);
            logf_(0, "CONFIG reloaded%s%s emergency=%ld resume=%ld", warn[0] ? ": " : "", warn, cfg.emergency, cfg.resume);
        }
        long s = read_temp();
        if (s >= 0) {
            t = s;
            if (t < lo || lo < 0)
                lo = t;
            if (t > hi)
                hi = t;
            if (t >= cfg.emergency_now && still_over(cfg.emergency_now)) {
                emergency(t, "at once");
                over = 0;
                lo = hi = t = read_temp();
            } else if (t >= cfg.emergency) {
                if (++over >= cfg.confirm) {
                    emergency(t, "confirmed");
                    over = 0;
                    lo = hi = t = read_temp();
                }
            } else {
                over = 0;
            }
            if (t >= 0)
                update_level(t);
        }
        if (mono() >= log_at) {
            logf_(0, "TEMP now=%ld.%ld min=%ld.%ld max=%ld.%ld level=%d", C(t), C(lo), C(hi), level);
            write_state(t);
            if (level >= 2) { /* again: the apps' cgroup may be new (appd restarted) */
                char arg[16];
                snprintf(arg, sizeof arg, "%d", level);
                run_action("level", arg);
            }
            log_at = mono() + cfg.log_every_s;
            lo = hi = t;
        }
        nap_ms(t >= cfg.emergency - cfg.fast_below ? cfg.poll_hot_ms : cfg.poll_ms);
    }
}
