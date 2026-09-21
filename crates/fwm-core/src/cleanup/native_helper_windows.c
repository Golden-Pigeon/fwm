/*
 * FWM Windows native recovery helper.
 *
 * This program is uploaded over the authenticated OpenSSH session and serves
 * the same JSON-lines claim/confirm/release protocol as the Unix helpers.  It
 * deliberately has no runtime dependency: all process, TCP and file
 * inspection is performed with Win32 APIs.  The helper supports Windows
 * OpenSSH's IPv4 and IPv6 transport tables and only terminates an sshd.exe
 * process whose creation-time, account SID, image name and owning transport
 * were recorded under the same manager/rule lease.
 *
 * A Windows process has no pidfd or portable SIGTERM equivalent.  Recovery
 * therefore opens a PROCESS_TERMINATE handle only after rechecking the full
 * identity and transport proof.  TerminateProcess is issued through that
 * pinned handle (never through a PID), and the handle is waited on before the
 * listener is considered reclaimable.  If any identity or ownership check is
 * unavailable the helper returns needs_attention/unsupported instead of
 * falling back to a PID-only kill.
 */
#define _CRT_SECURE_NO_WARNINGS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <tlhelp32.h>
#include <winsock2.h>
#include <ws2tcpip.h>
#include <iphlpapi.h>
#include <sddl.h>
#include <aclapi.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>
#include <time.h>

#ifdef _MSC_VER
#pragma comment(lib, "ws2_32.lib")
#pragma comment(lib, "iphlpapi.lib")
#pragma comment(lib, "advapi32.lib")
#endif

#define PROTOCOL 1
#define MAX_LINE 65536
#define PATH_CAP 4096
#define SID_CAP 192
#define NAME_CAP 260
#define TERM_MS 2000
#define KILL_MS 2000

struct identity {
    DWORD pid, ppid;
    ULONGLONG birth;
    char sid[SID_CAP];
    char name[NAME_CAP];
};

struct endpoint {
    int family;
    BYTE address[16];
    unsigned port;
};

struct transport { struct endpoint client, server; };

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
    HANDLE lock_handle;
    DWORD transport_pid;
    int claimed;
};

static int is_descendant_of(DWORD child,DWORD ancestor);

static void emit_json_text(const char *text) {
    putchar('"');
    for (const unsigned char *p=(const unsigned char *)(text ? text : ""); *p; p++) {
        if (*p=='"' || *p=='\\') printf("\\%c", *p);
        else if (*p=='\n') fputs("\\n", stdout);
        else if (*p=='\r') fputs("\\r", stdout);
        else if (*p=='\t') fputs("\\t", stdout);
        else if (*p<32) printf("\\u%04x", *p);
        else putchar(*p);
    }
    putchar('"');
}
static void emit_error(const char *op,const char *code,const char *message) {
    fputs("{\"ok\":false,\"op\":",stdout); emit_json_text(op);
    printf(",\"protocol\":%d,\"code\":",PROTOCOL); emit_json_text(code);
    fputs(",\"message\":",stdout); emit_json_text(message); fputs("}\n",stdout); fflush(stdout);
}

/* This restricted parser accepts only the canonical records emitted here and
 * by the existing Python/Unix helpers.  No escape sequences are accepted in
 * identity fields, which prevents key/value confusion in durable records. */
static int json_value(const char *json,const char *key,char *out,size_t cap,int string) {
    char needle[96]; const char *p; size_t n=0;
    if (snprintf(needle,sizeof(needle),"\"%s\"",key) >= (int)sizeof(needle)) return -1;
    p=strstr(json,needle); if(!p) return -1; p+=strlen(needle);
    while(*p==' '||*p=='\t'||*p=='\r'||*p=='\n')p++; if(*p++!=':')return -1;
    while(*p==' '||*p=='\t'||*p=='\r'||*p=='\n')p++;
    if(string){
        if(*p++!='"')return -1;
        while(*p && *p!='"'){ if(*p=='\\'||(unsigned char)*p<32||n+1>=cap)return -1; out[n++]=*p++; }
        if(*p!='"')return -1; out[n]=0; return 0;
    }
    while(*p && *p!=','&&*p!='}'&&*p!='\n'&&*p!=' '&&*p!='\t'){if(n+1>=cap)return -1;out[n++]=*p++;}
    out[n]=0; return n?0:-1;
}
static int json_nested(const char *json,const char *object,const char *key,char *out,size_t cap,int string){
    char needle[96]; const char *p; if(snprintf(needle,sizeof(needle),"\"%s\"",object)>=(int)sizeof(needle))return -1;
    p=strstr(json,needle); if(!p || !(p=strchr(p,'{')))return -1; return json_value(p,key,out,cap,string);
}
static int json_string(const char *j,const char *k,char *o,size_t c){return json_value(j,k,o,c,1);}
static int json_u64(const char *j,const char *k,unsigned long long *o){char b[64],*e;if(json_value(j,k,b,sizeof(b),0)<0)return -1;errno=0;*o=_strtoui64(b,&e,10);return errno||*e?-1:0;}
static int json_nested_u64(const char *j,const char *obj,const char *k,unsigned long long *o){char b[64],*e;if(json_nested(j,obj,k,b,sizeof(b),0)<0)return -1;errno=0;*o=_strtoui64(b,&e,10);return errno||*e?-1:0;}
static int json_nested_string(const char *j,const char *obj,const char *k,char *o,size_t c){return json_nested(j,obj,k,o,c,1);}
static int json_bool(const char *j,const char *k,int *o){char b[8];if(json_value(j,k,b,sizeof(b),0)<0)return -1;if(!strcmp(b,"true")){*o=1;return 0;}if(!strcmp(b,"false")){*o=0;return 0;}return -1;}
static int uuid_ok(const char *s){size_t i;if(strlen(s)!=36)return 0;for(i=0;i<36;i++){if(i==8||i==13||i==18||i==23){if(s[i]!='-')return 0;}else if(!((s[i]>='0'&&s[i]<='9')||(s[i]>='a'&&s[i]<='f')))return 0;}return 1;}

