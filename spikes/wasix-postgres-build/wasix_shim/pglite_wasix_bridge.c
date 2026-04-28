#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pwd.h>
#include <stdbool.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/ipc.h>
#include <sys/shm.h>

#ifndef EMSCRIPTEN_KEEPALIVE
#define EMSCRIPTEN_KEEPALIVE __attribute__((used))
#endif

#define PGLITE_UID 123
#define MAX_ATEXIT_FUNCS 32
#define PGLITE_PROTOCOL_FD 0

static unsigned char *pgl_wasix_input_buf;
static size_t pgl_wasix_input_len;
static size_t pgl_wasix_input_off;

static unsigned char *pgl_wasix_output_buf;
static size_t pgl_wasix_output_len_value;
static size_t pgl_wasix_output_cap;

static void (*atexit_funcs[MAX_ATEXIT_FUNCS])(void);
static int atexit_func_count;

int EMSCRIPTEN_KEEPALIVE
pgl_wasix_input_reset(void)
{
	pgl_wasix_input_len = 0;
	pgl_wasix_input_off = 0;
	return 0;
}

int EMSCRIPTEN_KEEPALIVE
pgl_wasix_input_write(const void *buffer, size_t length)
{
	if (length == 0)
		return 0;
	if (buffer == NULL)
	{
		errno = EINVAL;
		return -1;
	}

	if (pgl_wasix_input_off == pgl_wasix_input_len)
	{
		pgl_wasix_input_len = 0;
		pgl_wasix_input_off = 0;
	}

	size_t new_len = pgl_wasix_input_len + length;
	unsigned char *new_buf = realloc(pgl_wasix_input_buf, new_len);
	if (new_buf == NULL)
	{
		errno = ENOMEM;
		return -1;
	}

	pgl_wasix_input_buf = new_buf;
	memcpy(pgl_wasix_input_buf + pgl_wasix_input_len, buffer, length);
	pgl_wasix_input_len = new_len;
	return (int) length;
}

size_t EMSCRIPTEN_KEEPALIVE
pgl_wasix_input_available(void)
{
	if (pgl_wasix_input_off >= pgl_wasix_input_len)
		return 0;
	return pgl_wasix_input_len - pgl_wasix_input_off;
}

int EMSCRIPTEN_KEEPALIVE
pgl_wasix_input_peek(void)
{
	if (pgl_wasix_input_off >= pgl_wasix_input_len)
		return -1;
	return (int) pgl_wasix_input_buf[pgl_wasix_input_off];
}

static ssize_t
pgl_wasix_buffer_read(void *buffer, size_t max_length)
{
	if (buffer == NULL || max_length == 0)
		return 0;
	if (pgl_wasix_input_off >= pgl_wasix_input_len)
		return 0;

	size_t available = pgl_wasix_input_len - pgl_wasix_input_off;
	size_t to_copy = available < max_length ? available : max_length;
	memcpy(buffer, pgl_wasix_input_buf + pgl_wasix_input_off, to_copy);
	pgl_wasix_input_off += to_copy;
	return (ssize_t) to_copy;
}

int EMSCRIPTEN_KEEPALIVE
pgl_wasix_output_reset(void)
{
	pgl_wasix_output_len_value = 0;
	return 0;
}

size_t EMSCRIPTEN_KEEPALIVE
pgl_wasix_output_len(void)
{
	return pgl_wasix_output_len_value;
}

size_t EMSCRIPTEN_KEEPALIVE
pgl_wasix_output_read(void *buffer, size_t max_length)
{
	if (buffer == NULL || max_length == 0 || pgl_wasix_output_len_value == 0)
		return 0;

	size_t to_copy = pgl_wasix_output_len_value < max_length
		? pgl_wasix_output_len_value
		: max_length;
	memcpy(buffer, pgl_wasix_output_buf, to_copy);
	return to_copy;
}

