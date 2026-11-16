// Prototype of the W/S/C process model (wrapper, supervisor, filtered command).
// Minimal filter: notify on unlink/unlinkat, CONTINUE everything. Validates lifetime rules.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dirent.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static FILE *logf_;
#define LOG(...) do { if (logf_) { fprintf(logf_, __VA_ARGS__); fflush(logf_); } } while (0)

static int install_filter(void) {
  struct sock_filter f[] = {
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    BPF_JUMP(BPF_JMP | BPF_JGE | BPF_K, 0x40000000, 0, 1),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOSYS),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_unlink, 1, 0),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_unlinkat, 0, 1),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
  };
  struct sock_fprog prog = { .len = sizeof f / sizeof f[0], .filter = f };
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) return -1;
  return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_NEW_LISTENER, &prog);
}

static int send_fd(int sock, int fd) {
  char b = 'L'; struct iovec io = { &b, 1 };
  union { char buf[CMSG_SPACE(sizeof(int))]; struct cmsghdr a; } u;
  struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf };
  struct cmsghdr *c = CMSG_FIRSTHDR(&m);
  c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(c), &fd, sizeof(int));
  return sendmsg(sock, &m, 0) == 1 ? 0 : -1;
}
static int recv_fd(int sock) {
  char b; struct iovec io = { &b, 1 };
  union { char buf[CMSG_SPACE(sizeof(int))]; struct cmsghdr a; } u;
  struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf };
  if (recvmsg(sock, &m, MSG_CMSG_CLOEXEC) != 1) return -1;
  struct cmsghdr *c = CMSG_FIRSTHDR(&m); int fd; memcpy(&fd, CMSG_DATA(c), sizeof(int)); return fd;
}

// Kill every descendant of `root` (by walking /proc PPid links). Returns how many were signalled.
static int kill_descendants(pid_t root, int sig) {
  enum { MAX = 65536 }; static pid_t pids[MAX], ppids[MAX]; int n = 0;
  DIR *d = opendir("/proc"); struct dirent *e;
  while (d && (e = readdir(d)) && n < MAX) {
    pid_t p = atoi(e->d_name); if (p <= 0) continue;
    char path[64], buf[512]; snprintf(path, sizeof path, "/proc/%d/stat", p);
    int fd = open(path, O_RDONLY | O_CLOEXEC); if (fd < 0) continue;
    ssize_t k = read(fd, buf, sizeof buf - 1); close(fd); if (k <= 0) continue; buf[k] = 0;
    char *rp = strrchr(buf, ')'); if (!rp) continue; char st; int pp;
    if (sscanf(rp + 2, "%c %d", &st, &pp) != 2) continue;
    pids[n] = p; ppids[n] = pp; n++;
  }
  if (d) closedir(d);
  int killed = 0, changed = 1; static char mark[MAX]; memset(mark, 0, n);
  while (changed) { changed = 0;
    for (int i = 0; i < n; i++) if (!mark[i]) {
      int is_desc = ppids[i] == root;
      for (int j = 0; j < n && !is_desc; j++) if (mark[j] && pids[j] == ppids[i]) is_desc = 1;
      if (is_desc) { mark[i] = 1; changed = 1; }
    }
  }
  for (int i = 0; i < n; i++) if (mark[i]) { kill(pids[i], sig); killed++; }
  return killed;
}

