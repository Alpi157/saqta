// prewrite.c: feasibility prototype for syscall-level pre-image capture.
// Runs a command under a seccomp user-notification filter. Before any syscall
// that can destroy file data (unlink, rmdir, rename over a target, open for
// write, truncate), the supervisor preserves the old version, then lets the
// syscall continue on the real filesystem. Deletions and rename-overs are
// preserved with a hard link (no data copied). In-place writes get a copy
// (reflink when the filesystem supports it).
//
// Usage: prewrite [-n] [-s] -q QUARANTINE_DIR -j JOURNAL_TSV -- cmd args...
//   -n  notify only: intercept but capture nothing (measures interception cost)
//   -s  skip capture under regenerable paths (/node_modules/, /target/, /.cache/)
// x86_64 only. Not hardened: path reads are TOCTOU-racy, which is acceptable
// for accidental damage by a well-meaning agent but not for adversaries.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/fs.h>
#include <linux/seccomp.h>
#include <poll.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SECCOMP_USER_NOTIF_FLAG_CONTINUE
#define SECCOMP_USER_NOTIF_FLAG_CONTINUE (1UL << 0)
#endif
#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE (1 << 0)
#endif
#ifndef RENAME_EXCHANGE
#define RENAME_EXCHANGE (1 << 1)
#endif

#define WFLAGS (O_WRONLY | O_RDWR | O_TRUNC | O_CREAT)

static int g_notify_only = 0, g_skip_regen = 0;
static int g_qfd = -1;
static FILE *g_j;
static unsigned long g_seq, g_notifs, g_links, g_copies, g_copy_bytes, g_created,
    g_dirs, g_renames, g_skipped, g_errors;

static int sys_seccomp(unsigned op, unsigned flags, void *args) {
  return (int)syscall(SYS_seccomp, op, flags, args);
}

/* ---------- BPF filter ---------- */
#define LD_NR BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr))
#define LD_ARG(i) BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[i]))
#define RET(v) BPF_STMT(BPF_RET | BPF_K, (v))
#define IS(nr, skip) BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (nr), 0, (skip))

static struct sock_filter filter[] = {
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
    RET(SECCOMP_RET_ALLOW),
    LD_NR,
    /* openat(dirfd, path, flags): notify only when flags can modify data */
    IS(__NR_openat, 7), LD_ARG(2),
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, O_EXCL, 4, 0),   /* new file only: cannot destroy */
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, O_TRUNC, 2, 0),
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, O_APPEND, 2, 0), /* append without truncate */
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, WFLAGS, 0, 1),
    RET(SECCOMP_RET_USER_NOTIF), RET(SECCOMP_RET_ALLOW),
    /* open(path, flags) */
    IS(__NR_open, 7), LD_ARG(1),
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, O_EXCL, 4, 0),
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, O_TRUNC, 2, 0),
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, O_APPEND, 2, 0),
    BPF_JUMP(BPF_JMP | BPF_JSET | BPF_K, WFLAGS, 0, 1),
    RET(SECCOMP_RET_USER_NOTIF), RET(SECCOMP_RET_ALLOW),
    IS(__NR_creat, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_truncate, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_unlink, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_unlinkat, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_rmdir, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_rename, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_renameat, 1), RET(SECCOMP_RET_USER_NOTIF),
    IS(__NR_renameat2, 1), RET(SECCOMP_RET_USER_NOTIF),
    /* force fallbacks for interfaces the prototype does not inspect */
    IS(__NR_openat2, 1), RET(SECCOMP_RET_ERRNO | ENOSYS),
    IS(__NR_io_uring_setup, 1), RET(SECCOMP_RET_ERRNO | ENOSYS),
    RET(SECCOMP_RET_ALLOW),
};

/* ---------- helpers ---------- */
static ssize_t read_str(pid_t pid, uint64_t addr, char *buf, size_t max) {
  size_t off = 0;
  while (off < max - 1) {
    size_t page_left = 4096 - ((addr + off) & 4095);
    size_t chunk = max - 1 - off < page_left ? max - 1 - off : page_left;
    struct iovec l = {buf + off, chunk}, r = {(void *)(uintptr_t)(addr + off), chunk};
    ssize_t n = process_vm_readv(pid, &l, 1, &r, 1, 0);
    if (n <= 0) return -1;
    char *z = memchr(buf + off, 0, (size_t)n);
    if (z) return z - buf;
    off += (size_t)n;
  }
  buf[max - 1] = 0;
  return (ssize_t)(max - 1);
}