static ssize_t
pgl_wasix_buffer_write(const void *buffer, size_t length)
{
	if (length == 0)
		return 0;
	if (buffer == NULL)
	{
		errno = EINVAL;
		return -1;
	}

	size_t required = pgl_wasix_output_len_value + length;
	if (required > pgl_wasix_output_cap)
	{
		size_t next_cap = pgl_wasix_output_cap ? pgl_wasix_output_cap : 8192;
		while (next_cap < required)
			next_cap *= 2;
		unsigned char *new_buf = realloc(pgl_wasix_output_buf, next_cap);
		if (new_buf == NULL)
		{
			errno = ENOMEM;
			return -1;
		}
		pgl_wasix_output_buf = new_buf;
		pgl_wasix_output_cap = next_cap;
	}

	memcpy(pgl_wasix_output_buf + pgl_wasix_output_len_value, buffer, length);
	pgl_wasix_output_len_value += length;
	return (ssize_t) length;
}

int EMSCRIPTEN_KEEPALIVE
pgl_system(const char *command)
{
	(void) command;
	errno = ENOSYS;
	return -1;
}

__attribute__((weak)) void EMSCRIPTEN_KEEPALIVE
pg_free(void *ptr)
{
	free(ptr);
}

static char *
pgl_locale_file_path(void)
{
	const char *dir = getenv("PGSYSCONFDIR");
	if (dir == NULL || dir[0] == '\0')
		dir = "/base";

	const char *name = "/locale";
	size_t len = strlen(dir) + strlen(name) + 1;
	char *path = malloc(len);
	if (path == NULL)
		return NULL;

	snprintf(path, len, "%s%s", dir, name);
	return path;
}

FILE *EMSCRIPTEN_KEEPALIVE
OpenPipeStream(const char *command, const char *mode)
{
	if (command == NULL || mode == NULL || strcmp(command, "locale -a") != 0 ||
		strcmp(mode, "r") != 0)
	{
		errno = ENOSYS;
		return NULL;
	}

	char *path = pgl_locale_file_path();
	if (path == NULL)
	{
		errno = ENOMEM;
		return NULL;
	}

	if (access(path, F_OK) != 0)
	{
		FILE *file = fopen(path, "w");
		if (file != NULL)
		{
			const char *encoding = getenv("PGCLIENTENCODING");
			if (encoding == NULL || encoding[0] == '\0')
				encoding = "UTF8";
			fprintf(file, "C\nC.%s\nPOSIX\n%s\n", encoding, encoding);
			fclose(file);
		}
	}

	FILE *file = fopen(path, mode);
	free(path);
	return file;
}

__attribute__((weak)) FILE *EMSCRIPTEN_KEEPALIVE
pgl_popen(const char *command, const char *mode)
{
	return OpenPipeStream(command, mode);
}

__attribute__((weak)) int EMSCRIPTEN_KEEPALIVE
pgl_pclose(FILE *file)
{
	if (file == NULL)
	{
		errno = EINVAL;
		return -1;
	}
	return fclose(file);
}

uid_t EMSCRIPTEN_KEEPALIVE
pgl_geteuid(void)
{
	return PGLITE_UID;
}

uid_t EMSCRIPTEN_KEEPALIVE
pgl_getuid(void)
{
	return PGLITE_UID;
}

struct passwd *EMSCRIPTEN_KEEPALIVE
pgl_getpwuid(uid_t uid)
{
	if (uid != PGLITE_UID)
	{
		errno = ENOENT;
		return NULL;
	}

	static struct passwd pw;
	static char name[] = "postgres";
	static char passwd[] = "x";
	static char gecos[] = "Static User";
	static char dir[] = "/home/postgres";
	static char shell[] = "/bin/sh";

	pw.pw_name = name;
	pw.pw_passwd = passwd;
	pw.pw_uid = uid;
	pw.pw_gid = uid;
	pw.pw_gecos = gecos;
	pw.pw_dir = dir;
	pw.pw_shell = shell;

	return &pw;
}

int EMSCRIPTEN_KEEPALIVE
pgl_atexit(void (*function)(void))
{
	if (atexit_func_count >= MAX_ATEXIT_FUNCS)
		return -1;
	atexit_funcs[atexit_func_count++] = function;
	return 0;
}

void EMSCRIPTEN_KEEPALIVE
pgl_run_atexit_funcs(void)
{
	for (int i = atexit_func_count - 1; i >= 0; --i)
	{
		if (atexit_funcs[i])
			atexit_funcs[i]();
	}
	atexit_func_count = 0;
}

