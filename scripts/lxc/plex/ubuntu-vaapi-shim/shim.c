/*
 * C23 glibc symbol shim for Plex on Ubuntu 24.04.
 *
 * Ubuntu 24.04's intel-media-va-driver-non-free 24.1.0 is compiled against
 * glibc 2.39 and references C23 symbols (__isoc23_*). Plex's Transcoder is a
 * musl ELF that cannot resolve these via gcompat. This shim bridges the gap.
 *
 * Build:
 *   gcc -shared -fPIC -nostdlib -nodefaultlibs -o plex-vaapi-shim.so shim.c
 *
 * Deploy:
 *   cp plex-vaapi-shim.so /usr/local/lib/
 *   # then write the systemd drop-in (see install.sh)
 *
 * Not needed on Debian 12 — use the standard provision.sh instead.
 */

#include <stdarg.h>
#include <stdio.h>

extern unsigned long strtoul(const char *, char **, int);
extern long strtol(const char *, char **, int);
extern unsigned long long strtoull(const char *, char **, int);
extern long long strtoll(const char *, char **, int);

/* Force base symbol names — avoids glibc renaming to __isoc99_v*scanf */
extern int _vfscanf(FILE *, const char *, va_list) __asm__("vfscanf");
extern int _vsscanf(const char *, const char *, va_list) __asm__("vsscanf");

unsigned long __isoc23_strtoul(const char *s, char **e, int b) { return strtoul(s, e, b); }
long __isoc23_strtol(const char *s, char **e, int b) { return strtol(s, e, b); }
unsigned long long __isoc23_strtoull(const char *s, char **e, int b) { return strtoull(s, e, b); }
long long __isoc23_strtoll(const char *s, char **e, int b) { return strtoll(s, e, b); }

int __isoc23_fscanf(FILE *f, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = _vfscanf(f, fmt, ap); va_end(ap); return r;
}

int __isoc23_sscanf(const char *s, const char *fmt, ...) {
    va_list ap; va_start(ap, fmt);
    int r = _vsscanf(s, fmt, ap); va_end(ap); return r;
}