/* Open the directory a relative path is resolved against, as the target sees it. */
static int open_base(pid_t pid, int dirfd, const char *path, char *disp, size_t dn) {
  char p[64];
  if (path[0] == '/') { snprintf(disp, dn, "%s", path); return AT_FDCWD; }
  if (dirfd == AT_FDCWD) snprintf(p, sizeof p, "/proc/%d/cwd", pid);
  else snprintf(p, sizeof p, "/proc/%d/fd/%d", pid, dirfd);
  char dir[PATH_MAX];
  ssize_t n = readlink(p, dir, sizeof dir - 1);
  if (n < 0) n = 0;
  dir[n] = 0;
  snprintf(disp, dn, "%s/%s", dir, path);
  return open(p, O_PATH | O_DIRECTORY | O_CLOEXEC);
}

static int is_regen(const char *disp) {
  return strstr(disp, "/node_modules/") || strstr(disp, "/target/") || strstr(disp, "/.cache/");
}

/* de-duplicate captures of the same file version within one run */
#define SEEN_CAP (1u << 20)
struct key { dev_t d; ino_t i; long ms; long mns; off_t sz; };
static struct key *seen;
static int seen_add(const struct stat *st) {
  uint64_t h = (st->st_dev * 1000003u) ^ (st->st_ino * 2654435761u) ^ (uint64_t)st->st_mtim.tv_nsec;
  for (uint32_t k = 0; k < SEEN_CAP; k++) {
    struct key *e = &seen[(h + k) & (SEEN_CAP - 1)];
    if (e->i == 0) { *e = (struct key){st->st_dev, st->st_ino, st->st_mtim.tv_sec, st->st_mtim.tv_nsec, st->st_size}; return 1; }
    if (e->d == st->st_dev && e->i == st->st_ino && e->ms == st->st_mtim.tv_sec &&
        e->mns == st->st_mtim.tv_nsec && e->sz == st->st_size) return 0;
  }
  return 1;
}

static void jlog(const char *kind, const char *disp, const char *q, pid_t pid, const char *extra) {
  fprintf(g_j, "%lu\t%d\t%s\t%s\t%s\t%s\n", g_seq, pid, kind, disp, q ? q : "-", extra ? extra : "-");
}

static int copy_fd(int src, int dst, off_t size) {
  if (ioctl(dst, FICLONE, src) == 0) return 0;  /* reflink: XFS, btrfs, bcachefs */
  off_t left = size;
  while (left > 0) {
    ssize_t n = copy_file_range(src, NULL, dst, NULL, (size_t)left, 0);
    if (n <= 0) return -1;
    left -= n;
  }
  return 0;
}

