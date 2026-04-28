#include <stdio.h>

void dlopened_say_hello(const char *message) {
  printf("Hello from the dlopened library, caller says: %s\n", message);
}