static int parse_endpoint(const char *host,unsigned port,struct endpoint *out){
    if(!host||!out||port==0||port>65535)return -1;
    memset(out,0,sizeof(*out)); out->port=port;
    if(InetPtonA(AF_INET,host,out->address)==1){out->family=AF_INET;return 0;}
    if(InetPtonA(AF_INET6,host,out->address)==1){out->family=AF_INET6;return 0;}
    return -1;
}
static int endpoint_equal(const struct endpoint*a,const struct endpoint*b){return a->family==b->family&&a->port==b->port&&!memcmp(a->address,b->address,a->family==AF_INET?4:16);}
static int endpoint_to_text(const struct endpoint*e,char*out,size_t cap){if(!InetNtopA(e->family,e->address,out,(DWORD)cap))return -1;return 0;}
static int endpoint_matches_local(const struct endpoint *observed,const struct endpoint *requested){
    if(observed->family!=requested->family||observed->port!=requested->port)return 0;
    if(!memcmp(observed->address,requested->address,requested->family==AF_INET?4:16))return 1;
    {static const BYTE z[16]={0};if(!memcmp(observed->address,z,requested->family==AF_INET?4:16))return 1;}
    if(requested->family==AF_INET){DWORD x;memcpy(&x,requested->address,4);return x==0;}
    return !memcmp(requested->address,(const BYTE[16]){0},16);
}
static int parse_ssh_connection(struct transport *t){
    char env[512],a[128],b[128];unsigned cp,sp;DWORD n=GetEnvironmentVariableA("SSH_CONNECTION",env,sizeof(env));
    if(!n||n>=sizeof(env)||sscanf(env,"%127s %u %127s %u",a,&cp,b,&sp)!=4||!cp||!sp||cp>65535||sp>65535)return -1;
    if(parse_endpoint(a,cp,&t->client)<0||parse_endpoint(b,sp,&t->server)<0)return -1; return 0;
}

static int sid_for_token(HANDLE token,char *out,size_t cap){DWORD n=0;PTOKEN_USER u=NULL;char *s=NULL;int r=-1;if(!GetTokenInformation(token,TokenUser,NULL,0,&n)&&GetLastError()!=ERROR_INSUFFICIENT_BUFFER)return -1;u=(PTOKEN_USER)HeapAlloc(GetProcessHeap(),0,n);if(!u)return -1;if(!GetTokenInformation(token,TokenUser,u,n,&n))goto done;if(!ConvertSidToStringSidA(u->User.Sid,&s))goto done;if(strlen(s)+1>cap)goto done;strcpy(out,s);r=0;done:if(s)LocalFree(s);if(u)HeapFree(GetProcessHeap(),0,u);return r;}
static int process_identity_handle(HANDLE h,struct identity *id){FILETIME c,e,k,u;DWORD n=(DWORD)sizeof(id->name);HANDLE tok=NULL;memset(id,0,sizeof(*id));if(!GetProcessTimes(h,&c,&e,&k,&u)||!QueryFullProcessImageNameA(h,0,id->name,&n))return -1;id->birth=((ULONGLONG)c.dwHighDateTime<<32)|c.dwLowDateTime;if(!OpenProcessToken(h,TOKEN_QUERY,&tok))return -1;if(sid_for_token(tok,id->sid,sizeof(id->sid))<0){CloseHandle(tok);return -1;}CloseHandle(tok);char *base=strrchr(id->name,'\\');if(base)memmove(id->name,base+1,strlen(base+1)+1);return 0;}
static int process_identity(DWORD pid,struct identity*id,DWORD access){HANDLE h=OpenProcess(access,FALSE,pid);if(!h)return 0;int r=process_identity_handle(h,id);if(r==0)id->pid=pid;CloseHandle(h);if(r<0)return -1;return 1;}
static int same_identity(const struct identity*a,const struct identity*b){return a&&b&&a->pid==b->pid&&a->birth==b->birth&&!strcmp(a->sid,b->sid)&&!_stricmp(a->name,b->name);}
static int allowed_name(const char*n){return !_stricmp(n,"sshd.exe")||!_stricmp(n,"sshd");}

