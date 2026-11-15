// SPDX-License-Identifier: MIT
// Explicit compiler-only additions missing from the Scarlet SDK headers.
// These are real stack/string operations and the standard log2(e) constant.
#pragma once
#include <alloca.h>
#include <strings.h>
#include <stdint.h>
#ifndef M_LOG2E
#define M_LOG2E 1.4426950408889634074
#endif
#ifndef M_1_PI
#define M_1_PI 0.31830988618379067154
#endif
// Save/restore the AArch64 floating-point environment used during NIR constant
// evaluation. FPCR exception enables [12:8] plus IDE [15], FPSR sticky flags
// [4:0] plus input-denormal IDC [7].
// The operation preserves the caller's rounding mode and all unrelated bits.
struct scarlet_ir3_fenv { uint64_t control,status; };
static inline int scarlet_ir3_feholdexcept(struct scarlet_ir3_fenv *env) {
    __asm__ volatile("mrs %0, fpcr" : "=r"(env->control));
    __asm__ volatile("mrs %0, fpsr" : "=r"(env->status));
    uint64_t control=env->control & ~UINT64_C(0x9f00);
    uint64_t status=env->status & ~UINT64_C(0x9f);
    __asm__ volatile("msr fpcr, %0" :: "r"(control) : "memory");
    __asm__ volatile("msr fpsr, %0" :: "r"(status) : "memory");
    return 0;
}
static inline int scarlet_ir3_fesetenv(const struct scarlet_ir3_fenv *env) {
    __asm__ volatile("msr fpcr, %0" :: "r"(env->control) : "memory");
    __asm__ volatile("msr fpsr, %0" :: "r"(env->status) : "memory");
    return 0;
}

#include <stdio.h>
#include <sys/types.h>
ssize_t getline(char **line, size_t *capacity, FILE *stream);

#include "printf.h"
