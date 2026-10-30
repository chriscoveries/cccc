#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

/* Test-only interposition. The daemon and its ledger formats are unchanged. */
static _Thread_local int writer_fd = -1;
static _Thread_local char writer_name[PATH_MAX];
static atomic_bool append_reported = false;

static bool safe_path(const char *path) {
    if (!path || path[0] != '/' || strstr(path, "//") || strstr(path, "/../") ||
        strstr(path, "/./")) return false;
    for (const char *p = path; *p; ++p) {
        if (!( (*p >= 'a' && *p <= 'z') || (*p >= 'A' && *p <= 'Z') ||
               (*p >= '0' && *p <= '9') || strchr("/_-.", *p))) return false;
    }
    return true;
}

static const char *home(void) {
    const char *p = getenv("TURN_PROBE_HOME");
    if (!safe_path(p) || strncmp(p, "/tmp/", 5) != 0) return NULL;
    size_t n = strlen(p);
    if (n < 10 || n >= PATH_MAX - 128 || strcmp(p + n - 5, "/home")) return NULL;
    struct stat st;
    if (lstat(p, &st) || !S_ISDIR(st.st_mode) || st.st_uid != getuid() ||
        (st.st_mode & 0777) != 0700) return NULL;
    return p;
}

static bool fd_path(int fd, char out[PATH_MAX]) {
    char proc[64];
    int n = snprintf(proc, sizeof(proc), "/proc/self/fd/%d", fd);
    if (n <= 0 || (size_t)n >= sizeof(proc)) return false;
    ssize_t count = readlink(proc, out, PATH_MAX - 1);
    if (count < 0 || count >= PATH_MAX - 1) return false;
    out[count] = 0;
    return safe_path(out);
}

static bool in_home(const char *path) {
    const char *root = home();
    return root && !strncmp(path, root, strlen(root)) && path[strlen(root)] == '/';
}

static bool ends(const char *path, const char *suffix) {
    size_t n = strlen(path), k = strlen(suffix);
    return n >= k && !strcmp(path + n - k, suffix);
}

static bool writer_held(void) {
    char current[PATH_MAX];
    return writer_fd >= 0 && fd_path(writer_fd, current) &&
           !strcmp(current, writer_name);
}

static bool same_state(const char *path) {
    const char *suffix = "/turn-usage/writer.lock";
    size_t n = strlen(writer_name), k = strlen(suffix);
    return n >= k && !strncmp(path, writer_name, n - k) && path[n - k] == '/';
}

static int mode(void) {
    const char *root = home();
    if (!root) return 0;
    char path[PATH_MAX];
    size_t n = strlen(root) - 5;
    memcpy(path, root, n);
    strcpy(path + n, "/armed");
    int fd = (int)syscall(SYS_openat, AT_FDCWD, path,
                         O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC, 0);
    if (fd < 0) {
        if (errno == ENOENT) return 0;
        _exit(87);
    }
    struct stat st;
    if (fstat(fd, &st) || !S_ISREG(st.st_mode) || st.st_uid != getuid() ||
        st.st_nlink != 1 || (st.st_mode & 0777) != 0600 || st.st_size != 7 ||
        syscall(SYS_flock, fd, LOCK_SH)) _exit(87);
    char value[8] = {0};
    ssize_t count = read(fd, value, 7);
    syscall(SYS_flock, fd, LOCK_UN);
    syscall(SYS_close, fd);
    if (count != 7) _exit(87);
    if (!strcmp(value, "append\n")) return 1;
    if (!strcmp(value, "marker\n")) return 2;
    if (!strcmp(value, "off   \n")) return 0;
    _exit(87);
}

static void emit(const char *event, const char *target) {
    const char *root = home();
    if (!root) _exit(87);
    char path[PATH_MAX], line[PATH_MAX + 128];
    size_t n = strlen(root) - 5;
    memcpy(path, root, n);
    strcpy(path + n, "/probe-events.jsonl");
    int fd = (int)syscall(SYS_openat, AT_FDCWD, path,
                         O_WRONLY | O_CREAT | O_APPEND | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC, 0600);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) || !S_ISREG(st.st_mode) || st.st_uid != getuid() ||
        st.st_nlink != 1 || st.st_size > 65536) _exit(87);
    int length = snprintf(line, sizeof(line),
        "{\"event\":\"%s\",\"pid\":%ld,\"path\":\"%s\"}\n", event, (long)getpid(), target);
    if (length <= 0 || (size_t)length >= sizeof(line) ||
        write(fd, line, (size_t)length) != length || syscall(SYS_fsync, fd)) _exit(87);
    syscall(SYS_close, fd);
}

int flock(int fd, int operation) {
    char path[PATH_MAX];
    bool known = fd_path(fd, path) && in_home(path);
    if (known && ends(path, "/turn-usage/writer.lock")) {
        int rc = (int)syscall(SYS_flock, fd, operation);
        if (!rc && (operation & LOCK_EX)) {
            writer_fd = fd;
            memcpy(writer_name, path, strlen(path) + 1);
        }
        if (!rc && (operation & LOCK_UN) && writer_fd == fd) writer_fd = -1;
        return rc;
    }
    if (known && writer_held() && same_state(path) &&
        ends(path, "/ledger/ledger.lock") && (operation & LOCK_EX) && mode() == 1) {
        if (!atomic_exchange(&append_reported, true)) emit("append_blocked", path);
        errno = EWOULDBLOCK;
        return -1;
    }
    return (int)syscall(SYS_flock, fd, operation);
}

static void marker_boundary(int fd) {
    char path[PATH_MAX];
    if (fd_path(fd, path) && in_home(path) && writer_held() && same_state(path) &&
        strstr(path, "/turn-usage/committed/") && mode() == 2) {
        emit("marker_crash", path);
        _exit(86);
    }
}

int fsync(int fd) {
    marker_boundary(fd);
    return (int)syscall(SYS_fsync, fd);
}

int fdatasync(int fd) {
    marker_boundary(fd);
    return (int)syscall(SYS_fdatasync, fd);
}

int close(int fd) {
    if (fd == writer_fd) writer_fd = -1;
    return (int)syscall(SYS_close, fd);
}