struct proc_entry {DWORD pid,ppid;char name[NAME_CAP];};
static int parent_of(DWORD pid,struct proc_entry*out){HANDLE snap=CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0);PROCESSENTRY32W e;int found=0;if(snap==INVALID_HANDLE_VALUE)return -1;e.dwSize=sizeof(e);if(Process32FirstW(snap,&e))do{if(e.th32ProcessID==pid){out->pid=e.th32ParentProcessID;out->ppid=pid;WideCharToMultiByte(CP_UTF8,0,e.szExeFile,-1,out->name,sizeof(out->name),NULL,NULL);found=1;break;}}while(Process32NextW(snap,&e));CloseHandle(snap);return found?0:-1;}
static int is_descendant_of(DWORD child,DWORD ancestor){
    for(int depth=0;depth<32&&child>1;depth++){
        if(child==ancestor)return 1;
        struct proc_entry p;if(parent_of(child,&p)<0)return 0;
        if(p.pid==child)return 0; child=p.pid;
    }
    return child==ancestor;
}

/* Enumerate TCP rows and invoke callback on each row. */
typedef int (*tcp_cb)(const struct endpoint*,const struct endpoint*,DWORD,DWORD,void*);
static int tcp_table(int family,tcp_cb cb,void*arg){ULONG size=0,err;PMIB_TCPTABLE_OWNER_PID table=NULL;PMIB_TCP6TABLE_OWNER_PID table6=NULL;DWORD i;int result=-1;
    if(family==AF_INET){err=GetExtendedTcpTable(NULL,&size,FALSE,AF_INET,TCP_TABLE_OWNER_PID_ALL,0);if(err!=ERROR_INSUFFICIENT_BUFFER)return -1;table=(PMIB_TCPTABLE_OWNER_PID)HeapAlloc(GetProcessHeap(),0,size);if(!table)return -1;if(GetExtendedTcpTable(table,&size,FALSE,AF_INET,TCP_TABLE_OWNER_PID_ALL,0)!=NO_ERROR)goto done;for(i=0;i<table->dwNumEntries;i++){MIB_TCPROW_OWNER_PID*x=&table->table[i];struct endpoint l,r;memset(&l,0,sizeof(l));memset(&r,0,sizeof(r));l.family=r.family=AF_INET;memcpy(l.address,&x->dwLocalAddr,4);memcpy(r.address,&x->dwRemoteAddr,4);l.port=ntohs((u_short)x->dwLocalPort);r.port=ntohs((u_short)x->dwRemotePort);if(cb(&l,&r,x->dwOwningPid,x->dwState,arg)) {result=1;goto done;}}} else {err=GetExtendedTcpTable(NULL,&size,FALSE,AF_INET6,TCP_TABLE_OWNER_PID_ALL,0);if(err!=ERROR_INSUFFICIENT_BUFFER)return -1;table6=(PMIB_TCP6TABLE_OWNER_PID)HeapAlloc(GetProcessHeap(),0,size);if(!table6)return -1;if(GetExtendedTcpTable(table6,&size,FALSE,AF_INET6,TCP_TABLE_OWNER_PID_ALL,0)!=NO_ERROR)goto done;for(i=0;i<table6->dwNumEntries;i++){MIB_TCP6ROW_OWNER_PID*x=&table6->table[i];struct endpoint l,r;memset(&l,0,sizeof(l));memset(&r,0,sizeof(r));l.family=r.family=AF_INET6;memcpy(l.address,x->ucLocalAddr,16);memcpy(r.address,x->ucRemoteAddr,16);l.port=ntohs((u_short)x->dwLocalPort);r.port=ntohs((u_short)x->dwRemotePort);if(cb(&l,&r,x->dwOwningPid,x->dwState,arg)){result=1;goto done;}}}result=0;done:if(table)HeapFree(GetProcessHeap(),0,table);if(table6)HeapFree(GetProcessHeap(),0,table6);return result;}

