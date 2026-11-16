// SPDX-License-Identifier: MIT
// meow-buttond: the dongle's one hardware button while the firmware runs. Started by init
// (rootfs/buttond-session), independent of the launcher, appd and the network daemon, so a
// factory reset works when any of them hangs and when there is no picture at all.
//
// The button is GPIO3_D1, active low: the pin the stock miniloader samples at power-on to
// enter MaskROM. That use is untouched; this daemon only reads the line once Linux is up.
//
// One button, two actions, told apart by what the finger does (README.md "The button"):
//
//   press and let go within 1 s      -> "Pair a phone": the TV shows the pairing code (or the
//                                       Wi-Fi setup codes when it has no network). Pressing
//                                       again hides it. Harmless: pairing still needs the code
//                                       that only the TV shows.
//   hold                             -> nothing for 3 s, then the TV shows a factory-reset
//                                       warning with a countdown. Letting go during the
//                                       countdown cancels.
//   keep holding until 10 s          -> armed: "let go now to erase".
//   let go between 10 s and 15 s     -> factory reset (rootfs/factory-reset).
//   still held at 15 s               -> cancelled; the button is ignored until it is let go.
//
// Why the reset happens on letting go inside a window, and never while the button is down:
//   - a button that is stuck or squeezed (the dongle wedged behind a TV) is down for good and
//     can never reset the device, at boot or later;
//   - a knock, or a long press out of impatience, ends before 10 s and cancels;
//   - it needs no second key, so it works when no remote is paired, which is exactly when a
//     reset is needed. Without a picture: hold, count to twelve, let go.
// A button that is already down when the daemon starts is ignored until it is let go once.
//
// The launcher only draws: each step is one byte on its key FIFO (/run/ui-keys; the bytes are
// not key names, so a phone remote cannot send them). The timing is this daemon's alone.
//   '1' short press   '2' warning starts   '3' armed   '4' cancelled (let go early)
//   '5' cancelled (held too long)          '6' erasing
//
//   meow-buttond [-f] [-n]      -f: foreground, log to stderr too   -n: log the reset instead
//                               of running it
//   meow-buttond probe [SEC]    print the line's level changes for SEC seconds (default 10)
//   meow-buttond erase DEV      erase a whole MTD device block by block (factory-reset uses it
//                               on /dev/mtd0, which is the data partition and nothing else)
//   meow-buttond selftest       the timing rules, without hardware
// Environment: BUTTON_CHIP (/dev/gpiochip3), BUTTON_LINE (25), BUTTON_KEYS (/run/ui-keys),
// BUTTON_RESET_CMD (/bin/factory-reset button), BUTTON_SHORT_MS / WARN_MS / ARM_MS / LATE_MS.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <linux/gpio.h>
#include <mtd/mtd-user.h>

#define LOG "/run/button.log"

// ---- the timing rules (no hardware below this line until "hardware") --------------------

struct rules {
    long short_ms; /* a press shorter than this is a short press */
    long warn_ms;  /* held this long: the warning starts */
    long arm_ms;   /* held this long: armed */
    long late_ms;  /* still held: cancelled */
};

enum state { ST_BLOCKED, ST_IDLE, ST_DOWN, ST_WARN, ST_ARMED };
enum event { EV_NONE, EV_SHORT, EV_WARN, EV_ARMED, EV_CANCEL, EV_LATE, EV_RESET };

struct machine {
    enum state st;
    long long down_at;
};

/* The machine starts blocked: a button that is down at start must be let go first. */
static void machine_init(struct machine *m) {
    m->st = ST_BLOCKED;
    m->down_at = 0;
}

/* One observation of the button at time `now` (ms). At most one event per call; the caller
 * calls again with the same input until EV_NONE (several thresholds may have passed). */
