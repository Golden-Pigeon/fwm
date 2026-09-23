/*
 * FWM Linux native recovery helper.
 *
 * This file is uploaded and executed as a standalone program by the Rust
 * client.  It intentionally uses no interpreter, shell utility, pidfd, or
 * kernel interface newer than fcntl(F_SETOWN)/O_ASYNC.  The JSON protocol is
 * line based and has the same claim/confirm/release operations as the original
 * helper.  Linux IPv4 is supported; IPv6 returns an explicit unsupported
 * protocol error rather than silently weakening ownership checks.
 *
 * JSON object boundaries are parsed by the vendored MIT-licensed jsmn
 * tokenizer; protocol fields remain restricted UUID/IP/decimal values.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <ctype.h>
#define JSMN_STATIC
#define JSMN_STRICT
#define JSMN_PARENT_LINKS
#include "vendor/jsmn.h"
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define PROTOCOL 1
#define MAX_LINE 65536
#define TERM_MS 2000
#define KILL_MS 2000
#define PATH_CAP 4096

struct identity {
    pid_t pid, ppid;
    uid_t uid;
    unsigned long long start;
    char boot[64];
    char name[64];
    char state;
};
struct transport {
    char client[INET6_ADDRSTRLEN];
    unsigned client_port;
    char server[INET6_ADDRSTRLEN];
    unsigned server_port;
};
struct claim {
    char owner[37], rule[37], session[37], host[INET6_ADDRSTRLEN];
    unsigned long long generation;
    unsigned port;
};
struct helper_state {
    struct claim current;
    struct identity session, helper;
    struct transport transport;
    char directory[PATH_CAP], path[PATH_CAP], lock_path[PATH_CAP];
    int lock_fd;
    unsigned transport_uid;
    unsigned long transport_inode;
    unsigned listener_uid;
    unsigned long listener_inode;
    int claimed;
};

static void emit_json_text(const char *text) {
    putchar('"');
    for (const unsigned char *p = (const unsigned char *)(text ? text : ""); *p; p++) {
        if (*p == '"' || *p == '\\') printf("\\%c", *p);
        else if (*p == '\n') fputs("\\n", stdout);
        else if (*p == '\r') fputs("\\r", stdout);
        else if (*p == '\t') fputs("\\t", stdout);
        else if (*p < 32) printf("\\u%04x", *p);
        else putchar(*p);
    }
    putchar('"');
}
static void emit_error(const char *op, const char *code, const char *message) {
    fputs("{\"ok\":false,\"op\":", stdout); emit_json_text(op);
    printf(",\"protocol\":%d,\"code\":", PROTOCOL); emit_json_text(code);
    fputs(",\"message\":", stdout); emit_json_text(message); fputs("}\n", stdout);
    fflush(stdout);
}
static long long mono_ms(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t) < 0) return 0;
    return (long long)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static void nap(int ms) { (void)poll(NULL, 0, ms); }
static int write_all(int fd, const char *p, size_t n) {
    while (n) {
        ssize_t k = write(fd, p, n);
        if (k < 0 && errno == EINTR) continue;
        if (k <= 0) return -1;
        p += k; n -= (size_t)k;
    }
    return 0;
}
static int read_all_fd(int fd, char *buf, size_t cap) {
    size_t n = 0;
    while (n + 1 < cap) {
        ssize_t k = read(fd, buf + n, cap - n - 1);
        if (k < 0 && errno == EINTR) continue;
        if (k < 0) return -1;
        if (!k) break;
        n += (size_t)k;
    }
    buf[n] = 0;
    return n ? 0 : -1;
}
static int read_at(int dirfd, const char *name, char *buf, size_t cap) {
    int fd = openat(dirfd, name, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return -1;
    int r = read_all_fd(fd, buf, cap), saved = errno;
    close(fd); errno = saved; return r;
}
/* Decode only the exact field in this object, never a similarly named field
 * in a sibling/nested object. Counts and byte sizes are bounded by the protocol. */