struct find_transport {const struct transport*t;DWORD pid;int found;};
static int find_transport_cb(const struct endpoint*l,const struct endpoint*r,DWORD pid,DWORD state,void*arg){struct find_transport*x=arg;if(state==MIB_TCP_STATE_ESTAB&&endpoint_equal(l,&x->t->server)&&endpoint_equal(r,&x->t->client)){x->pid=pid;x->found=1;return 1;}return 0;}
static int transport_owner(const struct transport*t,DWORD*pid){struct find_transport x={t,0,0};if(tcp_table(t->server.family,find_transport_cb,&x)<0)return -1;if(!x.found)return 0;if(pid)*pid=x.pid;return 1;}
struct find_listener {const struct endpoint*want;DWORD pid;int found,foreign;};
static int find_listener_cb(const struct endpoint*l,const struct endpoint*r,DWORD pid,DWORD state,void*arg){struct find_listener*x=arg;(void)r;if(state==MIB_TCP_STATE_LISTEN&&endpoint_matches_local(l,x->want)){if(!x->found)x->pid=pid;x->found=1;if(x->pid!=pid)x->foreign=1;}return 0;}
static int listener_info(const char*host,unsigned port,DWORD*pid){struct endpoint e;if(parse_endpoint(host,port,&e)<0)return -1;struct find_listener x={&e,0,0,0};if(tcp_table(e.family,find_listener_cb,&x)<0)return -1;if(!x.found)return 0;if(x.foreign)return 2;if(pid)*pid=x.pid;return 1;}
static int listener_owned_by_session(DWORD session_pid,DWORD transport_pid,const char*host,unsigned port){
    DWORD owner=0;int r=listener_info(host,port,&owner);if(r==0)return 0;if(r!=1)return -1;
    if(owner==session_pid)return 1;
    if(owner==transport_pid){struct identity id;if(process_identity(owner,&id,PROCESS_QUERY_LIMITED_INFORMATION)==1&&allowed_name(id.name)&&is_descendant_of(session_pid,owner))return 1;}
    return -1;
}
static int port_free(const char*host,unsigned port){struct endpoint e;if(parse_endpoint(host,port,&e)<0)return -1;SOCKET s=WSASocketA(e.family,SOCK_STREAM,IPPROTO_TCP,NULL,0,0);if(s==INVALID_SOCKET)return -1;BOOL yes=TRUE;setsockopt(s,SOL_SOCKET,SO_REUSEADDR,(const char*)&yes,sizeof(yes));int ok=-1;if(e.family==AF_INET){struct sockaddr_in a;memset(&a,0,sizeof(a));a.sin_family=AF_INET;a.sin_port=htons((u_short)port);memcpy(&a.sin_addr,e.address,4);ok=bind(s,(struct sockaddr*)&a,sizeof(a))==0?1:(WSAGetLastError()==WSAEADDRINUSE?0:-1);}else{struct sockaddr_in6 a;memset(&a,0,sizeof(a));a.sin6_family=AF_INET6;a.sin6_port=htons((u_short)port);memcpy(&a.sin6_addr,e.address,16);ok=bind(s,(struct sockaddr*)&a,sizeof(a))==0?1:(WSAGetLastError()==WSAEADDRINUSE?0:-1);}closesocket(s);return ok;}

static int find_session(const struct transport*t,struct identity*found){
    struct identity helper,owner_id; DWORD pid=GetCurrentProcessId(),owner=0;
    if(process_identity(pid,&helper,PROCESS_QUERY_LIMITED_INFORMATION)!=1)return -1;
    /* Windows OpenSSH keeps the TCP transport on its service-side sshd
     * process while the authenticated command/forward runs in a child
     * sshd.exe.  Require the transport PID to be a real sshd image, then
     * locate the same-account sshd ancestor that owns the session.  The
     * listener check below still binds a forward to the child PID, so this
     * does not broaden termination to the service process. */
    int transport_ok=transport_owner(t,&owner);
    if(transport_ok!=1||process_identity(owner,&owner_id,PROCESS_QUERY_LIMITED_INFORMATION)!=1||!allowed_name(owner_id.name))return -1;
    for(int depth=0;depth<32&&pid>1;depth++){
        struct proc_entry p;if(parent_of(pid,&p)<0)return -1;pid=p.pid;
        struct identity id;if(process_identity(pid,&id,PROCESS_QUERY_LIMITED_INFORMATION)!=1)continue;
        if(allowed_name(id.name)&&!strcmp(id.sid,helper.sid)&&is_descendant_of(id.pid,owner)){*found=id;return 0;}
    }
    return -1;
}

