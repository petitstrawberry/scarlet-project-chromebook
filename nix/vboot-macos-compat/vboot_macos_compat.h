#ifndef SCARLET_VBOOT_MACOS_COMPAT_H
#define SCARLET_VBOOT_MACOS_COMPAT_H

#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <sys/ioctl.h>

#include "endian.h"

#ifndef O_LARGEFILE
#define O_LARGEFILE 0
#endif

#ifndef NAME_MAX
#define NAME_MAX 255
#endif

/*
 * gpio_uapi.c is linked into upstream's general host utility archive even
 * when Linux GPIO support is unavailable. Its public functions already fail
 * closed without line ioctls; these definitions only make the unused chip
 * discovery path compile on Darwin.
 */
struct gpiochip_info {
    char name[32];
    char label[32];
    uint32_t lines;
};

#ifndef GPIO_GET_CHIPINFO_IOCTL
#define GPIO_GET_CHIPINFO_IOCTL _IOR(0xb4, 0x01, struct gpiochip_info)
#endif

#endif
