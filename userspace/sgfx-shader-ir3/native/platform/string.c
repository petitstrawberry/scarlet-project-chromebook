// SPDX-License-Identifier: MIT
// A genuine libc extension used by Mesa VTN's symbol mangler. The installed
// Scarlet C runtime provides strcpy but not stpcpy. This private compiler
// closure supplies the standard copy-and-return-the-final-NUL operation.
char *stpcpy(char *restrict destination, const char *restrict source);
__attribute__((visibility("hidden")))
char *stpcpy(char *restrict destination, const char *restrict source) {
    while ((*destination = *source) != '\0') {
        ++destination;
        ++source;
    }
    return destination;
}

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>
/* Real POSIX line reader for Mesa's optional assembly round-trip diagnostic. */
ssize_t getline(char **line, size_t *capacity, FILE *stream);
__attribute__((visibility("hidden")))
ssize_t getline(char **line, size_t *capacity, FILE *stream) {
    if (!line || !capacity || !stream) { errno=EINVAL; return -1; }
    size_t used=0;
    for (;;) {
        int ch=fgetc(stream);
        if(ch==EOF) { if(!used || ferror(stream)) return -1; break; }
        if (!*line || used+1>=*capacity) {
            size_t size=(*line && *capacity) ? *capacity : 128;
            if(size>SIZE_MAX/2) { errno=ENOMEM; return -1; }
            size*=2;
            char *buffer=realloc(*line,size);
            if(!buffer) { errno=ENOMEM; return -1; }
            *line=buffer; *capacity=size;
        }
        (*line)[used++]=(char)ch;
        if(ch=='\n') break;
    }
    (*line)[used]=0;
    return (ssize_t)used;
}
