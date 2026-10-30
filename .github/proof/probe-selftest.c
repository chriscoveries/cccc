#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static char root[PATH_MAX];
static char home[PATH_MAX];
static char state[PATH_MAX];

static void check(bool ok) {
    if (!ok) { perror("probe selftest"); exit(1); }
}

static void path(char out[PATH_MAX], const char *base, const char *leaf) {
    int n = snprintf(out, PATH_MAX, "%s/%s", base, leaf);
    check(n > 0 && n < PATH_MAX);
}

static void directory(const char *base, const char *leaf) {
    char name[PATH_MAX]; path(name, base, leaf);
    check(mkdir(name, 0700) == 0);
}

static int file(const char *leaf) {
    char name[PATH_MAX]; path(name, state, leaf);
    int fd = open(name, O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, 0600);
    check(fd >= 0); return fd;
}

static void flag(const char *value) {
    char name[PATH_MAX]; path(name, root, "armed");
    int fd = open(name, O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, 0600);
    check(fd >= 0 && flock(fd, LOCK_EX) == 0);
    check(pwrite(fd, value, 7, 0) == 7 && ftruncate(fd, 7) == 0);
    check(flock(fd, LOCK_UN) == 0 && close(fd) == 0);
}

static void *ordinary_thread(void *unused) {
    (void)unused;
    int fd = file("ledger/ledger.lock");
    check(flock(fd, LOCK_EX | LOCK_NB) == 0);
    check(close(fd) == 0); return NULL;
}

static void exercise(const char *which) {
    int writer = file("turn-usage/writer.lock");
    int ledger = file("ledger/ledger.lock");
    int marker = file("turn-usage/committed/sample.tmp");
    if (!strcmp(which, "ordinary")) {
        check(flock(ledger, LOCK_EX | LOCK_NB) == 0);
    } else {
        check(flock(writer, LOCK_EX) == 0);
        if (!strcmp(which, "append")) {
            errno = 0;
            check(flock(ledger, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK);
        } else if (!strcmp(which, "thread")) {
            pthread_t thread;
            check(pthread_create(&thread, NULL, ordinary_thread, NULL) == 0);
            check(pthread_join(thread, NULL) == 0);
        } else if (!strcmp(which, "close")) {
            check(close(writer) == 0); writer = -1;
            check(flock(ledger, LOCK_EX | LOCK_NB) == 0);
        } else if (!strcmp(which, "unlock")) {
            check(flock(writer, LOCK_UN) == 0);
            check(flock(ledger, LOCK_EX | LOCK_NB) == 0);
        } else if (!strcmp(which, "off")) {
            check(flock(ledger, LOCK_EX | LOCK_NB) == 0 && fsync(marker) == 0);
        } else if (!strcmp(which, "marker")) {
            check(flock(ledger, LOCK_EX | LOCK_NB) == 0);
            check(write(ledger, "committed", 9) == 9 && fdatasync(ledger) == 0);
            fsync(marker); exit(2);
        } else if (!strcmp(which, "malformed")) {
            flock(ledger, LOCK_EX | LOCK_NB); exit(2);
        } else { exit(3); }
    }
    check(close(marker) == 0 && close(ledger) == 0);
    if (writer >= 0) check(close(writer) == 0);
}

int main(int argc, char **argv) {
    umask(077);
    if (argc == 3 && !strcmp(argv[1], "child")) {
        const char *value = getenv("TURN_PROBE_HOME"); check(value != NULL);
        check(strlen(value) < sizeof(home)); strcpy(home, value);
        check(strlen(home) > 5); strcpy(root, home); root[strlen(root) - 5] = 0;
        path(state, home, "groups/sample/state"); exercise(argv[2]); return 0;
    }
    check(argc == 2);
    char temp[] = "/tmp/turn-probe-check-XXXXXXXX";
    char *created = mkdtemp(temp); check(created != NULL); strcpy(root, created);
    directory(root, "home"); path(home, root, "home");
    directory(home, "groups"); directory(home, "groups/sample");
    directory(home, "groups/sample/state"); path(state, home, "groups/sample/state");
    directory(state, "ledger"); directory(state, "turn-usage");
    directory(state, "turn-usage/committed");
    check(setenv("TURN_PROBE_HOME", home, 1) == 0 && setenv("LD_PRELOAD", argv[1], 1) == 0);
    const char *cases[] = {"ordinary", "append", "thread", "close", "unlock", "off", "marker", "malformed"};
    for (unsigned i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i) {
        const char *which = cases[i];
        flag(!strcmp(which, "off") ? "off   \n" :
             !strcmp(which, "marker") ? "marker\n" :
             !strcmp(which, "malformed") ? "invalid" : "append\n");
        pid_t child = fork(); check(child >= 0);
        if (!child) { execl(argv[0], argv[0], "child", which, (char *)NULL); _exit(4); }
        int status; check(waitpid(child, &status, 0) == child && WIFEXITED(status));
        int expected = !strcmp(which, "marker") ? 86 : !strcmp(which, "malformed") ? 87 : 0;
        check(WEXITSTATUS(status) == expected);
        printf("PASS %s exit=%d\n", which, expected);
    }
    printf("PROBE_SELFTEST_PASS retained=%s\n", root); return 0;
}
