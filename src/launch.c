#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    (void)argc;
    const char *program = "/usr/local/libexec/shopping-generic";
    if (__builtin_cpu_supports("aes") && __builtin_cpu_supports("x86-64-v3")) {
        program = "/usr/local/libexec/shopping-tuned";
    }
    execv(program, argv);
    perror(program);
    return 1;
}
