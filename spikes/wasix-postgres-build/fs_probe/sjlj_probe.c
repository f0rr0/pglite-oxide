#include <setjmp.h>
#include <stdio.h>

static jmp_buf jump_target;

int main(void) {
  int value = setjmp(jump_target);
  if (value == 0) {
    puts("sjlj: before longjmp");
    longjmp(jump_target, 42);
  }

  printf("sjlj: after longjmp value=%d\n", value);
  return value == 42 ? 0 : 1;
}
