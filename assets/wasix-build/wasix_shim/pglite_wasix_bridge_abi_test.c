#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pwd.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/shm.h>

#define CHECK(condition)                                                                 \
	do                                                                                   \
	{                                                                                    \
		if (!(condition))                                                                \
		{                                                                                \
			fprintf(stderr, "bridge ABI check failed at %s:%d: %s\n", __FILE__, __LINE__, \
					#condition);                                                        \
			return 1;                                                                    \
		}                                                                                \
	} while (0)

FILE *OpenPipeStream(const char *command, const char *mode);
int pgl_system(const char *command);
uid_t pgl_geteuid(void);
uid_t pgl_getuid(void);
struct passwd *pgl_getpwuid(uid_t uid);
int pgl_wasix_input_reset(void);
int pgl_wasix_input_write(const void *buffer, size_t length);
size_t pgl_wasix_input_available(void);
int pgl_wasix_output_reset(void);
size_t pgl_wasix_output_len(void);
size_t pgl_wasix_output_read(void *buffer, size_t max_length);
int pgl_fcntl(int fd, int cmd, ...);
int pgl_setsockopt(int fd, int level, int optname, const void *optval, socklen_t optlen);
int pgl_getsockopt(int fd, int level, int optname, void *optval, socklen_t *optlen);
int pgl_getsockname(int fd, struct sockaddr *addr, socklen_t *len);
ssize_t pgl_recv(int fd, void *buf, size_t n, int flags);
ssize_t pgl_send(int fd, const void *buf, size_t n, int flags);
int pgl_connect(int socket, const struct sockaddr *address, socklen_t address_len);
int pgl_poll(struct pollfd fds[], nfds_t nfds, int timeout);
int pgl_munmap(void *addr, size_t length);
int pgl_shmget(key_t key, size_t size, int shmflg);
void *pgl_shmat(int shmid, const void *shmaddr, int shmflg);
int pgl_shmdt(const void *shmaddr);
int pgl_shmctl(int shmid, int cmd, struct shmid_ds *buf);

static int
check_locale_pipe(void)
{
	char temp_template[] = "/tmp/pglite-bridge-abi-XXXXXX";
	char *dir = mkdtemp(temp_template);
	CHECK(dir != NULL);
	CHECK(setenv("PGSYSCONFDIR", dir, 1) == 0);
	CHECK(setenv("PGCLIENTENCODING", "UTF8", 1) == 0);

	errno = 0;
	CHECK(OpenPipeStream("uname -a", "r") == NULL);
	CHECK(errno == ENOSYS);
	errno = 0;
	CHECK(OpenPipeStream("locale -a", "w") == NULL);
	CHECK(errno == ENOSYS);

	FILE *file = OpenPipeStream("locale -a", "r");
	CHECK(file != NULL);
	char contents[128] = {0};
	size_t read_len = fread(contents, 1, sizeof(contents) - 1, file);
	CHECK(fclose(file) == 0);
	CHECK(read_len > 0);
	CHECK(strstr(contents, "C\n") != NULL);
	CHECK(strstr(contents, "C.UTF8\n") != NULL);
	CHECK(strstr(contents, "POSIX\n") != NULL);
	return 0;
}

static int
check_identity_and_fail_closed_calls(void)
{
	CHECK(pgl_geteuid() == 123);
	CHECK(pgl_getuid() == 123);
	struct passwd *pw = pgl_getpwuid(123);
	CHECK(pw != NULL);
	CHECK(strcmp(pw->pw_name, "postgres") == 0);
	CHECK(pw->pw_uid == 123);
	errno = 0;
	CHECK(pgl_getpwuid(999) == NULL);
	CHECK(errno == ENOENT);

	errno = 0;
	CHECK(pgl_system("echo unsafe") == -1);
	CHECK(errno == ENOSYS);

	errno = 0;
	CHECK(pgl_connect(0, NULL, 0) == -1);
	CHECK(errno == ENOSYS);
	errno = 0;
	CHECK(pgl_connect(-1, NULL, 0) == -1);
	CHECK(errno == EBADF);
	return 0;
}

static int
check_protocol_socket(void)
{
	char buf[8] = {0};
	const char input[] = "abc";
	const char output[] = "xyz";

	CHECK(pgl_wasix_input_reset() == 0);
	CHECK(pgl_wasix_output_reset() == 0);
	CHECK(pgl_recv(0, buf, sizeof(buf), 0) == 0);
	CHECK(pgl_wasix_input_write(input, sizeof(input) - 1) == (int) (sizeof(input) - 1));
	CHECK(pgl_wasix_input_available() == sizeof(input) - 1);
	CHECK(pgl_recv(0, buf, 2, 0) == 2);
	CHECK(memcmp(buf, "ab", 2) == 0);
	CHECK(pgl_wasix_input_available() == 1);

	CHECK(pgl_send(0, output, sizeof(output) - 1, 0) == (ssize_t) (sizeof(output) - 1));
	CHECK(pgl_wasix_output_len() == sizeof(output) - 1);
	memset(buf, 0, sizeof(buf));
	CHECK(pgl_wasix_output_read(buf, sizeof(buf)) == sizeof(output) - 1);
	CHECK(memcmp(buf, output, sizeof(output) - 1) == 0);

#ifdef ENOTSOCK
	errno = 0;
	CHECK(pgl_recv(1, buf, sizeof(buf), 0) == -1);
	CHECK(errno == ENOTSOCK);
	errno = 0;
	CHECK(pgl_send(1, output, sizeof(output) - 1, 0) == -1);
	CHECK(errno == ENOTSOCK);
#endif

	CHECK(pgl_fcntl(0, F_GETFL) == 0);
	CHECK(pgl_fcntl(0, F_SETFL, O_NONBLOCK) == 0);
#ifdef O_APPEND
	errno = 0;
	CHECK(pgl_fcntl(0, F_SETFL, O_APPEND) == -1);
	CHECK(errno == EINVAL);
#endif

	int opt = 1;
	CHECK(pgl_setsockopt(0, SOL_SOCKET, SO_KEEPALIVE, &opt, sizeof(opt)) == 0);
#ifdef TCP_NODELAY
	CHECK(pgl_setsockopt(0, IPPROTO_TCP, TCP_NODELAY, &opt, sizeof(opt)) == 0);
#endif
	errno = 0;
	CHECK(pgl_setsockopt(0, SOL_SOCKET, 0x7ffffffe, &opt, sizeof(opt)) == -1);
	CHECK(errno == ENOPROTOOPT);

	opt = 0;
	socklen_t optlen = sizeof(opt);
	CHECK(pgl_getsockopt(0, SOL_SOCKET, SO_TYPE, &opt, &optlen) == 0);
	CHECK(opt == SOCK_STREAM);
	CHECK(optlen == (socklen_t) sizeof(opt));
	errno = 0;
	optlen = sizeof(opt);
	CHECK(pgl_getsockopt(0, SOL_SOCKET, 0x7ffffffd, &opt, &optlen) == -1);
	CHECK(errno == ENOPROTOOPT);

	struct sockaddr_storage addr;
	socklen_t addrlen = sizeof(addr);
	CHECK(pgl_getsockname(0, (struct sockaddr *) &addr, &addrlen) == 0);
	CHECK(addr.ss_family == AF_UNIX);

	CHECK(pgl_wasix_input_reset() == 0);
	struct pollfd fds[1] = {{.fd = 0, .events = POLLIN, .revents = 0}};
	CHECK(pgl_poll(fds, 1, 0) == 0);
	CHECK(fds[0].revents == 0);
	CHECK(pgl_wasix_input_write("q", 1) == 1);
	CHECK(pgl_poll(fds, 1, 0) == 1);
	CHECK((fds[0].revents & POLLIN) != 0);

	struct pollfd ignored[1] = {{.fd = -1, .events = POLLIN, .revents = 0}};
	CHECK(pgl_poll(ignored, 1, 0) == 0);
	struct pollfd mixed[2] = {
		{.fd = 0, .events = POLLOUT, .revents = 0},
		{.fd = 99, .events = POLLIN, .revents = 0},
	};
	CHECK(pgl_poll(mixed, 2, 0) == 2);
	CHECK((mixed[0].revents & POLLOUT) != 0);
#ifdef POLLNVAL
	CHECK((mixed[1].revents & POLLNVAL) != 0);
#endif
	return 0;
}

static int
check_memory_and_shared_memory(void)
{
	errno = 0;
	CHECK(pgl_munmap(NULL, 0) == -1);
	CHECK(errno == EINVAL);

#if defined(MAP_ANON)
	int anon_flag = MAP_ANON;
#elif defined(MAP_ANONYMOUS)
	int anon_flag = MAP_ANONYMOUS;
#else
	int anon_flag = 0;
#endif
	if (anon_flag != 0)
	{
		void *mapping = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | anon_flag, -1, 0);
		CHECK(mapping != MAP_FAILED);
		CHECK(pgl_munmap(mapping, 4096) == 0);
	}

	key_t key = 4242;
	int shmid = pgl_shmget(key, 64, IPC_CREAT | IPC_EXCL);
	CHECK(shmid > 0);
	errno = 0;
	CHECK(pgl_shmget(key, 64, IPC_CREAT | IPC_EXCL) == -1);
	CHECK(errno == EEXIST);
	errno = 0;
	CHECK(pgl_shmget(key + 1, 64, 0) == -1);
	CHECK(errno == ENOENT);

	void *addr = pgl_shmat(shmid, NULL, 0);
	CHECK(addr != (void *) -1);
	memset(addr, 0x7b, 64);

	struct shmid_ds statbuf;
	CHECK(pgl_shmctl(shmid, IPC_STAT, &statbuf) == 0);
	CHECK(statbuf.shm_segsz == 64);
	CHECK(statbuf.shm_nattch == 1);
	CHECK(pgl_shmdt(addr) == 0);
	CHECK(pgl_shmctl(shmid, IPC_RMID, NULL) == 0);
	errno = 0;
	CHECK(pgl_shmat(shmid, NULL, 0) == (void *) -1);
	CHECK(errno == EINVAL);
	return 0;
}

int
main(void)
{
	CHECK(check_locale_pipe() == 0);
	CHECK(check_identity_and_fail_closed_calls() == 0);
	CHECK(check_protocol_socket() == 0);
	CHECK(check_memory_and_shared_memory() == 0);
	return 0;
}
