#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void print_stat(const char *path) {
  struct stat st;
  if (stat(path, &st) == 0) {
    printf("stat %s: ok mode=%o size=%lld\n", path, (unsigned)st.st_mode,
           (long long)st.st_size);
  } else {
    printf("stat %s: errno=%d %s\n", path, errno, strerror(errno));
  }
}

static void print_access(const char *path) {
  errno = 0;
  printf("access %s R_OK: %s", path, access(path, R_OK) == 0 ? "ok" : "fail");
  if (errno != 0) {
    printf(" errno=%d %s", errno, strerror(errno));
  }
  printf("\n");

  errno = 0;
  printf("access %s X_OK: %s", path, access(path, X_OK) == 0 ? "ok" : "fail");
  if (errno != 0) {
    printf(" errno=%d %s", errno, strerror(errno));
  }
  printf("\n");
}

static void print_dir(const char *path) {
  DIR *dir = opendir(path);
  if (!dir) {
    printf("opendir %s: errno=%d %s\n", path, errno, strerror(errno));
    return;
  }

  printf("opendir %s: ok", path);
  for (int i = 0; i < 8; i++) {
    struct dirent *entry = readdir(dir);
    if (!entry) {
      break;
    }
    printf(" %s", entry->d_name);
  }
  printf("\n");
  closedir(dir);
}

int main(int argc, char **argv) {
  char cwd[4096];
  if (getcwd(cwd, sizeof(cwd))) {
    printf("cwd: %s\n", cwd);
  } else {
    printf("getcwd: errno=%d %s\n", errno, strerror(errno));
  }

  printf("argv0: %s argc=%d\n", argc > 0 ? argv[0] : "(none)", argc);
  print_stat("/");
  print_stat("/bin");
  print_stat("/bin/pglite.wasi");
  print_access("/bin/pglite.wasi");
  print_stat("/base");
  print_stat("/base/PG_VERSION");
  print_stat("/share");
  print_stat("/share/postgresql");
  print_stat("/share/postgresql/timezonesets");
  print_stat("/share/postgresql/timezonesets/Default");
  print_access("/share/postgresql/timezonesets/Default");
  print_stat("/tmp/pglite/bin/pglite.wasi");
  print_stat("bin/pglite.wasi");
  print_stat("base/PG_VERSION");
  print_dir("/");
  print_dir("/bin");
  print_dir("/base");
  print_dir("/share/postgresql/timezonesets");

  return 0;
}
