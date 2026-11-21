#define _GNU_SOURCE
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <fcntl.h>
#include <time.h>
int main(int argc,char **argv) {
    if (argc!=2) return 2;
    int rounds=atoi(argv[1]);
    if (rounds<1 || rounds>40) return 2;
    int ready=open("/scratch/ready",O_WRONLY|O_CREAT|O_EXCL,0644);
    if(ready<0) return 3;
    close(ready);
    for (int j=0;access("/scratch/go",F_OK) && j<12000;j++) usleep(10000);
    if(access("/scratch/go",F_OK)) return 4;
    for(int i=0;i<rounds;i++) {
        char out[64]; snprintf(out,sizeof(out),"/output/%d",i);
        if(mkdir(out,0755)||setenv("OUTPUT_DIR",out,1)) return 5;
        pid_t child=fork();
        if(child<0) return 6;
        if(!child) { execl("/sampled","/sampled",(char*)NULL); _exit(112); }
        int status=0;
        if(waitpid(child,&status,0)!=child || !WIFEXITED(status) || WEXITSTATUS(status)) return 7;
    }
    return 0;
}