/* Preserve a name that is about to disappear (unlink, rename-over). */
static void keep_by_link(int base, const char *rel, const char *disp, pid_t pid, const char *why) {
  struct stat st;
  if (fstatat(base, rel, &st, AT_SYMLINK_NOFOLLOW) != 0) return;
  if (S_ISDIR(st.st_mode)) return;
  if (g_skip_regen && is_regen(disp)) { g_skipped++; return; }
  char q[32];
  snprintf(q, sizeof q, "%010lu", ++g_seq);
  if (linkat(base, rel, g_qfd, q, 0) == 0) { g_links++; jlog("link", disp, q, pid, why); return; }
  /* different filesystem or link refused: fall back to a copy */
  int s = openat(base, rel, O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
  int d = openat(g_qfd, q, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, st.st_mode & 07777);
  if (s >= 0 && d >= 0 && copy_fd(s, d, st.st_size) == 0) {
    g_copies++; g_copy_bytes += (unsigned long)st.st_size; jlog("copy", disp, q, pid, why);
  } else g_errors++;
  if (s >= 0) close(s);
  if (d >= 0) close(d);
}

/* Preserve the content of a file that is about to be written in place. */
static void keep_by_copy(int base, const char *rel, const char *disp, pid_t pid, int nofollow, const char *why) {
  struct stat st;
  if (fstatat(base, rel, &st, nofollow ? AT_SYMLINK_NOFOLLOW : 0) != 0) {
    if (errno == ENOENT) { g_created++; jlog("created", disp, NULL, pid, why); }
    else g_errors++;
    return;
  }
  if (!S_ISREG(st.st_mode) || st.st_size == 0) return;
  if (g_skip_regen && is_regen(disp)) { g_skipped++; return; }
  if (!seen_add(&st)) return;
  char q[32];
  snprintf(q, sizeof q, "%010lu", ++g_seq);
  int s = openat(base, rel, O_RDONLY | O_CLOEXEC | (nofollow ? O_NOFOLLOW : 0));
  int d = openat(g_qfd, q, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, st.st_mode & 07777);
  if (s >= 0 && d >= 0 && copy_fd(s, d, st.st_size) == 0) {
    g_copies++; g_copy_bytes += (unsigned long)st.st_size; jlog("copy", disp, q, pid, why);
  } else g_errors++;
  if (s >= 0) close(s);
  if (d >= 0) close(d);
}

static void handle(struct seccomp_notif *n) {
  pid_t pid = (pid_t)n->pid;
  int nr = n->data.nr;
  uint64_t *a = (uint64_t *)n->data.args;
  char p1[PATH_MAX], p2[PATH_MAX], d1[PATH_MAX * 2], d2[PATH_MAX * 2];
  int dirfd1 = AT_FDCWD, dirfd2 = AT_FDCWD;
  uint64_t path1 = 0, path2 = 0;
  long flags = 0;
  switch (nr) {
    case __NR_openat: dirfd1 = (int)a[0]; path1 = a[1]; flags = (long)a[2]; break;
    case __NR_open: path1 = a[0]; flags = (long)a[1]; break;
    case __NR_creat: path1 = a[0]; flags = O_CREAT | O_WRONLY | O_TRUNC; break;
    case __NR_truncate: path1 = a[0]; break;
    case __NR_unlink: case __NR_rmdir: path1 = a[0]; break;
    case __NR_unlinkat: dirfd1 = (int)a[0]; path1 = a[1]; flags = (long)a[2]; break;
    case __NR_rename: path1 = a[0]; path2 = a[1]; break;
    case __NR_renameat: dirfd1 = (int)a[0]; path1 = a[1]; dirfd2 = (int)a[2]; path2 = a[3]; break;
    case __NR_renameat2: dirfd1 = (int)a[0]; path1 = a[1]; dirfd2 = (int)a[2]; path2 = a[3]; flags = (long)a[4]; break;
    default: return;
  }
  if (g_notify_only) return;
  if (read_str(pid, path1, p1, sizeof p1) < 0) { g_errors++; return; }
  if (path2 && read_str(pid, path2, p2, sizeof p2) < 0) { g_errors++; return; }
  int b1 = open_base(pid, dirfd1, p1, d1, sizeof d1);
  int b2 = path2 ? open_base(pid, dirfd2, p2, d2, sizeof d2) : -2;
  if (b1 == -1 || b2 == -1) {
    g_errors++;
    if (b1 >= 0) close(b1);
    if (b2 >= 0) close(b2);
    return;
  }

  switch (nr) {
    case __NR_open: case __NR_openat: case __NR_creat:
      if (flags & (O_WRONLY | O_RDWR | O_TRUNC))
        keep_by_copy(b1, p1, d1, pid, (flags & O_NOFOLLOW) != 0, "open-write");
      else if (flags & O_CREAT) {
        struct stat st;
        if (fstatat(b1, p1, &st, 0) != 0) { g_created++; jlog("created", d1, NULL, pid, "open-creat"); }
      }
      break;
    case __NR_truncate: keep_by_copy(b1, p1, d1, pid, 0, "truncate"); break;
    case __NR_unlink: keep_by_link(b1, p1, d1, pid, "unlink"); break;
    case __NR_unlinkat:
      if (flags & AT_REMOVEDIR) { g_dirs++; jlog("rmdir", d1, NULL, pid, "unlinkat-dir"); }
      else keep_by_link(b1, p1, d1, pid, "unlinkat");
      break;
    case __NR_rmdir: g_dirs++; jlog("rmdir", d1, NULL, pid, "rmdir"); break;
    case __NR_rename: case __NR_renameat: case __NR_renameat2:
      if (!(flags & RENAME_NOREPLACE)) keep_by_link(b2, p2, d2, pid, "rename-over");
      g_renames++;
      jlog("rename", d1, NULL, pid, d2);
      break;
  }
  if (b1 >= 0 && b1 != AT_FDCWD) close(b1);
  if (b2 >= 0 && b2 != AT_FDCWD) close(b2);
}

static int send_fd(int sock, int fd) {
  char buf[CMSG_SPACE(sizeof(int))] = {0}, c = 'x';
  struct iovec io = {&c, 1};
  struct msghdr m = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = buf, .msg_controllen = sizeof buf};
  struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
  cm->cmsg_level = SOL_SOCKET; cm->cmsg_type = SCM_RIGHTS; cm->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(cm), &fd, sizeof(int));
  return sendmsg(sock, &m, 0) < 0 ? -1 : 0;
}
static int recv_fd(int sock) {
  char buf[CMSG_SPACE(sizeof(int))] = {0}, c;
  struct iovec io = {&c, 1};
  struct msghdr m = {.msg_iov = &io, .msg_iovlen = 1, .msg_control = buf, .msg_controllen = sizeof buf};
  if (recvmsg(sock, &m, 0) <= 0) return -1;
  struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
  int fd; memcpy(&fd, CMSG_DATA(cm), sizeof(int));
  return fd;
}