static int no_reparse_path(const char*path){DWORD a=GetFileAttributesA(path);return a==INVALID_FILE_ATTRIBUTES?0:((a&FILE_ATTRIBUTE_REPARSE_POINT)?-1:1);}
static int ensure_dir(const char*path){char buf[PATH_CAP];size_t n=strlen(path);if(n>=sizeof(buf))return -1;strcpy(buf,path);for(char*p=buf+3;*p;p++){if(*p!='\\'&&*p!='/')continue;char save=*p;*p=0;if(*buf&&CreateDirectoryA(buf,NULL)==0&&GetLastError()!=ERROR_ALREADY_EXISTS){*p=save;return -1;}*p=save;if(no_reparse_path(buf)<0)return -1;}if(CreateDirectoryA(buf,NULL)==0&&GetLastError()!=ERROR_ALREADY_EXISTS)return -1;return no_reparse_path(buf)<0?-1:0;}
static int registry(struct helper_state*s,const char*owner,const char*rule){char base[PATH_CAP],home[PATH_CAP],leases[PATH_CAP],dir[PATH_CAP];DWORD n=GetEnvironmentVariableA("FWM_REMOTE_STATE_DIR",base,sizeof(base));if(!n||n>=sizeof(base)){n=GetEnvironmentVariableA("LOCALAPPDATA",home,sizeof(home));if(!n||n>=sizeof(home)){n=GetEnvironmentVariableA("USERPROFILE",home,sizeof(home));if(!n||n>=sizeof(home))return -1;}if(snprintf(base,sizeof(base),"%s\\fwm",home)>=(int)sizeof(base))return -1;}if(ensure_dir(base)<0)return -1;if(snprintf(leases,sizeof(leases),"%s\\leases",base)>=(int)sizeof(leases)||ensure_dir(leases)<0)return -1;if(snprintf(dir,sizeof(dir),"%s\\%s",leases,owner)>=(int)sizeof(dir)||ensure_dir(dir)<0)return -1;if(snprintf(s->directory,sizeof(s->directory),"%s",dir)>=PATH_CAP||snprintf(s->path,sizeof(s->path),"%s\\%s.json",dir,rule)>=PATH_CAP||snprintf(s->lock_path,sizeof(s->lock_path),"%s\\%s.lock",dir,rule)>=PATH_CAP)return -1;s->lock_handle=CreateFileA(s->lock_path,GENERIC_READ|GENERIC_WRITE,0,NULL,OPEN_ALWAYS,FILE_ATTRIBUTE_NORMAL|FILE_FLAG_OPEN_REPARSE_POINT,NULL);if(s->lock_handle==INVALID_HANDLE_VALUE){s->lock_handle=NULL;return -1;}BY_HANDLE_FILE_INFORMATION fi;if(!GetFileInformationByHandle(s->lock_handle,&fi)||(fi.dwFileAttributes&(FILE_ATTRIBUTE_REPARSE_POINT|FILE_ATTRIBUTE_DIRECTORY))||fi.nNumberOfLinks!=1){CloseHandle(s->lock_handle);s->lock_handle=NULL;return -1;}return 0;}
static void unlock_registry(struct helper_state*s){if(s->lock_handle){CloseHandle(s->lock_handle);s->lock_handle=NULL;}}
static int read_file(const char*path,char*buf,size_t cap){HANDLE h=CreateFileA(path,GENERIC_READ,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,NULL,OPEN_EXISTING,FILE_ATTRIBUTE_NORMAL|FILE_FLAG_OPEN_REPARSE_POINT,NULL);if(h==INVALID_HANDLE_VALUE)return GetLastError()==ERROR_FILE_NOT_FOUND?1:-1;BY_HANDLE_FILE_INFORMATION fi;if(!GetFileInformationByHandle(h,&fi)||(fi.dwFileAttributes&(FILE_ATTRIBUTE_REPARSE_POINT|FILE_ATTRIBUTE_DIRECTORY))||fi.nNumberOfLinks!=1||fi.nFileSizeHigh!=0||fi.nFileSizeLow>=cap){CloseHandle(h);return -1;}DWORD got=0;if(!ReadFile(h,buf,(DWORD)(cap-1),&got,NULL)){CloseHandle(h);return -1;}CloseHandle(h);buf[got]=0;return 0;}
static int write_file(const char*path,const char*text){char tmp[PATH_CAP];if(snprintf(tmp,sizeof(tmp),"%s.tmp-%lu",path,(unsigned long)GetCurrentProcessId())>=PATH_CAP)return -1;HANDLE h=CreateFileA(tmp,GENERIC_WRITE,0,NULL,CREATE_NEW,FILE_ATTRIBUTE_NORMAL|FILE_FLAG_OPEN_REPARSE_POINT,NULL);if(h==INVALID_HANDLE_VALUE)return -1;DWORD n=(DWORD)strlen(text),w=0;int ok=WriteFile(h,text,n,&w,NULL)&&w==n;FlushFileBuffers(h);CloseHandle(h);if(!ok){DeleteFileA(tmp);return -1;}if(!MoveFileExA(tmp,path,MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH)){DeleteFileA(tmp);return -1;}return 0;}

static int old_identity(const char*record,struct identity*id,DWORD*pid){unsigned long long p,b;char uid[SID_CAP],birth[128];memset(id,0,sizeof(*id));if(json_nested_u64(record,"session","pid",&p)==0){if(json_nested_string(record,"session","birth",birth,sizeof(birth))<0||json_nested_string(record,"session","name",id->name,sizeof(id->name))<0)return -1;if(json_nested_string(record,"session","uid",uid,sizeof(uid))<0){unsigned long long u;if(json_nested_u64(record,"session","uid",&u)<0)return -1;snprintf(uid,sizeof(uid),"%llu",u);}id->pid=(DWORD)p;id->birth=_strtoui64(birth,NULL,10);snprintf(id->sid,sizeof(id->sid),"%s",uid);}else{if(json_u64(record,"session_pid",&p)<0||json_u64(record,"session_start",&b)<0||json_string(record,"session_name",id->name,sizeof(id->name))<0)return -1;id->pid=(DWORD)p;id->birth=b;if(json_string(record,"session_sid",id->sid,sizeof(id->sid))<0){unsigned long long u;if(json_u64(record,"session_uid",&u)<0)return -1;snprintf(id->sid,sizeof(id->sid),"%llu",u);}}if(id->pid<2||!id->birth||!allowed_name(id->name))return -1;*pid=id->pid;return 0;}
static int old_transport(const char*record,struct transport*t){char c[128],s[128];unsigned long long cp,sp;if(strstr(record,"\"transport\"")){const char*p=strstr(record,"\"transport\"");p=strchr(p,'[');if(!p)return -1;p++;if(sscanf(p," \"%127[^\"]\" , %llu , \"%127[^\"]\" , %llu",c,&cp,s,&sp)!=4)return -1;}else{if(json_string(record,"transport_client",c,sizeof(c))<0||json_string(record,"transport_server",s,sizeof(s))<0||json_u64(record,"transport_client_port",&cp)<0||json_u64(record,"transport_server_port",&sp)<0)return -1;}if(cp>65535||sp>65535||parse_endpoint(c,(unsigned)cp,&t->client)<0||parse_endpoint(s,(unsigned)sp,&t->server)<0)return -1;return 0;}
static int validate_old(const char*r,const struct claim*q,struct identity*old,DWORD*pid,char*host,size_t hc,unsigned*port,struct transport*t){char phase[32],owner[37],rule[37],sid[37],registration[64];unsigned long long gen,p;if(json_u64(r,"protocol",&p)<0||p!=PROTOCOL||json_string(r,"phase",phase,sizeof(phase))<0||(strcmp(phase,"claimed")&&strcmp(phase,"confirmed"))||json_string(r,"owner_id",owner,sizeof(owner))<0||json_string(r,"rule_id",rule,sizeof(rule))<0||json_string(r,"session_id",sid,sizeof(sid))<0||!uuid_ok(owner)||!uuid_ok(rule)||!uuid_ok(sid)||strcmp(owner,q->owner)||strcmp(rule,q->rule)||json_u64(r,"generation",&gen)<0||gen>=q->generation||json_string(r,"listen_host",host,hc)<0||json_u64(r,"listen_port",&p)<0||p==0||p>65535||old_identity(r,old,pid)<0||json_string(r,"registration",registration,sizeof(registration))<0||strcmp(registration,"ssh_exec_ancestry")||old_transport(r,t)<0)return -1;*port=(unsigned)p;return 0;}

