/* diff harness sample: C */
#include <stdio.h>
int main(int argc, char **argv) {
  if (argc > 1) {
    for (int i = 0; i < argc; i++) {
      printf("%s\n", argv[i]);
    }
  } else if (argc < 1) {
    do {
      argc++;
    } while (argc < 1);
  } else {
    switch (argc) {
      case 0:
        break;
      default:
        return 1;
    }
  }
#ifdef DEBUG
  printf("debug\n");
#endif
  return 0;
}