static enum event machine_step(struct machine *m, const struct rules *r, int pressed, long long now) {
    long long held = now - m->down_at;
    switch (m->st) {
    case ST_BLOCKED:
        if (!pressed) m->st = ST_IDLE;
        return EV_NONE;
    case ST_IDLE:
        if (pressed) {
            m->st = ST_DOWN;
            m->down_at = now;
        }
        return EV_NONE;
    case ST_DOWN:
        if (!pressed) {
            m->st = ST_IDLE;
            return held < r->short_ms ? EV_SHORT : EV_NONE;
        }
        if (held >= r->warn_ms) {
            m->st = ST_WARN;
            return EV_WARN;
        }
        return EV_NONE;
    case ST_WARN:
        /* Let go wins over a threshold that passed unobserved: never reset on doubt. */
        if (!pressed) {
            m->st = ST_IDLE;
            return EV_CANCEL;
        }
        if (held >= r->arm_ms) {
            m->st = ST_ARMED;
            return EV_ARMED;
        }
        return EV_NONE;
    case ST_ARMED:
        if (held >= r->late_ms) {
            /* Also when it was let go just now but we saw it too late. */
            m->st = pressed ? ST_BLOCKED : ST_IDLE;
            return EV_LATE;
        }
        if (!pressed) {
            m->st = ST_IDLE;
            return EV_RESET;
        }
        return EV_NONE;
    }
    return EV_NONE;
}

/* How long the daemon may sleep before the next threshold (-1: until the line changes). */
static long machine_timeout(const struct machine *m, const struct rules *r, long long now) {
    long long at;
    switch (m->st) {
    case ST_DOWN: at = m->down_at + r->warn_ms; break;
    case ST_WARN: at = m->down_at + r->arm_ms; break;
    case ST_ARMED: at = m->down_at + r->late_ms; break;
    default: return -1;
    }
    return at > now ? (long)(at - now) : 0;
}

static int selftest(void) {
    static const struct rules r = { 1000, 3000, 10000, 15000 };
    /* time, pressed -> expected event; each script starts from a fresh machine */
    struct obs { long long t; int pressed; enum event want; };
    static const struct obs short_press[] = { {0, 0, EV_NONE}, {100, 1, EV_NONE}, {400, 0, EV_SHORT}, {-1, 0, 0} };
    static const struct obs medium[] = { {0, 0, EV_NONE}, {100, 1, EV_NONE}, {1600, 0, EV_NONE}, {-1, 0, 0} };
    static const struct obs early[] = { {0, 0, EV_NONE}, {0, 1, EV_NONE}, {3000, 1, EV_WARN}, {9999, 0, EV_CANCEL}, {-1, 0, 0} };
    static const struct obs reset[] = { {0, 0, EV_NONE}, {0, 1, EV_NONE}, {3000, 1, EV_WARN}, {10000, 1, EV_ARMED}, {12000, 0, EV_RESET}, {12100, 0, EV_NONE}, {-1, 0, 0} };
    static const struct obs late[] = { {0, 0, EV_NONE}, {0, 1, EV_NONE}, {3000, 1, EV_WARN}, {10000, 1, EV_ARMED}, {15000, 1, EV_LATE}, {99000, 1, EV_NONE}, {99500, 0, EV_NONE}, {99600, 1, EV_NONE}, {99700, 0, EV_SHORT}, {-1, 0, 0} };
    static const struct obs stuck_at_boot[] = { {0, 1, EV_NONE}, {12000, 1, EV_NONE}, {12001, 0, EV_NONE}, {20000, 0, EV_NONE}, {-1, 0, 0} };
    /* The daemon slept through everything (stopped, starved): seen only after 60 s. */
    static const struct obs overslept_down[] = { {0, 0, EV_NONE}, {0, 1, EV_NONE}, {60000, 1, EV_WARN}, {60000, 1, EV_ARMED}, {60000, 1, EV_LATE}, {60000, 1, EV_NONE}, {-1, 0, 0} };
    static const struct obs overslept_up[] = { {0, 0, EV_NONE}, {0, 1, EV_NONE}, {3000, 1, EV_WARN}, {60000, 0, EV_CANCEL}, {-1, 0, 0} };
    static const struct obs armed_overslept_up[] = { {0, 0, EV_NONE}, {0, 1, EV_NONE}, {3000, 1, EV_WARN}, {10000, 1, EV_ARMED}, {15000, 0, EV_LATE}, {15001, 0, EV_NONE}, {-1, 0, 0} };
    static const struct { const char *name; const struct obs *o; } scripts[] = {
        {"short press", short_press}, {"1.5 s press does nothing", medium}, {"let go during the warning", early},
        {"reset", reset}, {"held too long", late}, {"down at start", stuck_at_boot},
        {"overslept, still down", overslept_down}, {"overslept, let go", overslept_up},
        {"armed, let go seen too late", armed_overslept_up},
    };
    int bad = 0;
    for (unsigned i = 0; i < sizeof scripts / sizeof scripts[0]; i++) {
        struct machine m;
        machine_init(&m);
        for (const struct obs *o = scripts[i].o; o->t >= 0; o++) {
            enum event got = machine_step(&m, &r, o->pressed, o->t);
            if (got != o->want) {
                printf("FAIL %s: t=%lld pressed=%d: event %d, expected %d\n", scripts[i].name, o->t, o->pressed, got, o->want);
                bad++;
            }
        }
    }
    /* Whatever the sampling, a reset needs a let-go observed inside [arm, late). */
    for (long step = 7; step <= 4001; step += 13) {
        for (long release = 0; release <= 20000; release += 250) {
            struct machine m;
            machine_init(&m);
            machine_step(&m, &r, 0, 0);
            int got_reset = 0;
            long long reset_at = 0;
            for (long long t = 1; t < 24000; t += step) {
                int pressed = t >= 1 && t < 1 + release;
                enum event e;
                while ((e = machine_step(&m, &r, pressed, t)) != EV_NONE) {
                    if (e == EV_RESET) {
                        got_reset = 1;
                        reset_at = t;
                    }
                }
            }
            if (got_reset && (release < r.arm_ms || reset_at - 1 >= r.late_ms)) {
                printf("FAIL sweep: step %ld release %ld reset at %lld\n", step, release, reset_at);
                bad++;
            }
        }
    }
    printf(bad ? "BUTTON_SELFTEST failed (%d)\n" : "BUTTON_SELFTEST ok\n", bad);
    return bad ? 1 : 0;
}