static int pin_and_terminate(const struct identity*expected){HANDLE h=OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_TERMINATE|SYNCHRONIZE,FALSE,expected->pid);if(!h)return 0;struct identity now;if(process_identity_handle(h,&now)<0){CloseHandle(h);return -1;}now.pid=expected->pid;if(!same_identity(expected,&now)){CloseHandle(h);return -1;}if(!TerminateProcess(h,1)){CloseHandle(h);return -1;}DWORD wait=WaitForSingleObject(h,TERM_MS);if(wait==WAIT_OBJECT_0){CloseHandle(h);return 1;}if(wait==WAIT_TIMEOUT){/* The same handle still pins the original process. */if(!TerminateProcess(h,2)){CloseHandle(h);return -1;}wait=WaitForSingleObject(h,KILL_MS);}int ok=wait==WAIT_OBJECT_0?1:0;CloseHandle(h);return ok;}

static int make_record(struct helper_state*s,char*out,size_t cap,const char*phase){
    DWORD lp=0; int bound=listener_info(s->current.host,s->current.port,&lp);
    if(!strcmp(phase,"confirmed")&&bound!=1)return -1;
    char birth[64],sbirth[64],c[128],sv[128];
    _ui64toa(s->session.birth,birth,10); _ui64toa(s->helper.birth,sbirth,10);
    if(endpoint_to_text(&s->transport.client,c,sizeof(c))<0||endpoint_to_text(&s->transport.server,sv,sizeof(sv))<0)return -1;
    int n;
    if(!strcmp(phase,"confirmed"))
        n=snprintf(out,cap,"{\"protocol\":%d,\"phase\":\"%s\",\"owner_id\":\"%s\",\"rule_id\":\"%s\",\"generation\":%llu,\"session_id\":\"%s\",\"listen_host\":\"%s\",\"listen_port\":%u,\"session\":{\"pid\":%lu,\"ppid\":%lu,\"uid\":\"%s\",\"birth\":\"%s\",\"name\":\"%s\"},\"helper\":{\"pid\":%lu,\"ppid\":%lu,\"uid\":\"%s\",\"birth\":\"%s\",\"name\":\"%s\"},\"transport\":[\"%s\",%u,\"%s\",%u],\"session_proof\":{\"source\":\"ssh_exec_ancestry_tcp_pid\",\"pid\":%lu},\"listener_absent_at_claim\":true,\"listener_proof\":{\"source\":\"ssh_forward_ack_tcp_pid\",\"pid\":%lu},\"registration\":\"ssh_exec_ancestry\"}\n",PROTOCOL,phase,s->current.owner,s->current.rule,s->current.generation,s->current.session,s->current.host,s->current.port,(unsigned long)s->session.pid,(unsigned long)s->session.ppid,s->session.sid,birth,s->session.name,(unsigned long)s->helper.pid,(unsigned long)s->helper.ppid,s->helper.sid,sbirth,s->helper.name,c,s->transport.client.port,sv,s->transport.server.port,(unsigned long)s->transport_pid,(unsigned long)lp);
    else
        n=snprintf(out,cap,"{\"protocol\":%d,\"phase\":\"%s\",\"owner_id\":\"%s\",\"rule_id\":\"%s\",\"generation\":%llu,\"session_id\":\"%s\",\"listen_host\":\"%s\",\"listen_port\":%u,\"session\":{\"pid\":%lu,\"ppid\":%lu,\"uid\":\"%s\",\"birth\":\"%s\",\"name\":\"%s\"},\"helper\":{\"pid\":%lu,\"ppid\":%lu,\"uid\":\"%s\",\"birth\":\"%s\",\"name\":\"%s\"},\"transport\":[\"%s\",%u,\"%s\",%u],\"session_proof\":{\"source\":\"ssh_exec_ancestry_tcp_pid\",\"pid\":%lu},\"listener_absent_at_claim\":true,\"registration\":\"ssh_exec_ancestry\"}\n",PROTOCOL,phase,s->current.owner,s->current.rule,s->current.generation,s->current.session,s->current.host,s->current.port,(unsigned long)s->session.pid,(unsigned long)s->session.ppid,s->session.sid,birth,s->session.name,(unsigned long)s->helper.pid,(unsigned long)s->helper.ppid,s->helper.sid,sbirth,s->helper.name,c,s->transport.client.port,sv,s->transport.server.port,(unsigned long)s->transport_pid);
    return n>=0&&(size_t)n<cap?0:-1;
}

