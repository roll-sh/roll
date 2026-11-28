/* powercut.so: record what a process does to the files under one directory, so that a power
 * cut can be rebuilt afterwards (tools/powercut/rebuild.py; driven by tools/crash-binary.py
 * --power). LD_PRELOAD it with
 *
 *     POWERCUT=<root>:<grave>:<log>
 *
 * and POWERCUT_EXE=<the program's path> (realpath: only that executable records).
 * <root> the directory to watch (absolute, no symlinks), <grave> a directory on the same
 * filesystem outside it, <log> the record (appended). Nothing is buffered in the process: each
 * record is one write(2) to <log>, so a SIGKILL keeps all of them (the host's page cache), and
 * the real files keep everything written: the cut is made afterwards, from the record.
 *
 * Every change under <root> is made under one mutex, with its record written before it (data,
 * renames, removals) or right after it (a new file, directory or link: its inode is known then),
 * so the record is the order the kernel saw. A kill between a record and its change leaves at
 * most the last record not done, or the last change not recorded; the rebuild tells which from
 * the directory:
 *   W  a write (write, pwrite, writev, pwritev, copy_file_range, sendfile): the file's inode,
 *      offset, length, its size before, and the bytes it overwrote (an undo record: the new
 *      bytes are in the file itself);
 *   T  a size change (ftruncate, truncate, fallocate, open with O_TRUNC): size before and after,
 *      the bytes cut off;
 *   F / D  fsync or fdatasync of a file / a directory, with the sequence number at its start:
 *      the records before it are what it made durable (one that fails is not recorded);
 *   S  sync(2) / syncfs(2): everything before it;
 *   C / M  a file created (open O_CREAT) / a directory made: inode, parent's inode, name;
 *   R  a rename: inode, both parents and names, the inode it replaced (kept alive in <grave>
 *      by a hard link, so the cut can bring it back);
 *   U  unlink / rmdir: not done, the entry is renamed into <grave> instead (its data stays for
 *      a cut that brings the entry back);
 *   L  a hard link;
 *   N  a rename or removal recorded that then failed: the record named by its sequence number
 *      did not happen;
 *   X  something this layer can't model (a symlink, a rename across <root>'s edge, an fd past
 *      its table): the rebuild refuses to cut such a run.
 * Reads, and files outside <root>, are passed straight through.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

#define MAXFD (1 << 20)

/* Per fd: the inode + 1 of a file or directory under the root (0: not watched), and whether it
 * was opened O_APPEND. */
static uint64_t fd_ino[MAXFD];
static uint8_t fd_append[MAXFD];

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static char root[PATH_MAX], grave[PATH_MAX];
static size_t root_len;
static int logfd = -1;
static uint64_t seq = 1;
static int on;

struct rec {
	uint32_t size;  /* the whole record */
	uint8_t kind;
	uint8_t pad[3];
	uint32_t n1, n2; /* name lengths */
	uint64_t seq, a, b, c, d;
};

#define REAL(name, ret, ...) static ret (*real_##name)(__VA_ARGS__)
REAL(open, int, const char *, int, ...);
REAL(open64, int, const char *, int, ...);
REAL(openat, int, int, const char *, int, ...);
REAL(openat64, int, int, const char *, int, ...);
REAL(creat, int, const char *, mode_t);
REAL(creat64, int, const char *, mode_t);
REAL(close, int, int);
REAL(write, ssize_t, int, const void *, size_t);
REAL(pwrite, ssize_t, int, const void *, size_t, off_t);
REAL(pwrite64, ssize_t, int, const void *, size_t, off64_t);
REAL(writev, ssize_t, int, const struct iovec *, int);
REAL(pwritev, ssize_t, int, const struct iovec *, int, off_t);
REAL(pwritev64, ssize_t, int, const struct iovec *, int, off64_t);
REAL(pwritev2, ssize_t, int, const struct iovec *, int, off_t, int);
REAL(pwritev64v2, ssize_t, int, const struct iovec *, int, off64_t, int);
REAL(fsync, int, int);
REAL(fdatasync, int, int);
REAL(syncfs, int, int);
REAL(sync, void, void);
REAL(ftruncate, int, int, off_t);
REAL(ftruncate64, int, int, off64_t);
REAL(truncate, int, const char *, off_t);
REAL(truncate64, int, const char *, off64_t);
REAL(fallocate, int, int, int, off_t, off_t);
REAL(fallocate64, int, int, int, off64_t, off64_t);
REAL(posix_fallocate, int, int, off_t, off_t);
REAL(posix_fallocate64, int, int, off64_t, off64_t);
REAL(rename, int, const char *, const char *);
REAL(renameat, int, int, const char *, int, const char *);
REAL(renameat2, int, int, const char *, int, const char *, unsigned);
REAL(unlink, int, const char *);
REAL(unlinkat, int, int, const char *, int);
REAL(rmdir, int, const char *);
REAL(mkdir, int, const char *, mode_t);
REAL(mkdirat, int, int, const char *, mode_t);
REAL(link, int, const char *, const char *);
REAL(linkat, int, int, const char *, int, const char *, int);
REAL(symlink, int, const char *, const char *);
REAL(symlinkat, int, const char *, int, const char *);
REAL(dup, int, int);
REAL(dup2, int, int, int);
REAL(dup3, int, int, int, int);
REAL(fcntl, int, int, int, ...);
REAL(fcntl64, int, int, int, ...);
REAL(copy_file_range, ssize_t, int, off64_t *, int, off64_t *, size_t, unsigned);
REAL(sendfile, ssize_t, int, int, off_t *, size_t);
REAL(sendfile64, ssize_t, int, int, off64_t *, size_t);