void EMSCRIPTEN_KEEPALIVE
pgl_exit(int status)
{
	pgl_run_atexit_funcs();
	optind = 1;
	exit(status);
}

int EMSCRIPTEN_KEEPALIVE
pgl_munmap(void *addr, size_t length)
{
	if (addr == NULL || length == 0)
	{
		errno = EINVAL;
		return -1;
	}
	return munmap(addr, length);
}

int EMSCRIPTEN_KEEPALIVE
pgl_fcntl(int fd, int cmd, ...)
{
	va_list args;
	long arg = 0;

	switch (cmd)
	{
#ifdef F_GETFL
		case F_GETFL:
			if (fd == PGLITE_PROTOCOL_FD)
				return 0;
			return fcntl(fd, cmd);
#endif
#ifdef F_GETFD
		case F_GETFD:
			if (fd == PGLITE_PROTOCOL_FD)
				return 0;
			return fcntl(fd, cmd);
#endif
#ifdef F_SETFL
		case F_SETFL:
			va_start(args, cmd);
			arg = va_arg(args, long);
			va_end(args);
			if (fd == PGLITE_PROTOCOL_FD)
			{
#ifdef O_NONBLOCK
				if ((arg & ~((long) O_NONBLOCK)) == 0)
					return 0;
#else
				if (arg == 0)
					return 0;
#endif
				errno = EINVAL;
				return -1;
			}
			return fcntl(fd, cmd, (int) arg);
#endif
#ifdef F_SETFD
		case F_SETFD:
			va_start(args, cmd);
			arg = va_arg(args, long);
			va_end(args);
			if (fd == PGLITE_PROTOCOL_FD)
			{
#ifdef FD_CLOEXEC
				if ((arg & ~((long) FD_CLOEXEC)) == 0)
					return 0;
#else
				if (arg == 0)
					return 0;
#endif
				errno = EINVAL;
				return -1;
			}
			return fcntl(fd, cmd, (int) arg);
#endif
		default:
			errno = EINVAL;
			return -1;
	}
}

static int
pgl_write_int_sockopt(void *optval, socklen_t *optlen, int value)
{
	if (optval == NULL || optlen == NULL || *optlen < (socklen_t) sizeof(int))
	{
		errno = EINVAL;
		return -1;
	}
	memcpy(optval, &value, sizeof(value));
	*optlen = (socklen_t) sizeof(value);
	return 0;
}

int EMSCRIPTEN_KEEPALIVE
pgl_setsockopt(int fd, int level, int optname, const void *optval, socklen_t optlen)
{
	if (fd != PGLITE_PROTOCOL_FD)
		return setsockopt(fd, level, optname, optval, optlen);

	if (optval == NULL && optlen != 0)
	{
		errno = EINVAL;
		return -1;
	}

	if (level == SOL_SOCKET)
	{
		switch (optname)
		{
#ifdef SO_KEEPALIVE
			case SO_KEEPALIVE:
#endif
#ifdef SO_REUSEADDR
			case SO_REUSEADDR:
#endif
#ifdef SO_SNDBUF
			case SO_SNDBUF:
#endif
#ifdef SO_RCVBUF
			case SO_RCVBUF:
#endif
#ifdef SO_NOSIGPIPE
			case SO_NOSIGPIPE:
#endif
				return 0;
			default:
				break;
		}
	}

	if (level == IPPROTO_TCP)
	{
		switch (optname)
		{
#ifdef TCP_NODELAY
			case TCP_NODELAY:
#endif
#ifdef TCP_KEEPIDLE
			case TCP_KEEPIDLE:
#endif
#ifdef TCP_KEEPINTVL
			case TCP_KEEPINTVL:
#endif
#ifdef TCP_KEEPCNT
			case TCP_KEEPCNT:
#endif
#ifdef TCP_USER_TIMEOUT
			case TCP_USER_TIMEOUT:
#endif
				return 0;
			default:
				break;
		}
	}

	errno = ENOPROTOOPT;
	return -1;
}

