/*
 * FWM macOS native recovery helper.
 *
 * This file is uploaded and executed as a standalone program by the Rust
 * client.  It uses only the stock Darwin libproc process/socket APIs: no
 * Python, lsof, shell utility, pidfd, launch daemon, or package is required.
 * The JSON protocol is line based and has the same claim/confirm/release
 * operations as the original helper.  IPv4 and IPv6 are supported.
 *
 * The source is kept small and auditable.  It is not a general JSON parser:
 * values accepted from the manager are restricted UUID/IP/decimal fields and
 * records are written by this program with those same restrictions.
 */
#include <arpa/inet.h>
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
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/file.h>
#include <sys/proc_info.h>
#include <libproc.h>
#include <sys/sysctl.h>
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
    char boot[128];
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
static int json_value(const char *json, const char *key, char *out, size_t cap, int string) {
    char needle[96];
    if (snprintf(needle, sizeof(needle), "\"%s\"", key) >= (int)sizeof(needle)) return -1;
    const char *p = strstr(json, needle);
    if (!p) return -1;
    p += strlen(needle); while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') p++;
    if (*p++ != ':') return -1;
    while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') p++;
    if (string) {
        if (*p++ != '"') return -1;
        size_t n = 0;
        while (*p && *p != '"') {
            if (*p == '\\' || (unsigned char)*p < 32 || n + 1 >= cap) return -1;
            out[n++] = *p++;
        }
        if (*p != '"') return -1;
        out[n] = 0; return 0;
    }
    size_t n = 0;
    while (*p && *p != ',' && *p != '}' && *p != '\n' && *p != ' ' && *p != '\t') {
        if (n + 1 >= cap) return -1; out[n++] = *p++;
    }
    out[n] = 0; return n ? 0 : -1;
}
static int json_string(const char *json, const char *key, char *out, size_t cap) {
    return json_value(json, key, out, cap, 1);
}
static int json_nested(const char *json, const char *object, const char *key, char *out, size_t cap, int string) {
    char needle[96]; if (snprintf(needle, sizeof(needle), "\"%s\"", object) >= (int)sizeof(needle)) return -1;
    const char *p = strstr(json, needle); if (!p) return -1; p = strchr(p, '{'); if (!p) return -1;
    return json_value(p, key, out, cap, string);
}
static int json_nested_u64(const char *json, const char *object, const char *key, unsigned long long *out) {
    char buf[64]; char *end; if (json_nested(json, object, key, buf, sizeof(buf), 0) < 0) return -1;
    errno = 0; *out = strtoull(buf, &end, 10); return errno || *end ? -1 : 0;
}
static int json_nested_string(const char *json, const char *object, const char *key, char *out, size_t cap) {
    return json_nested(json, object, key, out, cap, 1);
}
static int json_u64(const char *json, const char *key, unsigned long long *out) {
    char buf[64]; char *end;
    if (json_value(json, key, buf, sizeof(buf), 0) < 0 || !buf[0]) return -1;
    errno = 0; *out = strtoull(buf, &end, 10);
    return errno || *end ? -1 : 0;
}
static int json_bool(const char *json, const char *key, int *out) {
    char buf[8]; if (json_value(json, key, buf, sizeof(buf), 0) < 0) return -1;
    if (!strcmp(buf, "true")) { *out = 1; return 0; }
    if (!strcmp(buf, "false")) { *out = 0; return 0; }
    return -1;
}
static int named_u64_after(const char *start, const char *key, unsigned long long *value) {
    char needle[64]; if (snprintf(needle,sizeof(needle),"\"%s\"",key) >= (int)sizeof(needle)) return -1;
    const char *p=strstr(start,needle); if(!p)return -1; p+=strlen(needle); while(*p==' '||*p=='\t'||*p==':')p++;
    int quoted=0; if(*p=='"'){quoted=1;p++;} char *end; errno=0; *value=strtoull(p,&end,10);
    if(errno||end==p||(quoted&&*end!='"'))return -1; return 0;
}
static int uuid_ok(const char *s) {
    if (strlen(s) != 36) return 0;
    for (int i = 0; i < 36; i++) {
        if (i == 8 || i == 13 || i == 18 || i == 23) { if (s[i] != '-') return 0; }
        else if (!((s[i] >= '0' && s[i] <= '9') || (s[i] >= 'a' && s[i] <= 'f'))) return 0;
    }
    return 1;
}
/* Darwin process/socket identity ------------------------------------------------ */
static int parse_ip(const char *s, int *family, unsigned char *bytes) {
    if (inet_pton(AF_INET, s, bytes) == 1) { if (family) *family = AF_INET; return 0; }
    if (inet_pton(AF_INET6, s, bytes) == 1) {
        int mapped=1; for(int i=0;i<10;i++) if(bytes[i]) mapped=0;
        if(mapped && bytes[10]==0xff && bytes[11]==0xff){ memmove(bytes,bytes+12,4); if(family)*family=AF_INET; return 0; }
        if (family) *family = AF_INET6; return 0;
    }
    return -1;
}
static int endpoint_equal(const char *left, const char *right) {
    int lf, rf; unsigned char lb[16], rb[16];
    if (parse_ip(left,&lf,lb) || parse_ip(right,&rf,rb) || lf != rf) return 0;
    return !memcmp(lb,rb,lf == AF_INET ? 4 : 16);
}
static int endpoint_overlap(const char *left, const char *right) {
    int lf, rf; unsigned char lb[16], rb[16];
    if (parse_ip(left,&lf,lb) || parse_ip(right,&rf,rb)) return 0;
    size_t ln=lf==AF_INET?4:16, rn=rf==AF_INET?4:16; int lzero=1,rzero=1;
    for(size_t i=0;i<ln;i++) if(lb[i]) lzero=0;
    for(size_t i=0;i<rn;i++) if(rb[i]) rzero=0;
    if (lf != rf) return (lf == AF_INET6 && lzero) || (rf == AF_INET6 && rzero);
    return lzero || rzero || !memcmp(lb,rb,ln);
}
static int socket_address(const struct in_sockinfo *in, int local, char *out, size_t cap) {
    const void *addr = local ? (in->insi_vflag == INI_IPV4 ? (const void *)&in->insi_laddr.ina_46.i46a_addr4 : (const void *)&in->insi_laddr.ina_6)
                             : (in->insi_vflag == INI_IPV4 ? (const void *)&in->insi_faddr.ina_46.i46a_addr4 : (const void *)&in->insi_faddr.ina_6);
    int family = in->insi_vflag == INI_IPV4 ? AF_INET : AF_INET6;
    return inet_ntop(family, addr, out, (socklen_t)cap) ? 0 : -1;
}
static int socket_matches(const struct socket_info *si, const char *host, unsigned port, int listening) {
    if (si->soi_kind != SOCKINFO_TCP) return 0;
    const struct in_sockinfo *in = &si->soi_proto.pri_tcp.tcpsi_ini;
    if (ntohs((uint16_t)in->insi_lport) != port || (listening && si->soi_proto.pri_tcp.tcpsi_state != TSI_S_LISTEN)) return 0;
    char local[INET6_ADDRSTRLEN], remote[INET6_ADDRSTRLEN];
    if (socket_address(in,1,local,sizeof(local)) < 0 || !endpoint_overlap(local,host)) return 0;
    if (!listening) {
        if (socket_address(in,0,remote,sizeof(remote)) < 0) return 0;
    }
    return listening || remote[0];
}
static int socket_matches_transport(const struct socket_info *si, const struct transport *t) {
    if (si->soi_kind != SOCKINFO_TCP) return 0;
    const struct in_sockinfo *in = &si->soi_proto.pri_tcp.tcpsi_ini;
    if (ntohs((uint16_t)in->insi_lport) != t->server_port || ntohs((uint16_t)in->insi_fport) != t->client_port) return 0;
    char local[INET6_ADDRSTRLEN], remote[INET6_ADDRSTRLEN];
    return socket_address(in,1,local,sizeof(local)) == 0 && socket_address(in,0,remote,sizeof(remote)) == 0 &&
           endpoint_equal(local,t->server) && endpoint_equal(remote,t->client);
}
static int each_socket(pid_t pid, int (*callback)(pid_t, int, const struct socket_fdinfo *, void *), void *arg) {
    int cap = 256;
    struct proc_fdinfo *fds = NULL;
    for (;;) {
        fds = malloc((size_t)cap * sizeof(*fds)); if (!fds) return -1;
        int bytes = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds, cap * (int)sizeof(*fds));
        if (bytes <= 0) { free(fds); return bytes == 0 ? 0 : -1; }
        if (bytes < cap * (int)sizeof(*fds)) {
            int n = bytes / (int)sizeof(*fds);
            for (int i=0;i<n;i++) if (fds[i].proc_fdtype == PROX_FDTYPE_SOCKET) {
                struct socket_fdinfo info;
                int got = proc_pidfdinfo(pid, fds[i].proc_fd, PROC_PIDFDSOCKETINFO, &info, sizeof(info));
                if (got == (int)sizeof(info)) { int r = callback(pid, fds[i].proc_fd, &info, arg); if (r) { free(fds); return r; } }
            }
            free(fds); return 0;
        }
        free(fds); cap *= 2; if (cap > 65536) return -1;
    }
}
struct find_socket_args { const char *host; unsigned port; int listening; uint64_t handle; unsigned uid; int found; };
static int find_listener_cb(pid_t pid, int fd, const struct socket_fdinfo *info, void *opaque) {
    (void)fd; struct find_socket_args *a = opaque;
    if (socket_matches(&info->psi,a->host,a->port,a->listening)) { a->handle = info->psi.soi_so; a->found=1; return 1; }
    (void)pid; return 0;
}
static int find_listener_for_pid(pid_t pid, const char *host, unsigned port, unsigned *uid, unsigned long *inode) {
    struct find_socket_args args = {host,port,1,0,0,0};
    if (each_socket(pid,find_listener_cb,&args) < 0) return -1;
    if (!args.found) return 0;
    struct proc_bsdinfo bi; if (proc_pidinfo(pid,PROC_PIDTBSDINFO,0,&bi,sizeof(bi)) != (int)sizeof(bi)) return -1;
    if (uid) *uid = (unsigned)bi.pbi_uid; if (inode) *inode=(unsigned long)args.handle; return 1;
}
static int listener_owned_cb(pid_t ignored, int fd, const struct socket_fdinfo *info, void *opaque) {
    (void)ignored; (void)fd; struct find_socket_args *a=opaque;
    if (info->psi.soi_kind == SOCKINFO_TCP && (info->psi.soi_pcb == a->handle || info->psi.soi_so == a->handle)) { a->found=1; return 1; }
    return 0;
}
static int process_listener_owned(pid_t pid, uint64_t handle) {
    struct find_socket_args args = {0,0,1,handle,0,0};
    return each_socket(pid,listener_owned_cb,&args) < 0 ? -1 : args.found;
}
struct find_transport_args { const struct transport *transport; uint64_t handle; unsigned uid; int found; };
static int find_transport_cb(pid_t pid, int fd, const struct socket_fdinfo *info, void *opaque) {
    (void)fd; struct find_transport_args *a=opaque;
    if (socket_matches_transport(&info->psi,a->transport)) {
        struct proc_bsdinfo bi; if (proc_pidinfo(pid,PROC_PIDTBSDINFO,0,&bi,sizeof(bi)) != (int)sizeof(bi)) return 0;
        a->handle=info->psi.soi_so; a->uid=(unsigned)bi.pbi_uid; a->found=1; return 1;
    }
    return 0;
}
static int transport_inode_for_pid(pid_t pid, const struct transport *t, unsigned *uid_out, unsigned long *inode_out) {
    struct find_transport_args args={t,0,0,0};
    if (each_socket(pid,find_transport_cb,&args)<0) return -1;
    if (!args.found) return 0; if (uid_out) *uid_out=args.uid; if (inode_out) *inode_out=(unsigned long)args.handle; return 1;
}
static int list_pids(pid_t **out, int *count) {
    int cap=4096;
    for (;;) {
        pid_t *p=malloc((size_t)cap*sizeof(*p)); if(!p)return -1;
        int bytes=proc_listpids(PROC_ALL_PIDS,0,p,cap*(int)sizeof(*p));
        if(bytes<0){free(p);return -1;}
        if(bytes >= cap*(int)sizeof(*p) && cap < 262144){free(p);cap*=2;continue;}
        *out=p; *count=bytes/(int)sizeof(*p); return 0;
    }
}
static int transport_inode(const struct transport *t, unsigned *uid_out, unsigned long *inode_out) {
    pid_t *pids; int n; if(list_pids(&pids,&n)<0)return -1; int result=0;
    for(int i=0;i<n;i++) if(pids[i]>1 && transport_inode_for_pid(pids[i],t,uid_out,inode_out)==1){result=1;break;}
    free(pids); return result;
}
static int transport_present(const struct transport *t) { return transport_inode(t,NULL,NULL); }
static int transport_owned_by(pid_t pid, const struct transport *t) { return transport_inode_for_pid(pid,t,NULL,NULL); }
static int listener_owned_by(pid_t pid, unsigned long inode) { return process_listener_owned(pid,(uint64_t)inode); }
static int parse_claim(const char *line, struct claim *c) {
    unsigned long long n, port; int family; unsigned char addr[16];
    if (json_string(line, "owner_id", c->owner, sizeof(c->owner)) < 0 || !uuid_ok(c->owner) ||
        json_string(line, "rule_id", c->rule, sizeof(c->rule)) < 0 || !uuid_ok(c->rule) ||
        json_string(line, "session_id", c->session, sizeof(c->session)) < 0 || !uuid_ok(c->session) ||
        json_string(line, "listen_host", c->host, sizeof(c->host)) < 0 || parse_ip(c->host,&family,addr) < 0 ||
        json_u64(line, "generation", &n) < 0 || json_u64(line, "listen_port", &port) < 0 || port == 0 || port > 65535) return -1;
    c->generation = n; c->port=(unsigned)port; return 0;
}
static int read_identity(pid_t pid, struct identity *id) {
    struct proc_bsdinfo bi; if (pid <= 1 || proc_pidinfo(pid,PROC_PIDTBSDINFO,0,&bi,sizeof(bi)) != (int)sizeof(bi)) return -1;
    id->pid=(pid_t)bi.pbi_pid; id->ppid=(pid_t)bi.pbi_ppid; id->uid=bi.pbi_uid; id->start=bi.pbi_start_tvsec*1000000ULL+bi.pbi_start_tvusec;
    snprintf(id->boot,sizeof(id->boot),"mac:%llu:%llu",(unsigned long long)bi.pbi_start_tvsec,(unsigned long long)bi.pbi_start_tvusec);
    const char *name=bi.pbi_comm[0]?bi.pbi_comm:bi.pbi_name; snprintf(id->name,sizeof(id->name),"%s",name); id->state=(char)bi.pbi_status; return 0;
}
static int same_id(const struct identity *a, const struct identity *b) { return a&&b&&a->pid==b->pid&&a->uid==b->uid&&a->start==b->start&&!strcmp(a->boot,b->boot)&&!strcmp(a->name,b->name); }
static int allowed_name(const char *s) { return !strcmp(s,"sshd") || !strcmp(s,"sshd-session"); }
static int read_transport(struct transport *t) {
    unsigned cp,sp; const char *env=getenv("SSH_CONNECTION");
    if(!env || sscanf(env,"%63s %u %63s %u",t->client,&cp,t->server,&sp)!=4 || cp>65535||sp>65535||!cp||!sp) return -1;
    int f; unsigned char b[16]; if(parse_ip(t->client,&f,b)||parse_ip(t->server,&f,b)) return -2;
    t->client_port=cp;t->server_port=sp;return 0;
}
static int find_session(const struct transport *t, struct identity *found) {
    struct identity cur; if(read_identity(getpid(),&cur)<0)return -1; pid_t pid=cur.ppid; uid_t me=geteuid();
    for(int depth=0;depth<32&&pid>1;depth++){ struct identity next; if(read_identity(pid,&next)<0)return -1;
        if(allowed_name(next.name)&&next.uid==me&&transport_owned_by(next.pid,t)==1){*found=next;return 0;} pid=next.ppid; }
    return -1;
}
static int tcp_listener(const char *host, unsigned port, unsigned *uid, unsigned long *inode) {
    pid_t *pids; int n; if(list_pids(&pids,&n)<0)return -1; int result=0;
    for(int i=0;i<n&&!result;i++) if(pids[i]>1){int r=find_listener_for_pid(pids[i],host,port,uid,inode); if(r>0)result=1;}
    free(pids); return result;
}
static int port_free(const char *host, unsigned port) {
    int family; unsigned char raw[16]; if(parse_ip(host,&family,raw)<0)return -2; int fd=socket(family,SOCK_STREAM,0); if(fd<0)return -1; int one=1; setsockopt(fd,SOL_SOCKET,SO_REUSEADDR,&one,sizeof(one)); int r;
    if(family==AF_INET){struct sockaddr_in a;memset(&a,0,sizeof(a));a.sin_family=AF_INET;memcpy(&a.sin_addr,raw,4);a.sin_port=htons((uint16_t)port);r=bind(fd,(struct sockaddr*)&a,sizeof(a));}
    else {struct sockaddr_in6 a;memset(&a,0,sizeof(a));a.sin6_family=AF_INET6;memcpy(&a.sin6_addr,raw,16);a.sin6_port=htons((uint16_t)port);r=bind(fd,(struct sockaddr*)&a,sizeof(a));}
    int saved=errno;close(fd);errno=saved;return r==0?1:(errno==EADDRINUSE||errno==EACCES?0:-1);
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
        const char *state = getenv("XDG_STATE_HOME");
        const char *home = getenv("HOME");
        if (state && state[0] == '/') snprintf(fallback, sizeof(fallback), "%s/fwm", state);
        else if (home) snprintf(fallback, sizeof(fallback), "%s/.local/state/fwm", home);
        else return -1;
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
static int parse_birth(const char *birth, unsigned long long fallback, struct identity *id) {
    if (!birth || !*birth) return -1;
    snprintf(id->boot,sizeof(id->boot),"%s",birth); id->start=fallback;
    const char *p=birth; if (!strncmp(p,"mac:",4)) p += 4;
    char *end; errno=0; unsigned long long first=strtoull(p,&end,10);
    if (errno || end==p || *end!=':') return id->start ? 0 : -1;
    errno=0; unsigned long long second=strtoull(end+1,&end,10);
    if (errno || end==p || *end) return id->start ? 0 : -1;
    if (!strncmp(birth,"mac:",4)) {
        id->start = first*1000000ULL+second; return 0;
    }
    /* Python's Darwin adapter stores start_sec:start_usec without a boot tag. */
    if (second >= 1000000ULL) return id->start ? 0 : -1;
    snprintf(id->boot,sizeof(id->boot),"mac:%llu:%llu",first,second);
    id->start=first*1000000ULL+second; return 0;
}
static int old_identity(const char *record, struct identity *id, pid_t *pid) {
    unsigned long long n, uid, pid_value;
    int nested = json_nested_u64(record,"session","pid",&n)==0;
    memset(id,0,sizeof(*id));
    if (nested) {
        pid_value=n;
        if(n<2||n>INT_MAX||json_nested_u64(record,"session","uid",&uid)<0||uid>UINT_MAX||
           json_nested_string(record,"session","name",id->name,sizeof(id->name))<0) return -1;
        char birth[128]; if(json_nested_string(record,"session","birth",birth,sizeof(birth))<0||parse_birth(birth,0,id)<0) return -1;
        if(!allowed_name(id->name)) return -1;
    } else {
        if(json_u64(record,"session_pid",&pid_value)<0||pid_value<2||pid_value>INT_MAX||json_u64(record,"session_uid",&uid)<0||uid>UINT_MAX||
           json_u64(record,"session_start",&n)<0||json_string(record,"session_name",id->name,sizeof(id->name))<0||!allowed_name(id->name)) return -1;
        char birth[128]; if(json_string(record,"session_birth",birth,sizeof(birth))<0||parse_birth(birth,n,id)<0) return -1;
    }
    *pid=(pid_t)pid_value; id->pid=*pid; id->uid=(uid_t)uid; return 0;
}
static int old_transport(const char *record, struct transport *t) {
    const char *p = strstr(record, "\"transport\"");
    if (p) {
        p = strchr(p, '['); if (!p) return -1; p++;
        while (*p == ' ' || *p == '\t') p++;
        if (*p++ != '"') return -1; size_t n = 0;
        while (*p && *p != '"' && n + 1 < sizeof(t->client)) t->client[n++] = *p++;
        if (*p++ != '"') return -1; t->client[n] = 0;
        while (*p == ' ' || *p == '\t' || *p == ',') p++;
        char *end; errno = 0; unsigned long cp = strtoul(p, &end, 10); if (errno || end == p || cp > 65535 || !cp) return -1; p = end;
        while (*p == ' ' || *p == '\t' || *p == ',') p++;
        if (*p++ != '"') return -1; n = 0;
        while (*p && *p != '"' && n + 1 < sizeof(t->server)) t->server[n++] = *p++;
        if (*p++ != '"') return -1; t->server[n] = 0;
        while (*p == ' ' || *p == '\t' || *p == ',') p++;
        errno = 0; unsigned long sp = strtoul(p, &end, 10); if (errno || end == p || sp > 65535 || !sp) return -1;
        if (parse_ip(t->client, &(int){0}, (unsigned char[16]){0}) || parse_ip(t->server, &(int){0}, (unsigned char[16]){0})) return -1;
        t->client_port = (unsigned)cp; t->server_port = (unsigned)sp; return 0;
    }
    unsigned long long n;
    if (json_string(record,"transport_client",t->client,sizeof(t->client)) < 0 || json_string(record,"transport_server",t->server,sizeof(t->server)) < 0 ||
        json_u64(record,"transport_client_port",&n) < 0 || n == 0 || n > 65535) return -1;
    t->client_port = (unsigned)n;
    if (json_u64(record,"transport_server_port",&n) < 0 || n == 0 || n > 65535) return -1;
    t->server_port = (unsigned)n;
    return parse_ip(t->client, &(int){0}, (unsigned char[16]){0}) || parse_ip(t->server, &(int){0}, (unsigned char[16]){0}) ? -1 : 0;
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
        json_string(record,"listen_host",oldhost,oldhost_cap) < 0 || parse_ip(oldhost,&(int){0},(unsigned char[16]){0}) != 0 ||
        json_u64(record,"listen_port",&port) < 0 || !port || port > 65535 || old_identity(record,oldid,oldpid) < 0 ||
        json_string(record,"registration",registration,sizeof(registration)) < 0 || strcmp(registration,"ssh_exec_ancestry") ||
        old_transport(record,oldtransport) < 0 ||
        !strstr(record,"\"session_proof\"")) return -1;
    struct identity live;
    if (read_identity(*oldpid,&live) == 0 && same_id(oldid,&live)) {
        int transport_seen = transport_present(oldtransport);
        int ownership = transport_seen == 1 ? transport_owned_by(*oldpid, oldtransport) : 0;
        if (transport_seen != 1 || ownership == 0) return -1;
        if (ownership < 0) {
            unsigned proof_uid; unsigned long proof_inode;
            if (transport_inode(oldtransport,&proof_uid,&proof_inode) != 1 || !session_proof_matches(record,proof_uid,proof_inode)) return -1;
        }
    }
    if (!strcmp(phase,"confirmed") && !strstr(record,"\"listener_proof\"")) return -1;
    *oldport = (unsigned)port; return 0;
}
static int listener_proof_matches(const char *record, unsigned uid, unsigned long inode) {
    const char *p=strstr(record,"\"listener_proof\""); if(!p)return 0;
    unsigned long long expected_uid, expected_inode;
    if(named_u64_after(p,"uid",&expected_uid)==0 && expected_uid != uid)return 0;
    if(named_u64_after(p,"inode",&expected_inode)==0) return expected_inode == inode;
    /* Nested Python Darwin records identify the owning process/fd and do not
     * expose Darwin's opaque socket handle.  The caller independently checks
     * the old process birth identity and that it owns the live listener. */
    return strstr(p,"\"source\":\"process_fd\"") != NULL && strstr(p,"\"sockets\"") != NULL;
}
static int session_proof_matches(const char *record, unsigned uid, unsigned long inode) {
    const char *p=strstr(record,"\"session_proof\""); if(!p)return 0;
    unsigned long long proof_uid, proof_inode;
    if(named_u64_after(p,"uid",&proof_uid)==0 && proof_uid != uid)return 0;
    if(named_u64_after(p,"inode",&proof_inode)==0) return proof_inode == inode;
    /* Python's Darwin helper records a process-fd witness rather than a
     * Linux socket inode.  Live transport ownership was already checked by
     * the caller; retaining this durable witness preserves compatibility. */
    return strstr(p,"\"source\":\"process_fd\"") != NULL && strstr(p,"\"socket\"") != NULL;
}
static int pin_signal(const struct identity *expected, int recover) {
    struct identity now;
    if (read_identity(expected->pid,&now) < 0) return 0;
    if (!same_id(expected,&now)) return -1;
    for (int pass=0; pass<(recover?2:1); pass++) {
        if (read_identity(expected->pid,&now) < 0 || !same_id(expected,&now)) return 1;
        if (kill(expected->pid, pass ? SIGKILL : SIGTERM) < 0 && errno != ESRCH) return errno==EPERM ? -1 : 0;
        long long until=mono_ms()+(pass?KILL_MS:TERM_MS);
        while (mono_ms()<until) { struct identity cur; if(read_identity(expected->pid,&cur)<0 || !same_id(expected,&cur)) return 1; nap(25); }
    }
    return 0;
}
static int make_record(struct helper_state *s, char *out, size_t cap, const char *phase) {
    unsigned listener_uid=0; unsigned long listener_inode=0;
    int confirmed = !strcmp(phase,"confirmed");
    if (confirmed && tcp_listener(s->current.host,s->current.port,&listener_uid,&listener_inode) <= 0) return -1;
    int n;
    if (confirmed) n = snprintf(out, cap,
        "{\"protocol\":%d,\"phase\":\"%s\",\"owner_id\":\"%s\",\"rule_id\":\"%s\",\"generation\":%llu,\"session_id\":\"%s\",\"listen_host\":\"%s\",\"listen_port\":%u,\"session_pid\":%ld,\"session_uid\":%u,\"session_start\":%llu,\"session_birth\":\"%s\",\"session_name\":\"%s\",\"helper_pid\":%ld,\"transport_client\":\"%s\",\"transport_client_port\":%u,\"transport_server\":\"%s\",\"transport_server_port\":%u,\"session_proof\":{\"source\":\"ssh_exec_ancestry_inode\",\"uid\":%u,\"inode\":%lu},\"listener_absent_at_claim\":true,\"listener_proof\":{\"source\":\"ssh_forward_ack_inode\",\"uid\":%u,\"inode\":%lu},\"registration\":\"ssh_exec_ancestry\"}\n",
        PROTOCOL, phase, s->current.owner, s->current.rule, s->current.generation, s->current.session,
        s->current.host, s->current.port, (long)s->session.pid, (unsigned)s->session.uid,
        s->session.start, s->session.boot, s->session.name, (long)s->helper.pid, s->transport.client, s->transport.client_port,
        s->transport.server, s->transport.server_port, s->transport_uid, s->transport_inode, listener_uid, listener_inode);
    else n = snprintf(out, cap,
        "{\"protocol\":%d,\"phase\":\"%s\",\"owner_id\":\"%s\",\"rule_id\":\"%s\",\"generation\":%llu,\"session_id\":\"%s\",\"listen_host\":\"%s\",\"listen_port\":%u,\"session_pid\":%ld,\"session_uid\":%u,\"session_start\":%llu,\"session_birth\":\"%s\",\"session_name\":\"%s\",\"helper_pid\":%ld,\"transport_client\":\"%s\",\"transport_client_port\":%u,\"transport_server\":\"%s\",\"transport_server_port\":%u,\"session_proof\":{\"source\":\"ssh_exec_ancestry_inode\",\"uid\":%u,\"inode\":%lu},\"listener_absent_at_claim\":true,\"registration\":\"ssh_exec_ancestry\"}\n",
        PROTOCOL, phase, s->current.owner, s->current.rule, s->current.generation, s->current.session,
        s->current.host, s->current.port, (long)s->session.pid, (unsigned)s->session.uid,
        s->session.start, s->session.boot, s->session.name, (long)s->helper.pid, s->transport.client, s->transport.client_port,
        s->transport.server, s->transport.server_port, s->transport_uid, s->transport_inode);
    return n >= 0 && (size_t)n < cap ? 0 : -1;
}
static int match_current(const char *line, const struct claim *c) {
    char owner[37], rule[37], sid[37]; unsigned long long gen;
    return json_string(line,"owner_id",owner,sizeof(owner))==0&&json_string(line,"rule_id",rule,sizeof(rule))==0&&json_string(line,"session_id",sid,sizeof(sid))==0&&json_u64(line,"generation",&gen)==0&&!strcmp(owner,c->owner)&&!strcmp(rule,c->rule)&&!strcmp(sid,c->session)&&gen==c->generation;
}
static int claim_op(struct helper_state *s, const char *line, char *op) {
    if (s->claimed) { emit_error(op,"invalid_request","this helper already claimed a rule"); return -1; }
    if (parse_claim(line, &s->current) < 0) { emit_error(op,"invalid_request","claim fields are invalid (UUID, IP, generation, or port)"); return -1; }
    int tr = read_transport(&s->transport);
    if (tr < 0 || find_session(&s->transport, &s->session) < 0 || s->session.uid != geteuid() ||
        transport_inode_for_pid(s->session.pid, &s->transport, &s->transport_uid, &s->transport_inode) != 1) { emit_error(op,"ownership_mismatch","cannot identify a same-user ancestor SSH session and its SSH_CONNECTION transport"); return -1; }
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
            (old_bound > 0 && listener_owner == 0) ||
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
    if (!s->claimed || !match_current(line,&s->current)) { emit_error(op,"invalid_request","claim must succeed before confirm"); return -1; }
    int ack=0; if (json_bool(line,"forward_ack",&ack)<0 || !ack) { emit_error(op,"ownership_mismatch","confirm requires the SSH forwarding success acknowledgement"); return -1; }
    struct identity cur; if (read_identity(s->session.pid,&cur)<0 || !same_id(&s->session,&cur)) { emit_error(op,"ownership_mismatch","current SSH process identity changed"); return -1; }
    unsigned uid; unsigned long inode; int bound=tcp_listener(s->current.host,s->current.port,&uid,&inode); if(bound<=0) { emit_error(op,"listener_missing","SSH has not established the registered reverse listener"); return -1; }
    if (uid != s->session.uid) { emit_error(op,"unmanaged_conflict","reverse listener UID does not match the SSH session"); return -1; }
    if (registry(s,s->current.owner,s->current.rule)<0) { emit_error(op,"permission_denied","cannot lock the private remote lease registry"); return -1; }
    char old[MAX_LINE]; if (read_record(s->path,old,sizeof(old)) != 0 || !match_current(old,&s->current)) { unlock_registry(s); emit_error(op,"superseded","this helper no longer owns the current generation"); return -1; }
    int listener_owner = listener_owned_by(s->session.pid,inode);
    if (listener_owner == 0 || (listener_owner < 0 &&
        (!strstr(old,"\"listener_absent_at_claim\":true") || !strstr(old,"\"registration\":\"ssh_exec_ancestry\"") || !strstr(old,"\"session_proof\"")))) {
        unlock_registry(s); emit_error(op,"unmanaged_conflict","reverse listener inode is not owned by the registered SSH session"); return -1;
    }
    char record[MAX_LINE]; if (make_record(s,record,sizeof(record),"confirmed") < 0 || write_record(s->path,record) < 0) { unlock_registry(s); emit_error(op,"io_error","cannot durably confirm remote lease"); return -1; } unlock_registry(s);
    printf("{\"ok\":true,\"op\":\"confirm\",\"protocol\":%d,\"generation\":%llu,\"session_id\":\"%s\",\"session_pid\":%ld}\n",PROTOCOL,s->current.generation,s->current.session,(long)s->session.pid); fflush(stdout); return 0;
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
