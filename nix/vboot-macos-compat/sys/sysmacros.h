#ifndef SCARLET_VBOOT_MACOS_SYSMACROS_H
#define SCARLET_VBOOT_MACOS_SYSMACROS_H

#include <sys/types.h>

/* Linux accepts this open(2) flag; Darwin's off_t is already 64-bit. */
#ifndef O_LARGEFILE
#define O_LARGEFILE 0
#endif

#endif