int EMSCRIPTEN_KEEPALIVE
pgl_getsockopt(int fd, int level, int optname, void *optval, socklen_t *optlen)
{
	if (fd != PGLITE_PROTOCOL_FD)
		return getsockopt(fd, level, optname, optval, optlen);

	if (level == SOL_SOCKET)
	{
		switch (optname)
		{
#ifdef SO_ERROR
			case SO_ERROR:
				return pgl_write_int_sockopt(optval, optlen, 0);
#endif
#ifdef SO_TYPE
			case SO_TYPE:
				return pgl_write_int_sockopt(optval, optlen, SOCK_STREAM);
#endif
#ifdef SO_SNDBUF
			case SO_SNDBUF:
				return pgl_write_int_sockopt(optval, optlen, 32768);
#endif
#ifdef SO_RCVBUF
			case SO_RCVBUF:
				return pgl_write_int_sockopt(optval, optlen, 32768);
#endif
			default:
				break;
		}
	}

	if (level == IPPROTO_TCP)
	{
		switch (optname)
		{
#ifdef TCP_KEEPIDLE
			case TCP_KEEPIDLE:
#endif
#ifdef TCP_KEEPINTVL
			case TCP_KEEPINTVL:
#endif
#ifdef TCP_KEEPCNT
			case TCP_KEEPCNT:
#endif
#ifdef TCP_USER_TIMEOUT
			case TCP_USER_TIMEOUT:
#endif
				return pgl_write_int_sockopt(optval, optlen, 0);
			default:
				break;
		}
	}

	errno = ENOPROTOOPT;
	return -1;
}

int EMSCRIPTEN_KEEPALIVE
pgl_getsockname(int fd, struct sockaddr *addr, socklen_t *len)
{
	if (fd != PGLITE_PROTOCOL_FD)
		return getsockname(fd, addr, len);

	if (addr == NULL || len == NULL || *len < (socklen_t) sizeof(sa_family_t))
	{
		errno = EINVAL;
		return -1;
	}

	memset(addr, 0, *len);
	addr->sa_family = AF_UNIX;
	*len = (socklen_t) sizeof(sa_family_t);
	return 0;
}

ssize_t EMSCRIPTEN_KEEPALIVE
pgl_recv(int fd, void *buf, size_t n, int flags)
{
	if (fd != PGLITE_PROTOCOL_FD)
		return recv(fd, buf, n, flags);
	return pgl_wasix_buffer_read(buf, n);
}

ssize_t EMSCRIPTEN_KEEPALIVE
pgl_send(int fd, const void *buf, size_t n, int flags)
{
	if (fd != PGLITE_PROTOCOL_FD)
		return send(fd, buf, n, flags);
	return pgl_wasix_buffer_write(buf, n);
}

int EMSCRIPTEN_KEEPALIVE
pgl_connect(int socket, const struct sockaddr *address, socklen_t address_len)
{
	if (socket != PGLITE_PROTOCOL_FD)
		return connect(socket, address, address_len);
	errno = ENOSYS;
	return -1;
}

int EMSCRIPTEN_KEEPALIVE
pgl_poll(struct pollfd fds[], nfds_t nfds, int timeout)
{
	bool has_protocol_fd = false;
	int ready = 0;

	for (nfds_t i = 0; i < nfds; i++)
	{
		if (fds[i].fd == PGLITE_PROTOCOL_FD)
		{
			has_protocol_fd = true;
			break;
		}
	}

	if (!has_protocol_fd)
		return poll(fds, nfds, timeout);

	for (nfds_t i = 0; i < nfds; i++)
	{
		fds[i].revents = 0;
		if (fds[i].fd != PGLITE_PROTOCOL_FD)
		{
			struct pollfd one = fds[i];
			int rc = poll(&one, 1, 0);
			if (rc < 0)
				return rc;
			fds[i].revents = one.revents;
			if (rc > 0)
				ready++;
			continue;
		}
#ifdef POLLIN
		if ((fds[i].events & POLLIN) && pgl_wasix_input_available() > 0)
			fds[i].revents |= POLLIN;
#endif
#ifdef POLLOUT
		if (fds[i].events & POLLOUT)
			fds[i].revents |= POLLOUT;
#endif
		if (fds[i].revents)
			ready++;
	}
	return ready;
}

