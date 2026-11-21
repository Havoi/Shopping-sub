#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    (void)argc;
    const char *program = "/usr/local/libexec/shopping-generic";
    if (__builtin_cpu_supports("x86-64-v3")) {
        program = __builtin_cpu_supports("x86-64-v4")
            ? "/usr/local/libexec/shopping-native-v4"
            : "/usr/local/libexec/shopping-native";
        if (setenv("LD_PRELOAD", "/usr/lib/x86_64-linux-gnu/libjemalloc.so.2", 0) != 0) {
            perror("LD_PRELOAD");
            return 1;
        }
    }
    execv(program, argv);
    perror(program);
    return 1;
}