static int match_current(const char*l,const struct claim*c){char o[37],r[37],s[37];unsigned long long g;return json_string(l,"owner_id",o,sizeof(o))==0&&json_string(l,"rule_id",r,sizeof(r))==0&&json_string(l,"session_id",s,sizeof(s))==0&&json_u64(l,"generation",&g)==0&&!strcmp(o,c->owner)&&!strcmp(r,c->rule)&&!strcmp(s,c->session)&&g==c->generation;}

static int parse_claim(const char*l,struct claim*c){unsigned long long g,p;if(json_string(l,"owner_id",c->owner,sizeof(c->owner))<0||!uuid_ok(c->owner)||json_string(l,"rule_id",c->rule,sizeof(c->rule))<0||!uuid_ok(c->rule)||json_string(l,"session_id",c->session,sizeof(c->session))<0||!uuid_ok(c->session)||json_string(l,"listen_host",c->host,sizeof(c->host))<0||parse_endpoint(c->host,1,&(struct endpoint){0})<0||json_u64(l,"generation",&g)<0||json_u64(l,"listen_port",&p)<0||p==0||p>65535)return -1;c->generation=g;c->port=(unsigned)p;return 0;}

static int claim_op(struct helper_state*s,const char*l,const char*op){
    if(s->claimed){emit_error(op,"invalid_request","this helper already claimed a rule");return -1;}
    if(parse_claim(l,&s->current)<0){emit_error(op,"invalid_request","claim fields are invalid (UUID, IP, generation, or port)");return -1;}
    if(parse_ssh_connection(&s->transport)<0||find_session(&s->transport,&s->session)<0||transport_owner(&s->transport,&s->transport_pid)!=1){emit_error(op,"ownership_mismatch","cannot identify a same-account ancestor OpenSSH session and its SSH_CONNECTION transport");return -1;}
    if(process_identity(GetCurrentProcessId(),&s->helper,PROCESS_QUERY_LIMITED_INFORMATION)!=1){emit_error(op,"ownership_mismatch","cannot identify the recovery helper process");return -1;}
    if(registry(s,s->current.owner,s->current.rule)<0){emit_error(op,"permission_denied","cannot create or lock the private remote lease registry");return -1;}
    char old[MAX_LINE];int rr=read_file(s->path,old,sizeof(old)),reclaimed=0;
    if(rr<0){unlock_registry(s);emit_error(op,"ownership_mismatch","existing remote lease is unreadable or unsafe");return -1;}
    if(rr==0){
        struct identity oldid;DWORD oldpid;char oldhost[INET6_ADDRSTRLEN];unsigned oldport;struct transport oldtr;
        if(validate_old(old,&s->current,&oldid,&oldpid,oldhost,sizeof(oldhost),&oldport,&oldtr)<0){unlock_registry(s);emit_error(op,"ownership_mismatch","existing remote lease identity or transport proof is invalid");return -1;}
        DWORD old_transport_pid=0;struct identity old_transport_identity;struct identity live;int live_ok=process_identity(oldpid,&live,PROCESS_QUERY_LIMITED_INFORMATION)==1&&same_identity(&oldid,&live);
        if(live_ok&&(transport_owner(&oldtr,&old_transport_pid)!=1||process_identity(old_transport_pid,&old_transport_identity,PROCESS_QUERY_LIMITED_INFORMATION)!=1||!allowed_name(old_transport_identity.name)||!is_descendant_of(oldpid,old_transport_pid))){unlock_registry(s);emit_error(op,"ownership_mismatch","old SSH transport no longer proves the registered session ancestry");return -1;}
        int bound=listener_info(oldhost,oldport,NULL);
        if(bound<0||bound==2||(bound==0&&port_free(oldhost,oldport)==0)){unlock_registry(s);emit_error(op,"unmanaged_conflict","old listener ownership could not be verified");return -1;}
        if(bound==1&&listener_owned_by_session(oldpid,old_transport_pid,oldhost,oldport)!=1){unlock_registry(s);emit_error(op,"unmanaged_conflict","old listener is not owned by the registered SSH session");return -1;}
        if(!live_ok&&(bound>0||port_free(oldhost,oldport)==0)){unlock_registry(s);emit_error(op,"unmanaged_conflict","old SSH identity is gone but its listener remains occupied");return -1;}
        if(live_ok){if(oldpid==s->session.pid&&oldid.birth==s->session.birth){unlock_registry(s);emit_error(op,"ownership_mismatch","old lease identifies the current SSH connection");return -1;}int killed=pin_and_terminate(&oldid);if(killed!=1){unlock_registry(s);emit_error(op,killed<0?"permission_denied":"unmanaged_conflict","old registered OpenSSH session could not be safely terminated");return -1;}reclaimed=1;}
    }
    int occupied=listener_info(s->current.host,s->current.port,NULL);if(occupied!=0||port_free(s->current.host,s->current.port)!=1){unlock_registry(s);emit_error(op,"unmanaged_conflict","remote listening port is occupied by an unregistered process");return -1;}s->helper.pid=GetCurrentProcessId();s->claimed=1;char rec[MAX_LINE];if(make_record(s,rec,sizeof(rec),"claimed")<0||write_file(s->path,rec)<0){s->claimed=0;unlock_registry(s);emit_error(op,"io_error","cannot durably write remote lease");return -1;}unlock_registry(s);printf("{\"ok\":true,\"op\":\"claim\",\"protocol\":%d,\"reclaimed\":%s,\"session_pid\":%lu,\"generation\":%llu,\"session_id\":\"%s\"}\n",PROTOCOL,reclaimed?"true":"false",(unsigned long)s->session.pid,s->current.generation,s->current.session);fflush(stdout);return 0;}

