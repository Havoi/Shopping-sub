#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int64_t now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts)) _exit(125);
    return (int64_t)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static int64_t timeval_us(struct timeval ts) {
    return (int64_t)ts.tv_sec * 1000000LL + ts.tv_usec;
}

int main(int argc, char **argv) {
    const int64_t start = now_ns();
    int64_t child_start = 0, exit_line = 0, waited = 0;
    struct rusage usage = {0};
    int status = 0;
    if (argc > 1 && strcmp(argv[1], "--noop")) {
        int errfd[2], tickfd[2];
        if (pipe(errfd) || pipe(tickfd)) return 125;
        pid_t child = fork();
        if (child < 0) return 125;
        if (!child) {
            close(errfd[0]); close(tickfd[0]);
            if (dup2(errfd[1], STDERR_FILENO) < 0) _exit(125);
            close(errfd[1]);
            int64_t at = now_ns();
            if (write(tickfd[1], &at, sizeof(at)) != sizeof(at)) _exit(125);
            close(tickfd[1]);
            execvp(argv[1], argv + 1);
            _exit(127);
        }
        close(errfd[1]); close(tickfd[1]);
        ssize_t n;
        do { n = read(tickfd[0], &child_start, sizeof(child_start)); } while (n < 0 && errno == EINTR);
        close(tickfd[0]);
        if (n != sizeof(child_start)) return 125;
        FILE *errors = fdopen(errfd[0], "r");
        if (!errors) return 125;
        char line[4096];
        while (fgets(line, sizeof(line), errors)) {
            if (!strncmp(line, "[v2b] exit at ", 14)) {
                exit_line = now_ns();
                fputs(line, stderr);
            }
        }
        fclose(errors);
        pid_t got;
        do { got = wait4(child, &status, 0, &usage); } while (got < 0 && errno == EINTR);
        waited = now_ns();
        if (got != child) return 125;
    }
    // The final timestamp immediately precedes the probe's own diagnostic write and return.
    fprintf(stderr, "[runtime-clock] %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64 " %" PRId64
            " %" PRId64 " %" PRId64 " %ld %ld %ld\n",
            start, child_start, exit_line, waited, now_ns(), timeval_us(usage.ru_utime),
            timeval_us(usage.ru_stime), usage.ru_maxrss, usage.ru_minflt, usage.ru_majflt);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
