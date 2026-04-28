#include <dlfcn.h>
#include <stdio.h>

extern void needed_say_hello(void);

typedef void (*say_hello_fn)(const char *);

int main(void) {
  printf("Hello from the main program.\n");
  needed_say_hello();

  void *handle = dlopen("./libdlopened.so", RTLD_NOW);
  if (!handle) {
    printf("dlopen failed: %s\n", dlerror());
    return 1;
  }

  say_hello_fn say_hello = (say_hello_fn)dlsym(handle, "dlopened_say_hello");
  if (!say_hello) {
    printf("dlsym failed: %s\n", dlerror());
    return 2;
  }

  say_hello("pglite-oxide");
  printf("All done.\n");
  return 0;
}