static int confirm_op(struct helper_state*s,const char*l,const char*op){if(!s->claimed||!match_current(l,&s->current)){emit_error(op,"invalid_request","claim must succeed before confirm");return -1;}int ack=0;if(json_bool(l,"forward_ack",&ack)<0||!ack){emit_error(op,"ownership_mismatch","confirm requires the SSH forwarding success acknowledgement");return -1;}struct identity cur;if(process_identity(s->session.pid,&cur,PROCESS_QUERY_LIMITED_INFORMATION)!=1||!same_identity(&s->session,&cur)){emit_error(op,"ownership_mismatch","current SSH process identity changed");return -1;}DWORD transport_pid=0,owner=0;if(transport_owner(&s->transport,&transport_pid)!=1||!is_descendant_of(s->session.pid,transport_pid)){emit_error(op,"ownership_mismatch","current SSH transport ancestry changed");return -1;}int bound=listener_info(s->current.host,s->current.port,&owner);if(bound!=1||(owner!=s->session.pid&&listener_owned_by_session(s->session.pid,transport_pid,s->current.host,s->current.port)!=1)){emit_error(op,bound==0?"listener_missing":"unmanaged_conflict","SSH has not established a listener owned by the registered OpenSSH session");return -1;}if(registry(s,s->current.owner,s->current.rule)<0){emit_error(op,"permission_denied","cannot lock the private remote lease registry");return -1;}char old[MAX_LINE];if(read_file(s->path,old,sizeof(old))!=0||!match_current(old,&s->current)){unlock_registry(s);emit_error(op,"superseded","this helper no longer owns the current generation");return -1;}char rec[MAX_LINE];if(make_record(s,rec,sizeof(rec),"confirmed")<0||write_file(s->path,rec)<0){unlock_registry(s);emit_error(op,"io_error","cannot durably confirm remote lease");return -1;}unlock_registry(s);printf("{\"ok\":true,\"op\":\"confirm\",\"protocol\":%d,\"generation\":%llu,\"session_id\":\"%s\",\"session_pid\":%lu}\n",PROTOCOL,s->current.generation,s->current.session,(unsigned long)s->session.pid);fflush(stdout);return 0;}
static int release_op(struct helper_state*s,const char*l,const char*op){if(!s->claimed||!match_current(l,&s->current)){emit_error(op,"invalid_request","claim must succeed before release");return -1;}if(registry(s,s->current.owner,s->current.rule)<0){emit_error(op,"permission_denied","cannot lock the private remote lease registry");return -1;}char old[MAX_LINE];if(read_file(s->path,old,sizeof(old))!=0||!match_current(old,&s->current)){unlock_registry(s);emit_error(op,"superseded","refusing to remove a newer session lease");return -1;}if(!DeleteFileA(s->path)){unlock_registry(s);emit_error(op,"io_error","cannot remove remote lease record");return -1;}unlock_registry(s);printf("{\"ok\":true,\"op\":\"release\",\"protocol\":%d,\"generation\":%llu,\"session_id\":\"%s\"}\n",PROTOCOL,s->current.generation,s->current.session);fflush(stdout);return 1;}

int main(void){char module[MAX_PATH];if(GetModuleFileNameA(NULL,module,sizeof(module)))DeleteFileA(module);WSADATA w;WSAStartup(MAKEWORD(2,2),&w);struct helper_state s;memset(&s,0,sizeof(s));char line[MAX_LINE+2];for(;;){if(!fgets(line,sizeof(line),stdin))return 0;size_t n=strlen(line);if(!n||line[n-1]!='\n'||n>MAX_LINE){emit_error("","invalid_request","request exceeds the protocol line limit");return 2;}char op[32]="";(void)json_string(line,"op",op,sizeof(op));int r=0;if(!strcmp(op,"claim"))r=claim_op(&s,line,op);else if(!strcmp(op,"confirm"))r=confirm_op(&s,line,op);else if(!strcmp(op,"release"))r=release_op(&s,line,op);else emit_error(op,"invalid_request","unknown operation; expected claim, confirm, or release");if(r==1){WSACleanup();return 0;}}}