static int supervisor(int wsock, pid_t w_pgid, const char *cmd) {
  prctl(PR_SET_CHILD_SUBREAPER, 1);
  setpgid(0, 0);
  sigset_t ss; sigemptyset(&ss); sigaddset(&ss, SIGCHLD); sigprocmask(SIG_BLOCK, &ss, NULL);
  int sigfd = signalfd(-1, &ss, SFD_CLOEXEC | SFD_NONBLOCK);
  int sp[2]; socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sp);
  pid_t c = fork();
  if (c == 0) {
    sigprocmask(SIG_UNBLOCK, &ss, NULL);
    close(sp[0]); close(wsock);
    setpgid(0, w_pgid);
    int lfd = install_filter();
    if (lfd < 0) { dprintf(2, "wsc: install filter: %s\n", strerror(errno)); _exit(125); }
    if (send_fd(sp[1], lfd)) _exit(125);
    close(lfd); close(sp[1]);
    execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
    _exit(127);
  }
  close(sp[1]);
  int lfd = recv_fd(sp[0]); close(sp[0]);
  if (lfd < 0) LOG("S: no listener\n");
  // Never hold the agent's pipes or terminal.
  int dn = open("/dev/null", O_RDWR | O_CLOEXEC); dup2(dn, 0); dup2(dn, 1); dup2(dn, 2); close(dn);
  if (chdir("/")) {}
  int c_done = 0, w_gone = 0, sent = 0, hup = 0, c_status = 0; long notifs = 0;
  struct seccomp_notif req; struct seccomp_notif_resp resp;
  for (;;) {
    struct pollfd p[3] = { { lfd, POLLIN, 0 }, { sigfd, POLLIN, 0 }, { w_gone ? -1 : wsock, POLLIN, 0 } };
    if (poll(p, 3, 1000) < 0 && errno != EINTR) break;
    if (p[1].revents & POLLIN) {
      struct signalfd_siginfo si; while (read(sigfd, &si, sizeof si) > 0) {}
      int st; pid_t r;
      while ((r = waitpid(-1, &st, WNOHANG)) > 0) {
        if (r == c) { c_done = 1; c_status = st; LOG("S: C exited st=0x%x\n", st); }
        else LOG("S: reaped orphan %d\n", r);
      }
    }
    if (p[2].revents & (POLLIN | POLLHUP | POLLERR)) {
      char b; ssize_t k = read(wsock, &b, 1);
      if (k <= 0) { w_gone = 1;
        if (!c_done) { int n = kill_descendants(getpid(), SIGKILL); LOG("S: W died early, killed %d\n", n); }
        else LOG("S: W returned normally\n"); }
    }
    if (p[0].revents & POLLIN) {
      memset(&req, 0, sizeof req);
      if (ioctl(lfd, SECCOMP_IOCTL_NOTIF_RECV, &req) == 0) {
        notifs++;
        memset(&resp, 0, sizeof resp); resp.id = req.id; resp.flags = SECCOMP_USER_NOTIF_FLAG_CONTINUE;
        ioctl(lfd, SECCOMP_IOCTL_NOTIF_SEND, &resp);
      }
    }
    if ((p[0].revents & POLLHUP) || lfd < 0) hup = 1;
    if (hup) {  // every filtered task has exited (maybe not reaped yet): reap all, blocking
      int st; pid_t r;
      while ((r = waitpid(-1, &st, 0)) > 0) if (r == c) { c_done = 1; c_status = st; }
    }
    if (c_done && !sent) {
      if (!hup && lfd >= 0) { struct pollfd q = { lfd, 0, 0 }; poll(&q, 1, 0); if (q.revents & POLLHUP) hup = 1; }
      int msg[2] = { c_status, !hup };  // status, detached (background processes remain)
      if (!w_gone && write(wsock, msg, sizeof msg) != sizeof msg) LOG("S: write to W failed\n");
      sent = 1; LOG("S: status sent, detached=%d\n", !hup);
    }
    if (hup) { LOG("S: POLLHUP, exiting (notifs=%ld)\n", notifs); break; }
  }
  return 0;
}

int main(int argc, char **argv) {
  if (argc != 2) { fprintf(stderr, "usage: wsc '<command>'\n"); return 125; }
  const char *lp = getenv("WSC_LOG"); if (lp) logf_ = fopen(lp, "a");
  int ws[2]; if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, ws)) return 125;
  pid_t w_pgid = getpgrp();
  pid_t s = fork();
  if (s < 0) return 125;
  if (s == 0) { close(ws[0]); _exit(supervisor(ws[1], w_pgid, argv[1])); }
  close(ws[1]);
  signal(SIGINT, SIG_IGN); signal(SIGQUIT, SIG_IGN);
  int msg[2]; ssize_t k = read(ws[0], msg, sizeof msg);
  if (k != sizeof msg) { fprintf(stderr, "wsc: supervisor lost\n"); return 125; }
  int st = msg[0];
  if (!msg[1]) waitpid(s, NULL, 0);  /* no background processes: reap the supervisor so it never lingers */
  if (WIFEXITED(st)) return WEXITSTATUS(st);
  if (WIFSIGNALED(st)) { int sig = WTERMSIG(st);
    struct rlimit rl = { 0, 0 }; setrlimit(RLIMIT_CORE, &rl);
    signal(sig, SIG_DFL); sigset_t u; sigemptyset(&u); sigaddset(&u, sig); sigprocmask(SIG_UNBLOCK, &u, NULL);
    raise(sig); return 128 + sig; }
  return 125;
}
