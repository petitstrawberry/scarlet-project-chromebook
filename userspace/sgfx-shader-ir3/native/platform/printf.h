// SPDX-License-Identifier: MIT
#pragma once
#include <stdarg.h>
#include <stdio.h>
#ifdef __cplusplus
extern "C" {
#endif
int scarlet_ir3_vsnprintf(char *, size_t, const char *, va_list);
int scarlet_ir3_snprintf(char *, size_t, const char *, ...);
int scarlet_ir3_vsprintf(char *, const char *, va_list);
int scarlet_ir3_sprintf(char *, const char *, ...);
int scarlet_ir3_vasprintf(char **, const char *, va_list);
int scarlet_ir3_asprintf(char **, const char *, ...);
int scarlet_ir3_vfprintf(FILE *, const char *, va_list);
int scarlet_ir3_fprintf(FILE *, const char *, ...);
int scarlet_ir3_vprintf(const char *, va_list);
int scarlet_ir3_printf(const char *, ...);
int scarlet_ir3_format_check(void);
#ifdef __cplusplus
}
#endif
#ifndef SCARLET_IR3_PRINTF_IMPLEMENTATION
#define vsnprintf scarlet_ir3_vsnprintf
#define snprintf scarlet_ir3_snprintf
#define vsprintf scarlet_ir3_vsprintf
#define sprintf scarlet_ir3_sprintf
#define vasprintf scarlet_ir3_vasprintf
#define asprintf scarlet_ir3_asprintf
#define vfprintf scarlet_ir3_vfprintf
#define fprintf scarlet_ir3_fprintf
#define vprintf scarlet_ir3_vprintf
#define printf scarlet_ir3_printf
#endif