static void die(const char *m) {
	char b[512];
	int n = snprintf(b, sizeof b, "powercut: %s: %s\n", m, strerror(errno));
	if (n > 0) (void)!write(2, b, (size_t)n);
	abort();
}

#define LOAD(name) real_##name = dlsym(RTLD_NEXT, #name)

__attribute__((constructor)) static void init(void) {
	LOAD(open); LOAD(open64); LOAD(openat); LOAD(openat64); LOAD(creat); LOAD(creat64);
	LOAD(close); LOAD(write); LOAD(pwrite); LOAD(pwrite64); LOAD(writev); LOAD(pwritev);
	LOAD(pwritev64); LOAD(pwritev2); LOAD(pwritev64v2); LOAD(fsync); LOAD(fdatasync);
	LOAD(syncfs); LOAD(sync); LOAD(ftruncate); LOAD(ftruncate64); LOAD(truncate);
	LOAD(truncate64); LOAD(fallocate); LOAD(fallocate64); LOAD(posix_fallocate);
	LOAD(posix_fallocate64); LOAD(rename); LOAD(renameat); LOAD(renameat2); LOAD(unlink);
	LOAD(unlinkat); LOAD(rmdir); LOAD(mkdir); LOAD(mkdirat); LOAD(link); LOAD(linkat);
	LOAD(symlink); LOAD(symlinkat); LOAD(dup); LOAD(dup2); LOAD(dup3); LOAD(fcntl);
	LOAD(fcntl64); LOAD(copy_file_range); LOAD(sendfile); LOAD(sendfile64);
	const char *v = getenv("POWERCUT");
	if (!v) return;
	char buf[3 * PATH_MAX];
	if (strlen(v) >= sizeof buf) return;
	strcpy(buf, v);
	char *g = strchr(buf, ':');
	if (!g) return;
	*g++ = 0;
	char *l = strchr(g, ':');
	if (!l) return;
	*l++ = 0;
	strcpy(root, buf);
	strcpy(grave, g);
	root_len = strlen(root);
	while (root_len > 1 && root[root_len - 1] == '/') root[--root_len] = 0;
	/* Only the program named by POWERCUT_EXE records: not a wrapper that execs it (taskset),
	 * not a program it runs (which inherits the variables but not this process's tables). */
	const char *want = getenv("POWERCUT_EXE");
	char exe[PATH_MAX];
	ssize_t en = readlink("/proc/self/exe", exe, sizeof exe - 1);
	if (!want || en < 0) return;
	exe[en] = 0;
	if (strcmp(exe, want)) return;
	logfd = real_open(l, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
	if (logfd < 0) die("open the log");
	/* Keep the log's fd out of the way of the program's own numbering. */
	int hi = real_fcntl(logfd, F_DUPFD_CLOEXEC, 1000);
	if (hi >= 0) {
		real_close(logfd);
		logfd = hi;
	}
	on = 1;
}

/* Append one record (the caller holds mu). */
static uint64_t emit(int kind, uint64_t a, uint64_t b, uint64_t c, uint64_t d, const char *n1, const char *n2,
		     const void *payload, size_t plen) {
	struct rec r = {0};
	size_t l1 = n1 ? strlen(n1) : 0, l2 = n2 ? strlen(n2) : 0;
	r.size = (uint32_t)(sizeof r + l1 + l2 + plen);
	r.kind = (uint8_t)kind;
	r.n1 = (uint32_t)l1;
	r.n2 = (uint32_t)l2;
	r.seq = seq++;
	r.a = a; r.b = b; r.c = c; r.d = d;
	struct iovec v[4] = {{&r, sizeof r}, {(void *)n1, l1}, {(void *)n2, l2}, {(void *)payload, plen}};
	size_t want = r.size;
	if (plen > 0xffffffffu - sizeof r - l1 - l2) { errno = EFBIG; die("a record past 4 GB"); }
	ssize_t n = real_writev(logfd, v, 4);
	if (n < 0 || (size_t)n != want) die("write the log");
	return r.seq;
}

/* Lexically normalised absolute path of (dirfd, path) into out; 0 when it can't be made. */
static int resolve(int dirfd, const char *path, char *out) {
	char base[PATH_MAX];
	if (!path) return 0;
	if (path[0] == '/') {
		base[0] = 0;
	} else if (dirfd == AT_FDCWD) {
		if (!getcwd(base, sizeof base)) return 0;
	} else {
		char p[64];
		snprintf(p, sizeof p, "/proc/self/fd/%d", dirfd);
		ssize_t n = readlink(p, base, sizeof base - 1);
		if (n < 0) return 0;
		base[n] = 0;
	}
	char tmp[2 * PATH_MAX];
	if (snprintf(tmp, sizeof tmp, "%s/%s", base, path) >= (int)sizeof tmp) return 0;
	/* Collapse //, /./ and /../. */
	char *comp[PATH_MAX / 2];
	int k = 0;
	char *save = NULL;
	for (char *s = strtok_r(tmp, "/", &save); s; s = strtok_r(NULL, "/", &save)) {
		if (!strcmp(s, ".")) continue;
		if (!strcmp(s, "..")) { if (k) k--; continue; }
		comp[k++] = s;
	}
	size_t o = 0;
	out[0] = 0;
	for (int i = 0; i < k; i++) {
		size_t l = strlen(comp[i]);
		if (o + l + 2 >= PATH_MAX) return 0;
		out[o++] = '/';
		memcpy(out + o, comp[i], l);
		o += l;
	}
	if (!o) { out[0] = '/'; o = 1; }
	out[o] = 0;
	return 1;
}

static int under(const char *abs) {
	return on && !strncmp(abs, root, root_len) && (abs[root_len] == '/' || abs[root_len] == 0);
}

/* Split abs into its parent's inode and its name (pointing into abs); 0 when the parent is gone. */
static int parent_of(char *abs, uint64_t *pino, const char **name) {
	char *s = strrchr(abs, '/');
	if (!s || s == abs) return 0;
	*s = 0;
	struct stat st;
	int ok = lstat(abs, &st) == 0;
	*s = '/';
	if (!ok) return 0;
	*pino = st.st_ino;
	*name = s + 1;
	return 1;
}

static void track(int fd, uint64_t ino, int append) {
	if (fd < 0) return;
	if (fd >= MAXFD) { emit('X', ino, (uint64_t)fd, 0, 0, "an fd past the table", NULL, NULL, 0); return; }
	fd_ino[fd] = ino + 1;
	fd_append[fd] = (uint8_t)append;
}

static uint64_t watched(int fd) {
	return on && fd >= 0 && fd < MAXFD ? fd_ino[fd] : 0;
}

/* Read [off, off+len) of path's file (or fd's) into a new buffer: what a change overwrites. */
static void *old_bytes(int fd, uint64_t off, uint64_t len) {
	if (!len) return NULL;
	char *b = malloc(len);
	if (!b) die("malloc");
	uint64_t got = 0;
	while (got < len) {
		ssize_t n = pread(fd, b + got, len - got, (off_t)(off + got));
		if (n < 0 && errno == EINTR) continue;
		if (n <= 0) die("read the bytes a write overwrites");
		got += (uint64_t)n;
	}
	return b;
}

/* A reading fd for fd's file: fd itself when it can be read, else one opened through /proc. */
static int readable(int fd, int *opened) {
	int fl = real_fcntl(fd, F_GETFL);
	*opened = 0;
	if (fl >= 0 && (fl & O_ACCMODE) != O_WRONLY) return fd;
	char p[64];
	snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
	int r = real_open(p, O_RDONLY | O_CLOEXEC);
	if (r < 0) die("reopen a write-only file to read what a write overwrites");
	*opened = 1;
	return r;
}

/* fd still the file it was opened as: an fd closed behind this layer's back (closedir's
 * internal close) and reused for a socket is dropped from the table. The caller holds mu. */
static int still(int fd, struct stat *st) {
	if (fstat(fd, st) == 0 && st->st_ino + 1 == fd_ino[fd] && (S_ISREG(st->st_mode) || S_ISDIR(st->st_mode))) return 1;
	fd_ino[fd] = 0;
	return 0;
}

/* Record a write of len bytes at off (off < 0: the fd's position, or its end with O_APPEND),
 * before it is made. The caller holds mu. */
static void note_write(int fd, int64_t off, uint64_t len) {
	struct stat st;
	if (!still(fd, &st)) return;
	uint64_t size = (uint64_t)st.st_size;
	uint64_t at;
	if (off >= 0) at = (uint64_t)off;
	else if (fd_append[fd]) at = size;
	else {
		off_t p = lseek(fd, 0, SEEK_CUR);
		if (p < 0) die("lseek a watched fd");
		at = (uint64_t)p;
	}
	uint64_t over = at < size ? (size - at < len ? size - at : len) : 0;
	int opened;
	int r = over ? readable(fd, &opened) : fd;
	void *old = over ? old_bytes(r, at, over) : NULL;
	if (over && opened) real_close(r);
	emit('W', st.st_ino, at, len, size, NULL, NULL, old, over);
	free(old);
}

/* Record a size change of fd's file to `to`, before it is made. The caller holds mu. */
static void note_size(int fd, uint64_t to) {
	struct stat st;
	if (fstat(fd, &st)) die("fstat a watched fd");
	if (fd >= 0 && fd < MAXFD && fd_ino[fd] && !still(fd, &st)) return;
	uint64_t size = (uint64_t)st.st_size;
	if (to == size) return;
	void *old = NULL;
	uint64_t cut = to < size ? size - to : 0;
	if (cut) {
		int opened;
		int r = readable(fd, &opened);
		old = old_bytes(r, to, cut);
		if (opened) real_close(r);
	}
	emit('T', st.st_ino, to, 0, size, NULL, NULL, old, cut);
	free(old);
}

static int do_open(int dirfd, const char *path, int flags, mode_t mode, int at) {
	char abs[PATH_MAX];
	if (!on || !resolve(dirfd, path, abs) || !under(abs)) {
		return at ? real_openat(dirfd, path, flags, mode) : real_open(path, flags, mode);
	}
	pthread_mutex_lock(&mu);
	struct stat st;
	int existed = lstat(abs, &st) == 0;
	if (existed && S_ISREG(st.st_mode) && (flags & O_TRUNC) && (flags & O_ACCMODE) != O_RDONLY && st.st_size > 0) {
		int r = real_open(abs, O_RDONLY | O_CLOEXEC);
		if (r >= 0) {
			void *old = old_bytes(r, 0, (uint64_t)st.st_size);
			real_close(r);
			emit('T', st.st_ino, 0, 0, (uint64_t)st.st_size, NULL, NULL, old, (uint64_t)st.st_size);
			free(old);
		}
	}
	int fd = at ? real_openat(dirfd, path, flags, mode) : real_open(path, flags, mode);
	int e = errno;
	if (fd >= 0) {
		struct stat now;
		if (fstat(fd, &now)) die("fstat a new fd");
		if (!existed) {
			uint64_t pino;
			const char *name;
			if (!parent_of(abs, &pino, &name)) die("find a new file's parent");
			emit('C', now.st_ino, pino, 0, 0, name, NULL, NULL, 0);
		}
		track(fd, now.st_ino, (flags & O_APPEND) != 0);
	}
	pthread_mutex_unlock(&mu);
	errno = e;
	return fd;
}

static mode_t mode_arg(int flags, va_list ap) {
	return (flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE ? (mode_t)va_arg(ap, int) : 0;
}

int open(const char *p, int f, ...) { va_list ap; va_start(ap, f); mode_t m = mode_arg(f, ap); va_end(ap); return do_open(AT_FDCWD, p, f, m, 0); }
int open64(const char *p, int f, ...) { va_list ap; va_start(ap, f); mode_t m = mode_arg(f, ap); va_end(ap); return do_open(AT_FDCWD, p, f | O_LARGEFILE, m, 0); }
int openat(int d, const char *p, int f, ...) { va_list ap; va_start(ap, f); mode_t m = mode_arg(f, ap); va_end(ap); return do_open(d, p, f, m, 1); }
int openat64(int d, const char *p, int f, ...) { va_list ap; va_start(ap, f); mode_t m = mode_arg(f, ap); va_end(ap); return do_open(d, p, f | O_LARGEFILE, m, 1); }
int __open_2(const char *p, int f) { return do_open(AT_FDCWD, p, f, 0, 0); }
int __open64_2(const char *p, int f) { return do_open(AT_FDCWD, p, f | O_LARGEFILE, 0, 0); }
int __openat_2(int d, const char *p, int f) { return do_open(d, p, f, 0, 1); }
int __openat64_2(int d, const char *p, int f) { return do_open(d, p, f | O_LARGEFILE, 0, 1); }
int creat(const char *p, mode_t m) { return do_open(AT_FDCWD, p, O_CREAT | O_WRONLY | O_TRUNC, m, 0); }
int creat64(const char *p, mode_t m) { return do_open(AT_FDCWD, p, O_CREAT | O_WRONLY | O_TRUNC | O_LARGEFILE, m, 0); }

int close(int fd) {
	if (watched(fd)) {
		pthread_mutex_lock(&mu);
		fd_ino[fd] = 0;
		int r = real_close(fd);
		pthread_mutex_unlock(&mu);
		return r;
	}
	return real_close(fd);
}

REAL(closedir, int, DIR *);
int closedir(DIR *d) {
	if (!real_closedir) real_closedir = dlsym(RTLD_NEXT, "closedir");
	int fd = dirfd(d);
	if (watched(fd)) {
		pthread_mutex_lock(&mu);
		fd_ino[fd] = 0;
		int r = real_closedir(d);
		pthread_mutex_unlock(&mu);
		return r;
	}
	return real_closedir(d);
}

static void copy_track(int from, int to) {
	if (to >= 0 && to < MAXFD) {
		fd_ino[to] = from >= 0 && from < MAXFD ? fd_ino[from] : 0;
		fd_append[to] = from >= 0 && from < MAXFD ? fd_append[from] : 0;
	}
}

int dup(int fd) {
	pthread_mutex_lock(&mu);
	int r = real_dup(fd);
	if (r >= 0) copy_track(fd, r);
	pthread_mutex_unlock(&mu);
	return r;
}
int dup2(int fd, int to) {
	pthread_mutex_lock(&mu);
	int r = real_dup2(fd, to);
	if (r >= 0) copy_track(fd, r);
	pthread_mutex_unlock(&mu);
	return r;
}
int dup3(int fd, int to, int fl) {
	pthread_mutex_lock(&mu);
	int r = real_dup3(fd, to, fl);
	if (r >= 0) copy_track(fd, r);
	pthread_mutex_unlock(&mu);
	return r;
}

static int do_fcntl(int (*real)(int, int, ...), int fd, int cmd, va_list ap) {
	switch (cmd) {
	case F_DUPFD:
	case F_DUPFD_CLOEXEC: {
		int a = va_arg(ap, int);
		pthread_mutex_lock(&mu);
		int r = real(fd, cmd, a);
		if (r >= 0) copy_track(fd, r);
		pthread_mutex_unlock(&mu);
		return r;
	}
	case F_SETFL: {
		int a = va_arg(ap, int);
		int r = real(fd, cmd, a);
		if (r == 0 && fd >= 0 && fd < MAXFD) fd_append[fd] = (a & O_APPEND) != 0;
		return r;
	}
	case F_GETFD: case F_GETFL: case F_GETOWN: case F_GETSIG: case F_GETLEASE: case F_GETPIPE_SZ:
	case F_GET_SEALS:
		return real(fd, cmd);
	default: {
		void *a = va_arg(ap, void *);
		return real(fd, cmd, a);
	}
	}
}
int fcntl(int fd, int cmd, ...) { va_list ap; va_start(ap, cmd); int r = do_fcntl(real_fcntl, fd, cmd, ap); va_end(ap); return r; }
int fcntl64(int fd, int cmd, ...) { va_list ap; va_start(ap, cmd); int r = do_fcntl(real_fcntl64 ? real_fcntl64 : real_fcntl, fd, cmd, ap); va_end(ap); return r; }

static size_t iov_len(const struct iovec *v, int n) {
	size_t s = 0;
	for (int i = 0; i < n; i++) s += v[i].iov_len;
	return s;
}

#define WRITE_WRAP(expr, fd, off, len)                     \
	do {                                               \
		if (!watched(fd)) return expr;             \
		pthread_mutex_lock(&mu);                   \
		note_write(fd, off, len);                  \
		ssize_t r_ = expr;                         \
		int e_ = errno;                            \
		pthread_mutex_unlock(&mu);                 \
		errno = e_;                                \
		return r_;                                 \
	} while (0)

ssize_t write(int fd, const void *b, size_t n) { WRITE_WRAP(real_write(fd, b, n), fd, -1, n); }
ssize_t pwrite(int fd, const void *b, size_t n, off_t o) { WRITE_WRAP(real_pwrite(fd, b, n, o), fd, o, n); }
ssize_t pwrite64(int fd, const void *b, size_t n, off64_t o) { WRITE_WRAP(real_pwrite64(fd, b, n, o), fd, o, n); }
ssize_t writev(int fd, const struct iovec *v, int c) { WRITE_WRAP(real_writev(fd, v, c), fd, -1, iov_len(v, c)); }
ssize_t pwritev(int fd, const struct iovec *v, int c, off_t o) { WRITE_WRAP(real_pwritev(fd, v, c, o), fd, o, iov_len(v, c)); }
ssize_t pwritev64(int fd, const struct iovec *v, int c, off64_t o) { WRITE_WRAP(real_pwritev64(fd, v, c, o), fd, o, iov_len(v, c)); }
ssize_t pwritev2(int fd, const struct iovec *v, int c, off_t o, int fl) { WRITE_WRAP(real_pwritev2(fd, v, c, o, fl), fd, o, iov_len(v, c)); }
ssize_t pwritev64v2(int fd, const struct iovec *v, int c, off64_t o, int fl) { WRITE_WRAP(real_pwritev64v2(fd, v, c, o, fl), fd, o, iov_len(v, c)); }
ssize_t copy_file_range(int i, off64_t *io, int o, off64_t *oo, size_t n, unsigned fl) {
	WRITE_WRAP(real_copy_file_range(i, io, o, oo, n, fl), o, oo ? *oo : -1, n);
}
ssize_t sendfile(int o, int i, off_t *io, size_t n) { WRITE_WRAP(real_sendfile(o, i, io, n), o, -1, n); }
ssize_t sendfile64(int o, int i, off64_t *io, size_t n) { WRITE_WRAP(real_sendfile64(o, i, io, n), o, -1, n); }

/* fsync / fdatasync: what was recorded before it started is durable once it returns 0. */
static int do_sync(int (*real)(int), int fd) {
	if (!watched(fd)) return real(fd);
	pthread_mutex_lock(&mu);
	uint64_t from = seq;
	pthread_mutex_unlock(&mu);
	int r = real(fd);
	int e = errno;
	if (r == 0) {
		struct stat st;
		pthread_mutex_lock(&mu);
		if (still(fd, &st)) emit(S_ISDIR(st.st_mode) ? 'D' : 'F', st.st_ino, from, 0, 0, NULL, NULL, NULL, 0);
		pthread_mutex_unlock(&mu);
	}
	errno = e;
	return r;
}
int fsync(int fd) { return do_sync(real_fsync, fd); }
int fdatasync(int fd) { return do_sync(real_fdatasync, fd); }

static int note_global_sync(int (*f)(int), int fd) {
	pthread_mutex_lock(&mu);
	uint64_t from = seq;
	pthread_mutex_unlock(&mu);
	int r = f ? f(fd) : (real_sync(), 0);
	if (r == 0 && on) {
		pthread_mutex_lock(&mu);
		emit('S', 0, from, 0, 0, NULL, NULL, NULL, 0);
		pthread_mutex_unlock(&mu);
	}
	return r;
}
int syncfs(int fd) { if (!on) return real_syncfs(fd); return note_global_sync(real_syncfs, fd); }
void sync(void) { if (!on) { real_sync(); return; } note_global_sync(NULL, -1); }

#define SIZE_WRAP(expr, fd, to)                          \
	do {                                             \
		if (!watched(fd)) return expr;           \
		pthread_mutex_lock(&mu);                 \
		note_size(fd, to);                       \
		int r_ = expr;                           \
		int e_ = errno;                          \
		pthread_mutex_unlock(&mu);               \
		errno = e_;                              \
		return r_;                               \
	} while (0)

int ftruncate(int fd, off_t l) { SIZE_WRAP(real_ftruncate(fd, l), fd, (uint64_t)l); }
int ftruncate64(int fd, off64_t l) { SIZE_WRAP(real_ftruncate64(fd, l), fd, (uint64_t)l); }

static uint64_t grown(int fd, int mode, uint64_t off, uint64_t len) {
	struct stat st;
	if (fstat(fd, &st)) die("fstat a watched fd");
	uint64_t end = off + len, size = (uint64_t)st.st_size;
	return (mode & FALLOC_FL_KEEP_SIZE) || end <= size ? size : end;
}
int fallocate(int fd, int m, off_t o, off_t l) {
	if (watched(fd) && (m & ~FALLOC_FL_KEEP_SIZE)) {
		pthread_mutex_lock(&mu);
		emit('X', 0, 0, 0, 0, "fallocate with a mode that changes contents", NULL, NULL, 0);
		pthread_mutex_unlock(&mu);
	}
	SIZE_WRAP(real_fallocate(fd, m, o, l), fd, grown(fd, m, (uint64_t)o, (uint64_t)l));
}
int fallocate64(int fd, int m, off64_t o, off64_t l) {
	if (watched(fd) && (m & ~FALLOC_FL_KEEP_SIZE)) {
		pthread_mutex_lock(&mu);
		emit('X', 0, 0, 0, 0, "fallocate with a mode that changes contents", NULL, NULL, 0);
		pthread_mutex_unlock(&mu);
	}
	SIZE_WRAP(real_fallocate64(fd, m, o, l), fd, grown(fd, m, (uint64_t)o, (uint64_t)l));
}
int posix_fallocate(int fd, off_t o, off_t l) { SIZE_WRAP(real_posix_fallocate(fd, o, l), fd, grown(fd, 0, (uint64_t)o, (uint64_t)l)); }
int posix_fallocate64(int fd, off64_t o, off64_t l) { SIZE_WRAP(real_posix_fallocate64(fd, o, l), fd, grown(fd, 0, (uint64_t)o, (uint64_t)l)); }

static int do_truncate(const char *p, uint64_t to, int (*real)(const char *, off_t)) {
	char abs[PATH_MAX];
	if (!on || !resolve(AT_FDCWD, p, abs) || !under(abs)) return real(p, (off_t)to);
	int fd = real_open(abs, O_RDONLY | O_CLOEXEC);
	if (fd < 0) return real(p, (off_t)to);
	pthread_mutex_lock(&mu);
	note_size(fd, to);
	int r = real(p, (off_t)to);
	int e = errno;
	pthread_mutex_unlock(&mu);
	real_close(fd);
	errno = e;
	return r;
}
int truncate(const char *p, off_t l) { return do_truncate(p, (uint64_t)l, real_truncate); }
int truncate64(const char *p, off64_t l) { return do_truncate(p, (uint64_t)l, (int (*)(const char *, off_t))real_truncate64); }

/* POWERCUT_KILL=<n>:<0|1> (selftest.py): SIGKILL in the n-th rename or removal recorded, after
 * its record and before the change (0) or after the change (1). The caller holds mu. */
static void kill_point(uint64_t rs, int after) {
	static long n, at = -1, when;
	if (at < 0) {
		const char *v = getenv("POWERCUT_KILL");
		at = 0;
		if (v && sscanf(v, "%ld:%ld", &at, &when) != 2) at = 0;
	}
	if (!rs || !at) return;
	if (!after) n++;
	if (n == at && after == when) raise(SIGKILL);
}

static int do_rename(int fd1, const char *p1, int fd2, const char *p2, unsigned flags) {
	char a1[PATH_MAX], a2[PATH_MAX];
	int ok1 = on && resolve(fd1, p1, a1), ok2 = on && resolve(fd2, p2, a2);
	int u1 = ok1 && under(a1), u2 = ok2 && under(a2);
	if (!u1 && !u2) return real_renameat2 ? real_renameat2(fd1, p1, fd2, p2, flags) : real_renameat(fd1, p1, fd2, p2);
	pthread_mutex_lock(&mu);
	if (!u1 || !u2 || (flags & RENAME_EXCHANGE)) emit('X', 0, 0, 0, 0, "a rename across the root's edge or an exchange", a1, NULL, 0);
	struct stat s1, s2;
	int have1 = lstat(a1, &s1) == 0, have2 = lstat(a2, &s2) == 0;
	char keep[PATH_MAX] = "";
	/* The replaced file stays reachable from the grave until the cut. */
	if (have1 && have2 && s1.st_ino != s2.st_ino && !S_ISDIR(s2.st_mode)) {
		snprintf(keep, sizeof keep, "%s/o%llu.%llu", grave, (unsigned long long)s2.st_ino, (unsigned long long)seq);
		if (real_link(a2, keep)) die("link a replaced file into the grave");
	}
	/* Recorded before it is made (a kill between the two leaves the record's last entry not
	 * done, which the rebuild checks against the directory), cancelled if it fails. A rename
	 * recorded after it was made lost its record to a kill in between: both its names then
	 * differed from the record (agent 341's spill file moved into base/). */
	uint64_t p1i, p2i, rs = 0;
	const char *n1, *n2;
	if (have1 && parent_of(a1, &p1i, &n1) && parent_of(a2, &p2i, &n2)) {
		uint64_t over = keep[0] ? (uint64_t)s2.st_ino : 0;
		/* The grave link is o<inode>.<this record's seq> (nothing was recorded since its name). */
		rs = emit('R', s1.st_ino, p1i, p2i, over, n1, n2, NULL, 0);
	}
	kill_point(rs, 0);
	int r = flags ? real_renameat2(AT_FDCWD, a1, AT_FDCWD, a2, flags) : real_rename(a1, a2);
	int e = errno;
	kill_point(rs, 1);
	if (r != 0) {
		if (rs) emit('N', rs, 0, 0, 0, NULL, NULL, NULL, 0);
		if (keep[0]) real_unlinkat(AT_FDCWD, keep, 0);
	}
	pthread_mutex_unlock(&mu);
	errno = e;
	return r;
}
int rename(const char *a, const char *b) { return do_rename(AT_FDCWD, a, AT_FDCWD, b, 0); }
int renameat(int fa, const char *a, int fb, const char *b) { return do_rename(fa, a, fb, b, 0); }
int renameat2(int fa, const char *a, int fb, const char *b, unsigned fl) { return do_rename(fa, a, fb, b, fl); }

static int dir_empty(const char *p) {
	DIR *d = opendir(p);
	if (!d) return -1;
	struct dirent *e;
	int empty = 1;
	while ((e = readdir(d))) {
		if (strcmp(e->d_name, ".") && strcmp(e->d_name, "..")) { empty = 0; break; }
	}
	closedir(d);
	return empty;
}

/* unlink / rmdir under the root: moved into the grave instead. */
static int do_remove(int dirfd, const char *p, int flags) {
	char abs[PATH_MAX];
	if (!on || !resolve(dirfd, p, abs) || !under(abs)) {
		return real_unlinkat(dirfd, p, flags);
	}
	pthread_mutex_lock(&mu);
	struct stat st;
	int r;
	int e = 0;
	if (lstat(abs, &st)) {
		r = -1;
		e = errno;
	} else if (S_ISDIR(st.st_mode) != !!(flags & AT_REMOVEDIR)) {
		r = -1;
		e = S_ISDIR(st.st_mode) ? EISDIR : ENOTDIR;
	} else if (S_ISDIR(st.st_mode) && dir_empty(abs) != 1) {
		r = -1;
		e = ENOTEMPTY;
	} else {
		char to[PATH_MAX];
		uint64_t pino;
		const char *name;
		if (!parent_of(abs, &pino, &name)) die("find a removed entry's parent");
		/* As a rename: recorded first (the grave name is this record's seq), cancelled if
		 * the move fails. */
		snprintf(to, sizeof to, "%s/g%llu.%llu", grave, (unsigned long long)st.st_ino, (unsigned long long)seq);
		uint64_t rs = emit('U', st.st_ino, pino, S_ISDIR(st.st_mode), 0, name, NULL, NULL, 0);
		kill_point(rs, 0);
		r = real_rename(abs, to);
		e = errno;
		kill_point(rs, 1);
		if (r != 0) emit('N', rs, 0, 0, 0, NULL, NULL, NULL, 0);
	}
	pthread_mutex_unlock(&mu);
	errno = e;
	return r;
}
int unlink(const char *p) { return do_remove(AT_FDCWD, p, 0); }
int unlinkat(int d, const char *p, int fl) { return do_remove(d, p, fl); }
int rmdir(const char *p) { return do_remove(AT_FDCWD, p, AT_REMOVEDIR); }

static int do_mkdir(int dirfd, const char *p, mode_t m) {
	char abs[PATH_MAX];
	if (!on || !resolve(dirfd, p, abs) || !under(abs)) return real_mkdirat(dirfd, p, m);
	pthread_mutex_lock(&mu);
	int r = real_mkdirat(dirfd, p, m);
	int e = errno;
	if (r == 0) {
		struct stat st;
		uint64_t pino;
		const char *name;
		if (lstat(abs, &st) || !parent_of(abs, &pino, &name)) die("stat a new directory");
		emit('M', st.st_ino, pino, 0, 0, name, NULL, NULL, 0);
	}
	pthread_mutex_unlock(&mu);
	errno = e;
	return r;
}
int mkdir(const char *p, mode_t m) { return do_mkdir(AT_FDCWD, p, m); }
int mkdirat(int d, const char *p, mode_t m) { return do_mkdir(d, p, m); }

static int do_link(int f1, const char *p1, int f2, const char *p2, int flags) {
	char a1[PATH_MAX], a2[PATH_MAX];
	int u2 = on && resolve(f2, p2, a2) && under(a2);
	if (!u2) return real_linkat(f1, p1, f2, p2, flags);
	pthread_mutex_lock(&mu);
	int u1 = resolve(f1, p1, a1) && under(a1);
	int r = real_linkat(f1, p1, f2, p2, flags);
	int e = errno;
	if (r == 0) {
		struct stat st;
		uint64_t pino;
		const char *name;
		if (lstat(a2, &st) || !parent_of(a2, &pino, &name)) die("stat a new link");
		if (!u1) emit('X', st.st_ino, 0, 0, 0, "a hard link from outside the root", a1, NULL, 0);
		emit('L', st.st_ino, pino, 0, 0, name, NULL, NULL, 0);
	}
	pthread_mutex_unlock(&mu);
	errno = e;
	return r;
}
int link(const char *a, const char *b) { return do_link(AT_FDCWD, a, AT_FDCWD, b, 0); }
int linkat(int fa, const char *a, int fb, const char *b, int fl) { return do_link(fa, a, fb, b, fl); }

static int do_symlink(const char *t, int d, const char *p) {
	char abs[PATH_MAX];
	if (on && resolve(d, p, abs) && under(abs)) {
		pthread_mutex_lock(&mu);
		emit('X', 0, 0, 0, 0, "a symlink", abs, NULL, 0);
		pthread_mutex_unlock(&mu);
	}
	return real_symlinkat(t, d, p);
}
int symlink(const char *t, const char *p) { return do_symlink(t, AT_FDCWD, p); }
int symlinkat(const char *t, int d, const char *p) { return do_symlink(t, d, p); }
