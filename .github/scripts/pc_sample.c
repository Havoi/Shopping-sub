#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <signal.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <time.h>
#include <unistd.h>
#include <stdlib.h>
#include <fcntl.h>
#include <errno.h>

/* Non-PIE Linux x86-64. Every sampled thread owns a CPU-time clock. */
static int raw_fd = -1;
struct kernel_event {
    union sigval value;
    int signo, notify, tid;
    unsigned char pad[44];
};
_Static_assert(sizeof(struct kernel_event) == 64, "unexpected sigevent ABI");
static void sample(int sig, siginfo_t *si, void *context) {
    (void)sig;
    if (!si || si->si_code != SI_TIMER) return;
    int previous = errno;
    ucontext_t *uc = context;
    uint64_t ip = (uint64_t)uc->uc_mcontext.gregs[REG_RIP];
    ssize_t ignored = write(raw_fd, &ip, sizeof(ip));
    (void)ignored;
    errno = previous;
}
static void arm(void) {
    int tid = (int)syscall(SYS_gettid), timer = -1;
    struct kernel_event event = {.signo=SIGPROF, .notify=4, .tid=tid};
    if (syscall(SYS_timer_create, CLOCK_THREAD_CPUTIME_ID, &event, &timer) != 0) _exit(106);
    long ns = 526000L + (tid % 101) * 1000L;
    struct itimerspec ticks = {.it_value={0,ns}, .it_interval={0,ns}};
    if (syscall(SYS_timer_settime,timer,0,&ticks,NULL) != 0) _exit(107);
}
__attribute__((constructor)) static void prepare(void) {
    const char *path = getenv("PC_SAMPLE_PATH");
    if (!path || !*path) return;
    raw_fd=open(path, O_APPEND|O_CREAT|O_WRONLY|O_CLOEXEC,0600);
    if(raw_fd<0) _exit(104);
    struct sigaction a={0};
    a.sa_sigaction=sample; a.sa_flags=SA_SIGINFO|SA_RESTART;
    sigemptyset(&a.sa_mask);
    if(sigaction(SIGPROF,&a,NULL)) _exit(105);
    arm();
}
struct worker { void *(*fn)(void *); void *arg; };
static void *enter(void *p) {
    struct worker h=*(struct worker *)p; free(p);
    arm(); return h.fn(h.arg);
}
extern int __real_pthread_create(pthread_t *,const pthread_attr_t *,void *(*)(void *),void *);
int __wrap_pthread_create(pthread_t *th,const pthread_attr_t *a,void *(*fn)(void*),void *arg) {
    if(raw_fd<0) return __real_pthread_create(th,a,fn,arg);
    struct worker *h=malloc(sizeof(*h));
    if(!h) return ENOMEM;
    h->fn=fn; h->arg=arg;
    int ret=__real_pthread_create(th,a,enter,h);
    if(ret) free(h);
    return ret;
}