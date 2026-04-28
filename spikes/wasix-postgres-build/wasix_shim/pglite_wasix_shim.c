#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/ipc.h>
#include <sys/shm.h>

typedef struct WasixShmSegment
{
	int			shmid;
	key_t		key;
	size_t		size;
	void	   *addr;
	unsigned long nattch;
	struct WasixShmSegment *next;
} WasixShmSegment;

static WasixShmSegment *wasix_shm_list;
static int	wasix_next_shmid = 1;

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

int
shmget(key_t key, size_t size, int shmflg)
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

void *
shmat(int shmid, const void *shmaddr, int shmflg)
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

int
shmdt(const void *shmaddr)
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

int
shmctl(int shmid, int cmd, struct shmid_ds *buf)
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
			buf->shm_perm.__key = seg->key;
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