int main(int argc, char **argv) {
  const char *qdir = NULL, *jpath = NULL;
  int i = 1;
  for (; i < argc; i++) {
    if (!strcmp(argv[i], "-n")) g_notify_only = 1;
    else if (!strcmp(argv[i], "-s")) g_skip_regen = 1;
    else if (!strcmp(argv[i], "-q") && i + 1 < argc) qdir = argv[++i];
    else if (!strcmp(argv[i], "-j") && i + 1 < argc) jpath = argv[++i];
    else if (!strcmp(argv[i], "--")) { i++; break; }
    else break;
  }
  if (i >= argc || !qdir || !jpath) {
    fprintf(stderr, "usage: prewrite [-n] [-s] -q QDIR -j JOURNAL -- cmd...\n");
    return 2;
  }
  mkdir(qdir, 0700);
  g_qfd = open(qdir, O_PATH | O_DIRECTORY | O_CLOEXEC);
  g_j = fopen(jpath, "a");
  seen = calloc(SEEN_CAP, sizeof *seen);
  if (g_qfd < 0 || !g_j || !seen) { perror("setup"); return 2; }

  struct sock_fprog prog = {.len = sizeof filter / sizeof filter[0], .filter = filter};
  int sv[2];
  if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) < 0) { perror("socketpair"); return 2; }
  struct timespec t0, t1;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  pid_t child = fork();
  if (child == 0) {
    close(sv[0]);
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) _exit(126);
    int lfd = sys_seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_NEW_LISTENER, &prog);
    if (lfd < 0) { perror("seccomp"); _exit(126); }
    send_fd(sv[1], lfd);
    close(lfd); close(sv[1]);
    execvp(argv[i], &argv[i]);
    perror("execvp");
    _exit(127);
  }
  close(sv[1]);
  int lfd = recv_fd(sv[0]);
  close(sv[0]);
  if (lfd < 0) { fprintf(stderr, "no listener fd\n"); return 2; }

  if (getenv("PREWRITE_SYNC")) {
    if (ioctl(lfd, SECCOMP_IOCTL_NOTIF_SET_FLAGS, SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP, 0) != 0)
      perror("SECCOMP_IOCTL_NOTIF_SET_FLAGS");
  }
  struct seccomp_notif_sizes sz;
  sys_seccomp(SECCOMP_GET_NOTIF_SIZES, 0, &sz);
  struct seccomp_notif *req = calloc(1, sz.seccomp_notif);
  struct seccomp_notif_resp *resp = calloc(1, sz.seccomp_notif_resp);

  for (;;) {
    struct pollfd pfd = {lfd, POLLIN, 0};
    if (poll(&pfd, 1, -1) < 0) { if (errno == EINTR) continue; break; }
    if (pfd.revents & POLLIN) {
      memset(req, 0, sz.seccomp_notif);
      if (ioctl(lfd, SECCOMP_IOCTL_NOTIF_RECV, req) == 0) {
        g_notifs++;
        handle(req);
        memset(resp, 0, sz.seccomp_notif_resp);
        resp->id = req->id;
        resp->flags = SECCOMP_USER_NOTIF_FLAG_CONTINUE;
        ioctl(lfd, SECCOMP_IOCTL_NOTIF_SEND, resp); /* ENOENT if the target died meanwhile */
      }
    }
    if (pfd.revents & (POLLHUP | POLLERR)) break; /* no task uses the filter any more */
  }
  int status = 0;
  waitpid(child, &status, 0);
  clock_gettime(CLOCK_MONOTONIC, &t1);
  fflush(g_j);
  double ms = (double)(t1.tv_sec - t0.tv_sec) * 1e3 + (double)(t1.tv_nsec - t0.tv_nsec) / 1e6;
  fprintf(stderr,
          "[prewrite] %.1f ms notifs=%lu links=%lu copies=%lu copied_bytes=%lu created=%lu "
          "rmdirs=%lu renames=%lu skipped_regen=%lu errors=%lu\n",
          ms, g_notifs, g_links, g_copies, g_copy_bytes, g_created, g_dirs, g_renames, g_skipped, g_errors);
  return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
