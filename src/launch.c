#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    (void)argc;
    const char *program = __builtin_cpu_supports("x86-64-v3") && __builtin_cpu_supports("aes")
        ? "/usr/local/libexec/shopping-tuned"
        : "/usr/local/libexec/shopping-generic";
    execv(program, argv);
    perror(program);
    return 1;
}
