#include <dlfcn.h>
#include <stdio.h>

typedef void *(*symbol_ptr_fn)(void);

int
main(void)
{
	void *handle = dlopen("./min_ext.so", RTLD_NOW | RTLD_GLOBAL);

	if (handle == NULL)
	{
		printf("dlopen failed: %s\n", dlerror());
		return 1;
	}

	symbol_ptr_fn magic = (symbol_ptr_fn) dlsym(handle, "Pg_magic_func");
	if (magic == NULL)
	{
		printf("dlsym Pg_magic_func failed: %s\n", dlerror());
		return 2;
	}

	symbol_ptr_fn finfo = (symbol_ptr_fn) dlsym(handle, "pg_finfo_wasix_min_ext_add_one");
	if (finfo == NULL)
	{
		printf("dlsym pg_finfo_wasix_min_ext_add_one failed: %s\n", dlerror());
		return 3;
	}

	printf("Pg_magic_func pointer: %p\n", magic());
	printf("pg_finfo pointer: %p\n", finfo());
	printf("Postgres-shaped extension dlopen proof passed.\n");
	return 0;
}