static int json_value(const char *json, const char *key, char *out, size_t cap, int kind) {
    jsmn_parser parser; jsmntok_t tokens[512];
    size_t length = strlen(json), key_length = strlen(key);
    jsmn_init(&parser);
    int count = jsmn_parse(&parser, json, length, tokens, 512);
    if (count < 1 || tokens[0].type != JSMN_OBJECT) return -1;
    for (size_t i = (size_t)tokens[0].end; i < length; i++)
        if (!isspace((unsigned char)json[i])) return -1;
    int found = -1;
    for (int i = 1; i + 1 < count; i++) {
        jsmntok_t *token = &tokens[i];
        if (token->parent != 0 || token->type != JSMN_STRING ||
            (size_t)(token->end - token->start) != key_length ||
            memcmp(json + token->start, key, key_length)) continue;
        if (found != -1 || tokens[i + 1].parent != i) return -1;
        found = i + 1;
    }
    if (found < 0) return -1;
    jsmntok_t *value = &tokens[found];
    jsmntype_t wanted = kind == 1 ? JSMN_STRING : kind == 2 ? JSMN_OBJECT :
                        kind == 3 ? JSMN_ARRAY : JSMN_PRIMITIVE;
    if (value->type != wanted) return -1;
    size_t n = (size_t)(value->end - value->start);
    if (n >= cap) return -1;
    if (kind == 1) {
        /* UUIDs, IP literals, process names and proof sources need no escapes.
         * Reject unsupported escaping rather than treating raw text as identity. */
        for (size_t i = 0; i < n; i++)
            if (json[value->start + i] == '\\' || (unsigned char)json[value->start + i] < 32) return -1;
    }
    memcpy(out, json + value->start, n); out[n] = 0;
    return 0;
}
static int json_string(const char *json, const char *key, char *out, size_t cap) {
    return json_value(json, key, out, cap, 1);
}
static int decimal_u64(const char *text, unsigned long long *out) {
    if (!*text) return -1;
    for (const char *p = text; *p; p++) if (*p < '0' || *p > '9') return -1;
    char *end; errno = 0; *out = strtoull(text, &end, 10);
    return errno || *end ? -1 : 0;
}
static int json_u64(const char *json, const char *key, unsigned long long *out) {
    char buf[32];
    return json_value(json, key, buf, sizeof(buf), 0) == 0 ? decimal_u64(buf, out) : -1;
}
static int json_inode(const char *json, unsigned long long *out) {
    char buf[32];
    if (json_u64(json, "inode", out) == 0) return *out ? 0 : -1;
    if (json_string(json, "inode", buf, sizeof(buf)) < 0 || decimal_u64(buf, out) < 0) return -1;
    return *out ? 0 : -1;
}
static int json_nested_u64(const char *json, const char *object, const char *key, unsigned long long *out) {
    char value[MAX_LINE];
    return json_value(json, object, value, sizeof(value), 2) == 0 ? json_u64(value, key, out) : -1;
}
static int json_nested_string(const char *json, const char *object, const char *key, char *out, size_t cap) {
    char value[MAX_LINE];
    return json_value(json, object, value, sizeof(value), 2) == 0 ? json_string(value, key, out, cap) : -1;
}
static int json_bool(const char *json, const char *key, int *out) {
    char buf[8]; if (json_value(json, key, buf, sizeof(buf), 0) < 0) return -1;
    if (!strcmp(buf, "true")) { *out = 1; return 0; }
    if (!strcmp(buf, "false")) { *out = 0; return 0; }
    return -1;
}
static int uuid_ok(const char *s) {
    if (strlen(s) != 36) return 0;
    for (int i = 0; i < 36; i++) {
        if (i == 8 || i == 13 || i == 18 || i == 23) { if (s[i] != '-') return 0; }
        else if (!((s[i] >= '0' && s[i] <= '9') || (s[i] >= 'a' && s[i] <= 'f'))) return 0;
    }
    return 1;
}
static int parse_ipv4(const char *s, struct in_addr *out) {
    if (inet_pton(AF_INET, s, out) == 1) return 0;
    if (strchr(s, ':')) return -2; /* explicit unsupported IPv6 */
    return -1;
}
struct tcp4_row {
    struct in_addr local, remote;
    unsigned local_port, remote_port, state, uid;
    unsigned long inode;
};

/* procfs prints IPv4 addresses as native-endian hexadecimal words. Assign
 * the parsed word directly to s_addr; htonl would reverse it a second time.
 * The three fields between state and UID are tx:rx, timer, and retransmits.
 * https://docs.kernel.org/networking/proc_net_tcp.html
 */
static int parse_tcp4_row(const char *line, struct tcp4_row *row) {
    unsigned local, remote;
    int count = sscanf(line,
        " %*u: %8x:%4x %8x:%4x %2x %*s %*s %*s %u %*u %lu",
        &local, &row->local_port, &remote, &row->remote_port,
        &row->state, &row->uid, &row->inode);
    if (count != 7) return -1;
    row->local.s_addr = local;
    row->remote.s_addr = remote;
    return 0;
}

static int transport_inode(const struct transport *t, unsigned *uid_out, unsigned long *inode_out) {
    struct in_addr client, server;
    if (parse_ipv4(t->client, &client) || parse_ipv4(t->server, &server)) return -1;
    FILE *f = fopen("/proc/net/tcp", "r");
    if (!f) return -1;
    char line[512]; int result = 0;
    while (fgets(line, sizeof(line), f)) {
        struct tcp4_row row;
        if (parse_tcp4_row(line, &row) != 0 || row.state != 1 || !row.inode) continue;
        if (row.local.s_addr == server.s_addr && row.remote.s_addr == client.s_addr &&
            row.local_port == t->server_port && row.remote_port == t->client_port) {
            if (uid_out) *uid_out = row.uid;
            if (inode_out) *inode_out = row.inode;
            result = 1;
            break;
        }
    }
    if (ferror(f)) result = -1;
    fclose(f);
    return result;
}

/* -2 means the kernel denied fd inspection (normal for a privsep sshd).
 * Other errors must not authorize the opaque-descriptor proof path. */
static int listener_owned_by(pid_t pid, unsigned long inode) {
    char path[64], wanted[64], link[128];
    if (!inode) return 0;
    snprintf(path, sizeof(path), "/proc/%ld/fd", (long)pid);
    snprintf(wanted, sizeof(wanted), "socket:[%lu]", inode);
    DIR *directory = opendir(path);
    if (!directory) {
        if (errno == EACCES || errno == EPERM) return -2;
        return errno == ENOENT || errno == ESRCH ? 0 : -1;
    }
    int found = 0;
    struct dirent *entry;
    errno = 0;
    while ((entry = readdir(directory))) {
        if (entry->d_name[0] == '.') continue;
        char fdpath[PATH_CAP];
        if (snprintf(fdpath, sizeof(fdpath), "%s/%s", path, entry->d_name) >= (int)sizeof(fdpath)) {
            found = -1; break;
        }
        ssize_t length = readlink(fdpath, link, sizeof(link) - 1);
        if (length >= 0) {
            link[length] = 0;
            if (!strcmp(link, wanted)) { found = 1; break; }
        } else if (errno == EACCES || errno == EPERM) {
            found = -2;
        } else if (errno != ENOENT && errno != ESRCH) {
            found = -1; break;
        }
        errno = 0;
    }
    if (!entry && errno) found = -1;
    closedir(directory);
    return found;
}