// ---- hardware -----------------------------------------------------------------------------

static int foreground;

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void logf_(const char *fmt, ...) {
    char line[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    struct stat sb;
    if (stat(LOG, &sb) == 0 && sb.st_size > 65536) rename(LOG, LOG ".1");
    FILE *f = fopen(LOG, "a");
    if (f) {
        fprintf(f, "[%lld.%03lld] %s\n", now_ms() / 1000, now_ms() % 1000, line);
        fclose(f);
    }
    if (foreground) fprintf(stderr, "%s\n", line);
}

static const char *env_or(const char *name, const char *def) {
    const char *v = getenv(name);
    return v && *v ? v : def;
}

static long env_ms(const char *name, long def) {
    const char *v = getenv(name);
    long n = v ? atol(v) : 0;
    return n > 0 ? n : def;
}

/* The button as an input with both edges, active low (1 = pressed). The pad's pull-up is
 * asked for; a kernel that cannot set it still gets the line (the board has its own). */
static int open_line(void) {
    const char *chip = env_or("BUTTON_CHIP", "/dev/gpiochip3");
    int line = atoi(env_or("BUTTON_LINE", "25")); /* GPIO3_D1 = 3 * 8 + 1 */
    int cfd = open(chip, O_RDONLY | O_CLOEXEC);
    if (cfd < 0) {
        logf_("BUTTON %s: %s", chip, strerror(errno));
        return -1;
    }
    for (int attempt = 0; attempt < 2; attempt++) {
        struct gpio_v2_line_request req;
        memset(&req, 0, sizeof req);
        req.offsets[0] = line;
        req.num_lines = 1;
        snprintf(req.consumer, sizeof req.consumer, "meow-button");
        req.config.flags = GPIO_V2_LINE_FLAG_INPUT | GPIO_V2_LINE_FLAG_ACTIVE_LOW | GPIO_V2_LINE_FLAG_EDGE_RISING | GPIO_V2_LINE_FLAG_EDGE_FALLING;
        if (attempt == 0) req.config.flags |= GPIO_V2_LINE_FLAG_BIAS_PULL_UP;
        req.config.num_attrs = 1;
        req.config.attrs[0].mask = 1;
        req.config.attrs[0].attr.id = GPIO_V2_LINE_ATTR_ID_DEBOUNCE;
        req.config.attrs[0].attr.debounce_period_us = 30000;
        if (ioctl(cfd, GPIO_V2_GET_LINE_IOCTL, &req) == 0) {
            close(cfd);
            logf_("BUTTON %s line %d%s", chip, line, attempt ? " (no pull-up setting)" : "");
            return req.fd;
        }
        logf_("BUTTON %s line %d request%s: %s", chip, line, attempt ? "" : " with pull-up", strerror(errno));
    }
    close(cfd);
    return -1;
}

static int read_line(int fd) {
    struct gpio_v2_line_values v = { .bits = 0, .mask = 1 };
    if (ioctl(fd, GPIO_V2_LINE_GET_VALUES_IOCTL, &v) < 0) return -1;
    return (int)(v.bits & 1);
}

static void drain(int fd) {
    struct gpio_v2_line_event ev[16];
    struct pollfd p = { .fd = fd, .events = POLLIN };
    while (poll(&p, 1, 0) > 0 && (p.revents & POLLIN)) {
        if (read(fd, ev, sizeof ev) <= 0) break;
    }
}

/* One byte to the launcher. No launcher (or one that is not reading): nothing is lost that
 * matters, the daemon acts by itself. */
static void tell_ui(char c) {
    int fd = open(env_or("BUTTON_KEYS", "/run/ui-keys"), O_WRONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) return;
    if (write(fd, &c, 1) < 0) logf_("BUTTON ui: %s", strerror(errno));
    close(fd);
}

static void run_reset(int dry) {
    const char *cmd = env_or("BUTTON_RESET_CMD", "/bin/factory-reset button");
    if (dry) {
        logf_("BUTTON reset (dry run): would run %s", cmd);
        return;
    }
    logf_("BUTTON reset: %s", cmd);
    pid_t pid = fork();
    if (pid == 0) {
        setsid();
        execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        _exit(127);
    }
    int status = 0;
    if (pid > 0) waitpid(pid, &status, 0);
    logf_("BUTTON reset finished: status %d", pid > 0 && WIFEXITED(status) ? WEXITSTATUS(status) : -1);
}

static int daemon_main(int dry) {
    struct rules r = { env_ms("BUTTON_SHORT_MS", 1000), env_ms("BUTTON_WARN_MS", 3000), env_ms("BUTTON_ARM_MS", 10000), env_ms("BUTTON_LATE_MS", 15000) };
    if (!(r.short_ms <= r.warn_ms && r.warn_ms < r.arm_ms && r.arm_ms < r.late_ms)) {
        logf_("BUTTON bad timing in the environment");
        return 2;
    }
    int fd = open_line();
    if (fd < 0) return 1;
    struct machine m;
    machine_init(&m);
    int last = read_line(fd);
    logf_("BUTTON started: %s; short < %ld ms, warning %ld, armed %ld, too long %ld", last == 1 ? "DOWN at start, ignored until it is let go" : "up", r.short_ms, r.warn_ms, r.arm_ms, r.late_ms);
    for (;;) {
        long long now = now_ms();
        int pressed = read_line(fd);
        if (pressed < 0) {
            logf_("BUTTON read: %s", strerror(errno));
            return 1;
        }
        if (pressed != last) {
            logf_("BUTTON %s", pressed ? "down" : "up");
            last = pressed;
        }
        enum event e;
        while ((e = machine_step(&m, &r, pressed, now)) != EV_NONE) {
            switch (e) {
            case EV_SHORT: logf_("BUTTON short press: pairing screen"); tell_ui('1'); break;
            case EV_WARN: logf_("BUTTON held %ld ms: factory-reset warning", r.warn_ms); tell_ui('2'); break;
            case EV_ARMED: logf_("BUTTON held %ld ms: armed", r.arm_ms); tell_ui('3'); break;
            case EV_CANCEL: logf_("BUTTON let go early: cancelled"); tell_ui('4'); break;
            case EV_LATE: logf_("BUTTON held past %ld ms: cancelled", r.late_ms); tell_ui('5'); break;
            case EV_RESET:
                tell_ui('6');
                run_reset(dry);
                /* Whatever the button did meanwhile is not a command. */
                drain(fd);
                machine_init(&m);
                last = -1;
                break;
            default: break;
            }
        }
        /* While the button is down the level is also sampled, in case an edge got lost. */
        long t = machine_timeout(&m, &r, now_ms());
        if (pressed) {
            long sample = m.st == ST_BLOCKED ? 1000 : 100; /* a stuck button costs 1 wake-up/s */
            if (t < 0 || t > sample) t = sample;
        }
        struct pollfd p = { .fd = fd, .events = POLLIN };
        int n = poll(&p, 1, (int)t);
        if (n < 0 && errno != EINTR) {
            logf_("BUTTON poll: %s", strerror(errno));
            return 1;
        }
        if (n > 0) drain(fd);
    }
}

static int probe(int seconds) {
    foreground = 1;
    int fd = open_line();
    if (fd < 0) return 1;
    int last = -1;
    long long end = now_ms() + seconds * 1000LL;
    while (now_ms() < end) {
        int v = read_line(fd);
        if (v != last) {
            printf("BUTTON_PROBE %lld %s\n", now_ms(), v == 1 ? "down" : v == 0 ? "up" : "error");
            fflush(stdout);
            last = v;
        }
        struct pollfd p = { .fd = fd, .events = POLLIN };
        if (poll(&p, 1, 50) > 0) drain(fd);
    }
    return 0;
}

/* Erase every block of an MTD device. Blocks that fail are counted, not fatal: JFFS2 skips
 * a block it cannot use. */
static int erase(const char *dev) {
    int fd = open(dev, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        fprintf(stderr, "erase: %s: %s\n", dev, strerror(errno));
        return 1;
    }
    struct mtd_info_user info;
    if (ioctl(fd, MEMGETINFO, &info) < 0 || info.erasesize == 0) {
        fprintf(stderr, "erase: %s: not an MTD device\n", dev);
        return 1;
    }
    unsigned failed = 0, blocks = info.size / info.erasesize;
    for (unsigned i = 0; i < blocks; i++) {
        struct erase_info_user e = { .start = i * info.erasesize, .length = info.erasesize };
        if (ioctl(fd, MEMERASE, &e) < 0) {
            fprintf(stderr, "erase: block %u: %s\n", i, strerror(errno));
            failed++;
        }
    }
    close(fd);
    printf("ERASE %s: %u blocks of %u bytes, %u failed\n", dev, blocks, info.erasesize, failed);
    return failed ? 1 : 0;
}

int main(int argc, char **argv) {
    if (argc >= 2 && !strcmp(argv[1], "selftest")) return selftest();
    if (argc >= 2 && !strcmp(argv[1], "probe")) return probe(argc >= 3 ? atoi(argv[2]) : 10);
    if (argc >= 2 && !strcmp(argv[1], "erase")) {
        if (argc != 3) {
            fprintf(stderr, "usage: meow-buttond erase DEV\n");
            return 2;
        }
        return erase(argv[2]);
    }
    int dry = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-f")) foreground = 1;
        else if (!strcmp(argv[i], "-n")) dry = 1;
        else {
            fprintf(stderr, "usage: meow-buttond [-f] [-n] | probe [SEC] | erase DEV | selftest\n");
            return 2;
        }
    }
    signal(SIGPIPE, SIG_IGN);
    return daemon_main(dry);
}