typedef struct WasixShmSegment
{
	int shmid;
	key_t key;
	size_t size;
	void *addr;
	unsigned long nattch;
	struct WasixShmSegment *next;
} WasixShmSegment;

static WasixShmSegment *wasix_shm_list;
static int wasix_next_shmid = 1;

static WasixShmSegment *
find_by_key(key_t key)
{
	for (WasixShmSegment *seg = wasix_shm_list; seg; seg = seg->next)
	{
		if (seg->key == key)
			return seg;
	}
	return NULL;
}

static WasixShmSegment *
find_by_id(int shmid)
{
	for (WasixShmSegment *seg = wasix_shm_list; seg; seg = seg->next)
	{
		if (seg->shmid == shmid)
			return seg;
	}
	return NULL;
}

int EMSCRIPTEN_KEEPALIVE
pgl_shmget(key_t key, size_t size, int shmflg)
{
	WasixShmSegment *existing = find_by_key(key);

	if (existing)
	{
		if ((shmflg & IPC_CREAT) && (shmflg & IPC_EXCL))
		{
			errno = EEXIST;
			return -1;
		}
		return existing->shmid;
	}

	if ((shmflg & IPC_CREAT) == 0)
	{
		errno = ENOENT;
		return -1;
	}

	size_t alloc_size = size ? size : 1;
	long pagesize = sysconf(_SC_PAGESIZE);
	if (pagesize > 0)
	{
		size_t page = (size_t) pagesize;
		alloc_size = ((alloc_size + page - 1) / page) * page;
	}

	void *addr = calloc(1, alloc_size);
	if (!addr)
	{
		errno = ENOMEM;
		return -1;
	}

	WasixShmSegment *seg = calloc(1, sizeof(*seg));
	if (!seg)
	{
		free(addr);
		errno = ENOMEM;
		return -1;
	}

	seg->shmid = wasix_next_shmid++;
	seg->key = key;
	seg->size = size;
	seg->addr = addr;
	seg->next = wasix_shm_list;
	wasix_shm_list = seg;

	return seg->shmid;
}

void *EMSCRIPTEN_KEEPALIVE
pgl_shmat(int shmid, const void *shmaddr, int shmflg)
{
	(void) shmaddr;
	(void) shmflg;

	WasixShmSegment *seg = find_by_id(shmid);
	if (!seg)
	{
		errno = EINVAL;
		return (void *) -1;
	}

	seg->nattch++;
	return seg->addr;
}

int EMSCRIPTEN_KEEPALIVE
pgl_shmdt(const void *shmaddr)
{
	for (WasixShmSegment *seg = wasix_shm_list; seg; seg = seg->next)
	{
		if (seg->addr == shmaddr)
		{
			if (seg->nattch > 0)
				seg->nattch--;
			return 0;
		}
	}

	errno = EINVAL;
	return -1;
}

int EMSCRIPTEN_KEEPALIVE
pgl_shmctl(int shmid, int cmd, struct shmid_ds *buf)
{
	WasixShmSegment *prev = NULL;
	WasixShmSegment *seg = wasix_shm_list;

	while (seg && seg->shmid != shmid)
	{
		prev = seg;
		seg = seg->next;
	}

	if (!seg)
	{
		errno = EINVAL;
		return -1;
	}

	switch (cmd)
	{
		case IPC_RMID:
			if (prev)
				prev->next = seg->next;
			else
				wasix_shm_list = seg->next;
			free(seg->addr);
			free(seg);
			return 0;

		case IPC_STAT:
			if (!buf)
			{
				errno = EINVAL;
				return -1;
			}
			memset(buf, 0, sizeof(*buf));
#if defined(__APPLE__)
			buf->shm_perm._key = seg->key;
#else
			buf->shm_perm.__key = seg->key;
#endif
			buf->shm_segsz = seg->size;
			buf->shm_nattch = seg->nattch;
			buf->shm_atime = buf->shm_dtime = buf->shm_ctime = time(NULL);
			return 0;

		case IPC_SET:
			if (!buf)
			{
				errno = EINVAL;
				return -1;
			}
			seg->size = buf->shm_segsz;
			return 0;

		default:
			errno = EINVAL;
			return -1;
	}
}