static int transport_owned_by(pid_t pid, const struct transport *t) {
    unsigned long inode;
    int found = transport_inode(t, NULL, &inode);
    return found == 1 ? listener_owned_by(pid, inode) : found;
}
static int parse_claim(const char *line, struct claim *c) {
    unsigned long long n, port;
    if (json_string(line, "owner_id", c->owner, sizeof(c->owner)) < 0 || !uuid_ok(c->owner) ||
        json_string(line, "rule_id", c->rule, sizeof(c->rule)) < 0 || !uuid_ok(c->rule) ||
        json_string(line, "session_id", c->session, sizeof(c->session)) < 0 || !uuid_ok(c->session) ||
        json_string(line, "listen_host", c->host, sizeof(c->host)) < 0 ||
        parse_ipv4(c->host, &(struct in_addr){0}) != 0 || json_u64(line, "generation", &n) < 0 ||
        json_u64(line, "listen_port", &port) < 0 || port == 0 || port > 65535) return -1;
    c->generation = n; c->port = (unsigned)port; return 0;
}
static int read_identity_fd(int dirfd, struct identity *id) {
    char statbuf[4096], status[8192], *left, *right, *save, *field, *end;
    if (read_at(dirfd, "stat", statbuf, sizeof(statbuf)) < 0 || read_at(dirfd, "status", status, sizeof(status)) < 0) return -1;
    left = strchr(statbuf, '('); right = strrchr(statbuf, ')');
    if (!left || !right || right <= left || right[1] != ' ') return -1;
    size_t namelen = (size_t)(right - left - 1); if (namelen >= sizeof(id->name)) return -1;
    memcpy(id->name, left + 1, namelen); id->name[namelen] = 0;
    errno = 0; long p = strtol(statbuf, &end, 10); if (errno || end != left - 1 || p < 1 || p > INT_MAX) return -1;
    id->pid = (pid_t)p; id->state = 0; id->ppid = 0; id->start = 0;
    int fieldno = 3; field = strtok_r(right + 2, " ", &save);
    while (field) {
        if (fieldno == 3) id->state = field[0];
        if (fieldno == 4) id->ppid = (pid_t)strtol(field, &end, 10);
        if (fieldno == 22) { errno = 0; id->start = strtoull(field, &end, 10); if (errno || *end) return -1; break; }
        fieldno++; field = strtok_r(NULL, " ", &save);
    }
    if (fieldno != 22 || !id->state) return -1;
    char *uid = strstr(status, "\nUid:"); unsigned real, effective, saved, fs;
    if (!uid || sscanf(uid + 1, "Uid: %u %u %u %u", &real, &effective, &saved, &fs) != 4) return -1;
    id->uid = (uid_t)effective;
    int bootfd = open("/proc/sys/kernel/random/boot_id", O_RDONLY | O_CLOEXEC);
    if (bootfd < 0 || read_all_fd(bootfd, id->boot, sizeof(id->boot)) < 0) { if (bootfd >= 0) close(bootfd); return -1; }
    close(bootfd); char *nl = strchr(id->boot, '\n'); if (nl) *nl = 0;
    return 0;
}
static int proc_fd(pid_t pid) {
    char path[64]; if (snprintf(path, sizeof(path), "/proc/%ld", (long)pid) >= (int)sizeof(path)) return -1;
    return open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
}
static int read_identity(pid_t pid, struct identity *id) {
    int fd = proc_fd(pid); if (fd < 0) return -1;
    int r = read_identity_fd(fd, id); close(fd); return r;
}
static int same_id(const struct identity *a, const struct identity *b) {
    return a && b && a->pid == b->pid && a->uid == b->uid && a->start == b->start &&
           !strcmp(a->boot, b->boot) && !strcmp(a->name, b->name);
}
static int allowed_name(const char *s) { return !strcmp(s, "sshd") || !strcmp(s, "sshd-session"); }
static int read_transport(struct transport *t) {
    const char *env = getenv("SSH_CONNECTION");
    char client[INET6_ADDRSTRLEN], server[INET6_ADDRSTRLEN], extra;
    unsigned cp, sp;
    if (!env || sscanf(env, "%45s %u %45s %u %c", client, &cp, server, &sp, &extra) != 4 ||
        cp > 65535 || sp > 65535 || !cp || !sp) return -1;
    if (parse_ipv4(client, &(struct in_addr){0}) || parse_ipv4(server, &(struct in_addr){0})) return -2;
    strcpy(t->client, client); strcpy(t->server, server);
    t->client_port = cp; t->server_port = sp;
    return 0;
}
static int find_session(const struct transport *t, struct identity *found) {
    struct identity cur;
    unsigned long inode;
    if (read_identity(getpid(), &cur) < 0 || transport_inode(t, NULL, &inode) != 1) return -1;
    pid_t pid = cur.ppid; uid_t me = geteuid();
    for (int depth = 0; depth < 32 && pid > 1; depth++) {
        struct identity next;
        if (read_identity(pid, &next) < 0) return -1;
        if (allowed_name(next.name) && next.uid == me) {
            int ownership = listener_owned_by(next.pid, inode);
            /* Exact transport plus a same-user SSH ancestor registers the
             * durable proof only when fd inspection is explicitly denied. */
            if (ownership == 1 || ownership == -2) { *found = next; return 0; }
            if (ownership < 0) return -1;
        }
        if (pid == next.ppid) return -1;
        pid = next.ppid;
    }
    return -1;
}
static int tcp_listener_info(const char *host, unsigned port, unsigned *uid,
                             unsigned long *inode, struct in_addr *actual) {
    struct in_addr requested;
    if (parse_ipv4(host, &requested)) return -2;
    FILE *f = fopen("/proc/net/tcp", "r");
    if (!f) return -1;
    char line[512]; int result = 0;
    while (fgets(line, sizeof(line), f)) {
        struct tcp4_row row;
        if (parse_tcp4_row(line, &row) != 0 || row.state != 0x0A || row.local_port != port) continue;
        if (row.local.s_addr == requested.s_addr || row.local.s_addr == 0 || requested.s_addr == 0) {
            if (!row.inode || result) { result = -1; break; }
            result = 1;
            if (uid) *uid = row.uid;
            if (inode) *inode = row.inode;
            if (actual) *actual = row.local;
        }
    }
    if (ferror(f)) result = -1;
    fclose(f);
    return result;
}
static int tcp_listener(const char *host, unsigned port, unsigned *uid, unsigned long *inode) {
    return tcp_listener_info(host, port, uid, inode, NULL);
}
static int port_free(const char *host, unsigned port) {
    struct in_addr addr; if (parse_ipv4(host, &addr)) return -2;
    int fd = socket(AF_INET, SOCK_STREAM, 0); if (fd < 0) return -1; int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)); struct sockaddr_in a = {0}; a.sin_family = AF_INET; a.sin_addr = addr; a.sin_port = htons((uint16_t)port);
    int r = bind(fd, (struct sockaddr *)&a, sizeof(a)); int saved = errno; close(fd); errno = saved;
    return r == 0 ? 1 : (errno == EADDRINUSE || errno == EACCES ? 0 : -1);
}
static int private_dir_recursive(const char *path) {
    char copy[PATH_CAP]; if (strlen(path) >= sizeof(copy)) { errno = ENAMETOOLONG; return -1; }
    strcpy(copy, path);
    for (char *p = copy + 1; *p; p++) {
        if (*p != '/') continue;
        *p = 0;
        if (mkdir(copy, 0700) < 0 && errno != EEXIST) return -1;
        *p = '/';
    }
    if (mkdir(copy, 0700) < 0 && errno != EEXIST) return -1;
    struct stat st; if (lstat(copy, &st) < 0 || !S_ISDIR(st.st_mode) || st.st_uid != geteuid() || (st.st_mode & 077)) { errno = EPERM; return -1; }
    return 0;
}
static int registry(struct helper_state *s, const char *owner, const char *rule) {
    const char *base = getenv("FWM_REMOTE_STATE_DIR"); char fallback[PATH_CAP];
    if (!base) {
        const char *state = getenv("XDG_STATE_HOME"), *home = getenv("HOME");
        int n;
        if (state) n = snprintf(fallback, sizeof(fallback), "%s/fwm", state);
        else { if (!home) return -1; n = snprintf(fallback, sizeof(fallback), "%s/.local/state/fwm", home); }
        if (n < 0 || (size_t)n >= sizeof(fallback)) return -1;
        base = fallback;
    }
    if (base[0] != '/') return -1;
    char leases[PATH_CAP], owner_dir[PATH_CAP]; snprintf(leases, sizeof(leases), "%s/leases", base); snprintf(owner_dir, sizeof(owner_dir), "%s/%s", leases, owner);
    if (private_dir_recursive(base) < 0 || private_dir_recursive(leases) < 0 || private_dir_recursive(owner_dir) < 0) return -1;
    snprintf(s->directory, sizeof(s->directory), "%s", owner_dir);
    snprintf(s->path, sizeof(s->path), "%s/%s.json", owner_dir, rule);
    snprintf(s->lock_path, sizeof(s->lock_path), "%s/%s.lock", owner_dir, rule);
    s->lock_fd = open(s->lock_path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600); if (s->lock_fd < 0) return -1;
    struct stat st; if (fstat(s->lock_fd, &st) < 0 || !S_ISREG(st.st_mode) || st.st_uid != geteuid() || (st.st_mode & 077) || st.st_nlink != 1) { close(s->lock_fd); s->lock_fd = -1; errno = EPERM; return -1; }
    if (flock(s->lock_fd, LOCK_EX) < 0) { close(s->lock_fd); s->lock_fd = -1; return -1; }
    return 0;
}
static int read_record(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW); if (fd < 0) return errno == ENOENT ? 1 : -1;
    struct stat st; if (fstat(fd, &st) < 0 || !S_ISREG(st.st_mode) || st.st_uid != geteuid() || (st.st_mode & 077) || st.st_nlink != 1) { close(fd); errno = EPERM; return -1; }
    int r = read_all_fd(fd, buf, cap); close(fd); return r < 0 ? -1 : 0;
}
static int write_record(const char *path, const char *text) {
    char tmp[PATH_CAP]; snprintf(tmp, sizeof(tmp), "%s.tmp-%ld", path, (long)getpid());
    int fd = open(tmp, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600); if (fd < 0) return -1;
    int r = write_all(fd, text, strlen(text)); if (!r && fsync(fd) < 0) r = -1; close(fd);
    if (!r && rename(tmp, path) < 0) r = -1;
    if (!r) { char parent[PATH_CAP]; snprintf(parent,sizeof(parent),"%s",path); char *slash=strrchr(parent,'/'); if(slash){*slash=0; int d=open(parent,O_RDONLY|O_DIRECTORY|O_CLOEXEC); if(d>=0){if(fsync(d)<0)r=-1;close(d);}else r=-1;} }
    if (r) unlink(tmp); return r;
}
static void unlock_registry(struct helper_state *s) { if (s->lock_fd >= 0) { flock(s->lock_fd, LOCK_UN); close(s->lock_fd); s->lock_fd = -1; } }
static int old_identity(const char *record, struct identity *id, pid_t *pid) {
    unsigned long long start, uid, pid_value;
    char birth[96];
    memset(id, 0, sizeof(*id));
    int nested = json_nested_u64(record, "session", "pid", &pid_value) == 0;
    if (nested) {
        if (json_nested_u64(record, "session", "uid", &uid) < 0 ||
            json_nested_string(record, "session", "name", id->name, sizeof(id->name)) < 0 ||
            json_nested_string(record, "session", "birth", birth, sizeof(birth)) < 0) return -1;
    } else {
        if (json_u64(record, "session_pid", &pid_value) < 0 || json_u64(record, "session_uid", &uid) < 0 ||
            json_u64(record, "session_start", &start) < 0 ||
            json_string(record, "session_name", id->name, sizeof(id->name)) < 0 ||
            json_string(record, "session_birth", birth, sizeof(birth)) < 0) return -1;
    }
    if (pid_value < 2 || pid_value > INT_MAX || uid > UINT_MAX || !allowed_name(id->name)) return -1;
    char *colon = strrchr(birth, ':');
    if (!colon) return -1;
    *colon++ = 0;
    if (!uuid_ok(birth) || decimal_u64(colon, &id->start) < 0 ||
        (!nested && start != id->start)) return -1;
    strcpy(id->boot, birth);
    *pid = (pid_t)pid_value; id->pid = *pid; id->uid = (uid_t)uid;
    return 0;
}
static int old_transport(const char *record, struct transport *t) {
    char array[256], extra;
    unsigned long long cp, sp;
    if (json_value(record, "transport", array, sizeof(array), 3) == 0) {
        int end = 0;
        if (sscanf(array, "[ \"%45[^\"]\" , %llu , \"%45[^\"]\" , %llu ] %n%c",
                   t->client, &cp, t->server, &sp, &end, &extra) != 4 ||
            !end || array[end]) return -1;
    } else {
        if (json_string(record, "transport_client", t->client, sizeof(t->client)) < 0 ||
            json_string(record, "transport_server", t->server, sizeof(t->server)) < 0 ||
            json_u64(record, "transport_client_port", &cp) < 0 ||
            json_u64(record, "transport_server_port", &sp) < 0) return -1;
    }
    if (!cp || cp > 65535 || !sp || sp > 65535 ||
        parse_ipv4(t->client, &(struct in_addr){0}) || parse_ipv4(t->server, &(struct in_addr){0})) return -1;
    t->client_port = (unsigned)cp; t->server_port = (unsigned)sp;
    return 0;
}
static int session_proof_matches(const char *record, unsigned uid, unsigned long inode);
static int validate_old_record(const char *record, const struct claim *requested, struct identity *oldid, pid_t *oldpid,
                               char *oldhost, size_t oldhost_cap, unsigned *oldport, struct transport *oldtransport) {
    unsigned long long protocol, generation, port; char phase[32], owner[37], rule[37], sid[37], registration[64];
    if (json_u64(record,"protocol",&protocol) < 0 || protocol != PROTOCOL || json_string(record,"phase",phase,sizeof(phase)) < 0 ||
        (strcmp(phase,"claimed") && strcmp(phase,"confirmed")) || json_string(record,"owner_id",owner,sizeof(owner)) < 0 ||
        json_string(record,"rule_id",rule,sizeof(rule)) < 0 || json_string(record,"session_id",sid,sizeof(sid)) < 0 ||
        !uuid_ok(owner) || !uuid_ok(rule) || !uuid_ok(sid) || strcmp(owner,requested->owner) || strcmp(rule,requested->rule) ||
        json_u64(record,"generation",&generation) < 0 || generation >= requested->generation ||
        json_string(record,"listen_host",oldhost,oldhost_cap) < 0 || parse_ipv4(oldhost,&(struct in_addr){0}) != 0 ||
        json_u64(record,"listen_port",&port) < 0 || !port || port > 65535 || old_identity(record,oldid,oldpid) < 0 ||
        json_string(record,"registration",registration,sizeof(registration)) < 0 || strcmp(registration,"ssh_exec_ancestry") ||
        old_transport(record,oldtransport) < 0 ||
        !strstr(record,"\"session_proof\"")) return -1;
    struct identity live;
    if (read_identity(*oldpid,&live) == 0 && same_id(oldid,&live)) {
        int transport_seen = transport_inode(oldtransport, NULL, NULL);
        int ownership = transport_seen == 1 ? transport_owned_by(*oldpid, oldtransport) : 0;
        if (transport_seen != 1 || (ownership != 1 && ownership != -2)) return -1;
        if (ownership == -2) {
            unsigned proof_uid; unsigned long proof_inode;
            if (transport_inode(oldtransport,&proof_uid,&proof_inode) != 1 || !session_proof_matches(record,proof_uid,proof_inode)) return -1;
        }
    }
    if (!strcmp(phase,"confirmed") && !strstr(record,"\"listener_proof\"")) return -1;
    *oldport = (unsigned)port; return 0;
}
static int proof_matches(const char *record, const char *key, int listener, unsigned uid, unsigned long inode) {
    char proof[MAX_LINE], source[64], socket_json[MAX_LINE];
    if (!inode || json_value(record, key, proof, sizeof(proof), 2) < 0 ||
        json_string(proof, "source", source, sizeof(source)) < 0) return 0;
    if (strcmp(source, "process_fd") && strcmp(source, listener ? "ssh_forward_ack_inode" : "ssh_exec_ancestry_inode")) return 0;
    const char *socket_proof = proof;
    if (listener) {
        char sockets[MAX_LINE];
        if (json_value(proof, "sockets", sockets, sizeof(sockets), 3) == 0) {
            jsmn_parser parser; jsmntok_t tokens[128]; jsmn_init(&parser);
            int count = jsmn_parse(&parser, sockets, strlen(sockets), tokens, 128);
            if (count < 2 || tokens[0].size != 1 || tokens[1].type != JSMN_OBJECT) return 0;
            size_t size = (size_t)(tokens[1].end - tokens[1].start);
            if (size >= sizeof(socket_json)) return 0;
            memcpy(socket_json, sockets + tokens[1].start, size); socket_json[size] = 0;
            socket_proof = socket_json;
        }
    } else if (json_value(proof, "socket", socket_json, sizeof(socket_json), 2) == 0) {
        socket_proof = socket_json;
    }
    unsigned long long proof_uid, proof_inode;
    return json_u64(socket_proof, "uid", &proof_uid) == 0 && proof_uid == uid &&
           json_inode(socket_proof, &proof_inode) == 0 && proof_inode == inode;
}
static int listener_proof_matches(const char *record, unsigned uid, unsigned long inode) {
    return proof_matches(record, "listener_proof", 1, uid, inode);
}
static int session_proof_matches(const char *record, unsigned uid, unsigned long inode) {
    return proof_matches(record, "session_proof", 0, uid, inode);
}
static int pin_signal(const struct identity *expected, int recover) {
    int proc = proc_fd(expected->pid); if (proc < 0) return 0; struct identity now;
    if (read_identity_fd(proc, &now) < 0 || !same_id(expected, &now)) { close(proc); return -1; }
    int pipefd[2] = {-1,-1}; if (pipe(pipefd) < 0) { close(proc); return -1; }
    int fl = fcntl(pipefd[0], F_GETFL); if (fl < 0 || fcntl(pipefd[0], F_SETFL, fl | O_NONBLOCK) < 0 || fcntl(pipefd[0], F_SETOWN, expected->pid) < 0) goto fail;
    if (read_identity_fd(proc, &now) < 0 || !same_id(expected, &now)) goto fail;
    int sig = SIGTERM;
    for (int pass = 0; pass < (recover ? 2 : 1); pass++) {
#ifdef F_SETSIG
        if (fcntl(pipefd[0], F_SETSIG, sig) < 0 || fcntl(pipefd[0], F_SETFL, (fl | O_NONBLOCK | O_ASYNC)) < 0) goto fail;
#else
        (void)sig; errno = ENOTSUP; goto fail;
#endif
        char x = 'x'; if (write(pipefd[1], &x, 1) != 1) goto fail;
        long long until = mono_ms() + (pass ? KILL_MS : TERM_MS); int gone = 0;
        while (mono_ms() < until) { struct identity cur; if (read_identity_fd(proc, &cur) < 0 || cur.state == 'Z' || cur.state == 'X' || cur.state == 'x') { gone = 1; break; } nap(25); }
        if (gone) { fl = fcntl(pipefd[0], F_GETFL); if (fl >= 0) fcntl(pipefd[0], F_SETFL, fl & ~O_ASYNC); close(pipefd[0]); close(pipefd[1]); close(proc); return 1; }
        if (!recover) break;
        char drain[64]; while (read(pipefd[0], drain, sizeof(drain)) > 0) {}
        sig = SIGKILL;
    }
    fl = fcntl(pipefd[0], F_GETFL); if (fl >= 0) fcntl(pipefd[0], F_SETFL, fl & ~O_ASYNC); close(pipefd[0]); close(pipefd[1]); close(proc); return 0;
fail:
    fl = fcntl(pipefd[0], F_GETFL); if (fl >= 0) fcntl(pipefd[0], F_SETFL, fl & ~O_ASYNC); close(pipefd[0]); close(pipefd[1]); close(proc); return -1;
}
static int identity_json(const struct identity *id, char *out, size_t cap) {
    /* Both identities came from procfs, not the request. Refuse names that
     * cannot be safely represented by this fixed record format. */
    for (const unsigned char *p = (const unsigned char *)id->name; *p; p++)
        if (*p < 32 || *p == '"' || *p == '\\') return -1;
    int n = snprintf(out, cap,
        "{\"pid\":%ld,\"ppid\":%ld,\"uid\":%u,\"birth\":\"%s:%llu\",\"name\":\"%s\"}",
        (long)id->pid, (long)id->ppid, (unsigned)id->uid, id->boot, id->start, id->name);
    return n >= 0 && (size_t)n < cap ? 0 : -1;
}
static int make_record(struct helper_state *s, char *out, size_t cap, const char *phase) {
    char session[512], helper[512], listener[1024] = "";
    if (identity_json(&s->session, session, sizeof(session)) < 0 ||
        identity_json(&s->helper, helper, sizeof(helper)) < 0) return -1;
    if (!strcmp(phase, "confirmed")) {
        if (!s->listener_inode) return -1;
        int n = snprintf(listener, sizeof(listener),
            ",\"listener_proof\":{\"source\":\"ssh_forward_ack_inode\",\"uid\":%u,\"inode\":%lu,"
            "\"sockets\":[{\"local\":[\"%s\",%u],\"remote\":[\"0.0.0.0\",0],\"listening\":true,\"uid\":%u,\"inode\":\"%lu\"}]}",
            s->listener_uid, s->listener_inode, s->current.host, s->current.port,
            s->listener_uid, s->listener_inode);
        if (n < 0 || (size_t)n >= sizeof(listener)) return -1;
    }
    /* Keep native flat fields and the original nested identity/socket schema.
     * Older helpers and existing recovery diagnostics can read the same lease. */
    int n = snprintf(out, cap,
        "{\"protocol\":%d,\"phase\":\"%s\",\"owner_id\":\"%s\",\"rule_id\":\"%s\",\"generation\":%llu,"
        "\"session_id\":\"%s\",\"listen_host\":\"%s\",\"listen_port\":%u,"
        "\"session_pid\":%ld,\"session_uid\":%u,\"session_start\":%llu,\"session_birth\":\"%s:%llu\",\"session_name\":\"%s\",\"helper_pid\":%ld,"
        "\"session\":%s,\"helper\":%s,"
        "\"transport_client\":\"%s\",\"transport_client_port\":%u,\"transport_server\":\"%s\",\"transport_server_port\":%u,"
        "\"transport\":[\"%s\",%u,\"%s\",%u],"
        "\"session_proof\":{\"source\":\"ssh_exec_ancestry_inode\",\"uid\":%u,\"inode\":%lu,"
        "\"socket\":{\"local\":[\"%s\",%u],\"remote\":[\"%s\",%u],\"listening\":false,\"uid\":%u,\"inode\":\"%lu\"}},"
        "\"listener_absent_at_claim\":true,\"registration\":\"ssh_exec_ancestry\"%s}\n",
        PROTOCOL, phase, s->current.owner, s->current.rule, s->current.generation,
        s->current.session, s->current.host, s->current.port,
        (long)s->session.pid, (unsigned)s->session.uid, s->session.start, s->session.boot,
        s->session.start, s->session.name, (long)s->helper.pid, session, helper,
        s->transport.client, s->transport.client_port, s->transport.server, s->transport.server_port,
        s->transport.client, s->transport.client_port, s->transport.server, s->transport.server_port,
        s->transport_uid, s->transport_inode, s->transport.server, s->transport.server_port,
        s->transport.client, s->transport.client_port, s->transport_uid, s->transport_inode, listener);
    return n >= 0 && (size_t)n < cap ? 0 : -1;
}
static int match_current(const char *line, const struct claim *c) {
    char owner[37], rule[37], sid[37]; unsigned long long gen;
    return json_string(line,"owner_id",owner,sizeof(owner))==0&&json_string(line,"rule_id",rule,sizeof(rule))==0&&json_string(line,"session_id",sid,sizeof(sid))==0&&json_u64(line,"generation",&gen)==0&&!strcmp(owner,c->owner)&&!strcmp(rule,c->rule)&&!strcmp(sid,c->session)&&gen==c->generation;
}
static int claim_op(struct helper_state *s, const char *line, char *op) {
    if (s->claimed) { emit_error(op,"invalid_request","this helper already claimed a rule"); return -1; }
    if (parse_claim(line, &s->current) < 0) { emit_error(op,"invalid_request","claim fields are invalid (UUID, IPv4, generation, or port)"); return -1; }
    int tr = read_transport(&s->transport); if (tr == -2) { emit_error(op,"unsupported","IPv6 SSH_CONNECTION requires a future native helper build"); return -1; }
    if (tr < 0 || find_session(&s->transport, &s->session) < 0 || s->session.uid != geteuid() ||
        transport_inode(&s->transport, &s->transport_uid, &s->transport_inode) != 1) { emit_error(op,"ownership_mismatch","cannot identify a same-user ancestor SSH session and its SSH_CONNECTION transport"); return -1; }
    if (read_identity(getpid(), &s->helper) < 0) { emit_error(op,"ownership_mismatch","cannot identify the current recovery helper"); return -1; }
    if (registry(s, s->current.owner, s->current.rule) < 0) { emit_error(op,"permission_denied","cannot create or lock the private remote lease registry"); return -1; }
    char old[MAX_LINE]; int rr = read_record(s->path, old, sizeof(old)); int reclaimed = 0;
    if (rr < 0) { unlock_registry(s); emit_error(op,"ownership_mismatch","existing remote lease is unreadable or unsafe"); return -1; }
    if (rr == 0) {
        struct identity oldid; pid_t oldpid; char oldhost[INET6_ADDRSTRLEN]; unsigned oldport; struct transport oldtransport;
        if (validate_old_record(old, &s->current, &oldid, &oldpid, oldhost, sizeof(oldhost), &oldport, &oldtransport) < 0) {
            unlock_registry(s); emit_error(op,"ownership_mismatch","existing remote lease identity or transport proof is invalid"); return -1;
        }
        unsigned old_uid; unsigned long old_inode;
        int old_bound = tcp_listener(oldhost, oldport, &old_uid, &old_inode);
        if (old_bound < 0 || (old_bound > 0 && old_uid != oldid.uid) || (old_bound == 0 && port_free(oldhost, oldport) == 0)) {
            unlock_registry(s); emit_error(op,"unmanaged_conflict","old listener ownership could not be verified"); return -1;
        }
        char old_phase[32];
        int listener_owner = old_bound > 0 ? listener_owned_by(oldpid,old_inode) : 0;
        if (json_string(old,"phase",old_phase,sizeof(old_phase)) < 0 ||
            (old_bound > 0 && listener_owner != 1 && listener_owner != -2) ||
            (!strcmp(old_phase,"confirmed") && old_bound > 0 && !listener_proof_matches(old,old_uid,old_inode)) ||
            (!strcmp(old_phase,"confirmed") && old_bound > 0 && listener_owner < 0 && !listener_proof_matches(old,old_uid,old_inode))) {
            unlock_registry(s); emit_error(op,"unmanaged_conflict","old listener proof does not match the observed listener"); return -1;
        }
        struct identity live;
        int old_live = read_identity(oldpid,&live) == 0 && same_id(&oldid,&live);
        if (!old_live && (old_bound > 0 || port_free(oldhost,oldport) == 0)) {
            unlock_registry(s); emit_error(op,"unmanaged_conflict","old SSH identity is gone but its listener remains occupied"); return -1;
        }
        if (same_id(&oldid, &s->session)) {
            unlock_registry(s); emit_error(op,"ownership_mismatch","old lease identifies the current SSH connection"); return -1;
        }
        /* A changed listen address is checked before terminating the old
         * session, preserving the old usable forward on an unrelated conflict. */
        if ((strcmp(oldhost, s->current.host) || oldport != s->current.port) &&
            (tcp_listener(s->current.host, s->current.port, NULL, NULL) > 0 || port_free(s->current.host, s->current.port) == 0)) {
            unlock_registry(s); emit_error(op,"unmanaged_conflict","the new listening address is occupied; leaving the old session intact"); return -1;
        }
        if (old_live) {
            if (pin_signal(&oldid, 1) != 1) { unlock_registry(s); emit_error(op,"permission_denied","old registered SSH session could not be safely terminated"); return -1; }
            reclaimed = 1;
        } else if (tcp_listener(s->current.host,s->current.port,NULL,NULL) > 0 || port_free(s->current.host,s->current.port) == 0) {
            unlock_registry(s); emit_error(op,"unmanaged_conflict","old identity is gone but the requested listener remains occupied"); return -1;
        }
    }
    if (tcp_listener(s->current.host,s->current.port,NULL,NULL) > 0 || port_free(s->current.host,s->current.port) == 0) { unlock_registry(s); emit_error(op,"unmanaged_conflict","remote listening port is occupied by an unregistered process"); return -1; }
    s->helper.pid = getpid(); s->claimed = 1; char record[MAX_LINE]; if (make_record(s,record,sizeof(record),"claimed") < 0 || write_record(s->path,record) < 0) { s->claimed=0; unlock_registry(s); emit_error(op,"io_error","cannot durably write remote lease"); return -1; }
    unlock_registry(s);
    printf("{\"ok\":true,\"op\":\"claim\",\"protocol\":%d,\"reclaimed\":%s,\"session_pid\":%ld,\"generation\":%llu,\"session_id\":\"%s\"}\n", PROTOCOL, reclaimed?"true":"false", (long)s->session.pid, s->current.generation, s->current.session); fflush(stdout); return 0;
}
static int confirm_op(struct helper_state *s, const char *line, const char *op) {
    if (!s->claimed || !match_current(line, &s->current)) { emit_error(op,"invalid_request","claim must succeed before confirm"); return -1; }
    int ack = 0;
    if (json_bool(line, "forward_ack", &ack) < 0 || !ack) { emit_error(op,"ownership_mismatch","confirm requires the SSH forwarding success acknowledgement"); return -1; }
    if (registry(s, s->current.owner, s->current.rule) < 0) { emit_error(op,"permission_denied","cannot lock the private remote lease registry"); return -1; }
    char old[MAX_LINE];
    if (read_record(s->path, old, sizeof(old)) != 0 || !match_current(old, &s->current)) {
        unlock_registry(s); emit_error(op,"superseded","this helper no longer owns the current generation"); return -1;
    }
    struct identity cur;
    unsigned transport_uid; unsigned long transport_id;
    if (read_identity(s->session.pid, &cur) < 0 || !same_id(&s->session, &cur) ||
        transport_inode(&s->transport, &transport_uid, &transport_id) != 1 ||
        transport_uid != s->transport_uid || transport_id != s->transport_inode) {
        unlock_registry(s); emit_error(op,"ownership_mismatch","current SSH process or transport identity changed"); return -1;
    }
    unsigned uid; unsigned long inode;
    struct in_addr actual, requested;
    int bound = tcp_listener_info(s->current.host, s->current.port, &uid, &inode, &actual);
    if (bound <= 0) {
        unlock_registry(s); emit_error(op,"listener_missing","SSH has not established the registered reverse listener"); return -1;
    }
    if (parse_ipv4(s->current.host, &requested) || actual.s_addr != requested.s_addr) {
        unlock_registry(s); emit_error(op,"ownership_mismatch","sshd changed the requested bind address; check GatewayPorts"); return -1;
    }
    if (uid != s->session.uid) {
        unlock_registry(s); emit_error(op,"unmanaged_conflict","reverse listener UID does not match the SSH session"); return -1;
    }
    int listener_owner = listener_owned_by(s->session.pid, inode);
    int absent = 0; char registration[64];
    if (listener_owner != 1 && (listener_owner != -2 ||
        json_bool(old, "listener_absent_at_claim", &absent) < 0 || !absent ||
        json_string(old, "registration", registration, sizeof(registration)) < 0 ||
        strcmp(registration, "ssh_exec_ancestry") || !session_proof_matches(old, transport_uid, transport_id))) {
        unlock_registry(s); emit_error(op,"unmanaged_conflict","reverse listener inode is not owned by the registered SSH session"); return -1;
    }
    /* Persist exactly the socket checked above, never a new unverified query. */
    s->listener_uid = uid; s->listener_inode = inode;
    char record[MAX_LINE];
    if (make_record(s, record, sizeof(record), "confirmed") < 0 || write_record(s->path, record) < 0) {
        unlock_registry(s); emit_error(op,"io_error","cannot durably confirm remote lease"); return -1;
    }
    unlock_registry(s);
    printf("{\"ok\":true,\"op\":\"confirm\",\"protocol\":%d,\"generation\":%llu,\"session_id\":\"%s\",\"session_pid\":%ld}\n",
           PROTOCOL, s->current.generation, s->current.session, (long)s->session.pid);
    fflush(stdout); return 0;
}
static int release_op(struct helper_state *s, const char *line, const char *op) {
    if (!s->claimed || !match_current(line,&s->current)) { emit_error(op,"invalid_request","claim must succeed before release"); return -1; }
    if (registry(s,s->current.owner,s->current.rule)<0) { emit_error(op,"permission_denied","cannot lock the private remote lease registry"); return -1; }
    char old[MAX_LINE]; if (read_record(s->path,old,sizeof(old)) != 0 || !match_current(old,&s->current)) { unlock_registry(s); emit_error(op,"superseded","refusing to remove a newer session lease"); return -1; }
    if (unlink(s->path) < 0) { unlock_registry(s); emit_error(op,"io_error","cannot remove remote lease record"); return -1; } unlock_registry(s);
    printf("{\"ok\":true,\"op\":\"release\",\"protocol\":%d,\"generation\":%llu,\"session_id\":\"%s\"}\n",PROTOCOL,s->current.generation,s->current.session); fflush(stdout); return 1;
}
int main(int argc, char **argv) {
    if (argc > 0 && argv[0]) unlink(argv[0]);
    struct helper_state s; memset(&s,0,sizeof(s)); s.lock_fd=-1; char line[MAX_LINE+2];
    for (;;) {
        if (!fgets(line,sizeof(line),stdin)) return 0;
        size_t n=strlen(line); if (!n || line[n-1] != '\n' || n > MAX_LINE) { emit_error("", "invalid_request", "request exceeds the protocol line limit"); return 2; }
        char op[32] = ""; (void)json_string(line,"op",op,sizeof(op));
        int result = 0;
        if (!strcmp(op,"claim")) result=claim_op(&s,line,op);
        else if (!strcmp(op,"confirm")) result=confirm_op(&s,line,op);
        else if (!strcmp(op,"release")) result=release_op(&s,line,op);
        else emit_error(op,"invalid_request","unknown operation; expected claim, confirm, or release");
        if (result == 1) return 0;
    }
}
