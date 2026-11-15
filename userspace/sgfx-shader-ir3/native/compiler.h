/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef SCARLET_IR3_COMPILER_H
#define SCARLET_IR3_COMPILER_H
#include <stddef.h>
#include <stdint.h>
/* Version 1. All sizes are bytes except SPIR-V word_count. stage: 0 VS, 1 FS,
 * 2 CS. Callers serialize calls (Mesa's type/debug state is process-global).
 * SPIR-V must first pass Naga validation and this crate's resource profile.
 * Result owns every pointer; release it even on failure. No pointers into Mesa
 * objects escape. No device access, shader cache files, or fallback ISA.
 */
struct scarlet_ir3_result {
    uint8_t *allocation;
    size_t allocation_size;
    char *metadata;
    size_t metadata_size;
    char *error;
};
int scarlet_ir3_compile(const uint32_t *words, size_t word_count,
    uint32_t stage, const char *entry, uint32_t push_constant_bytes,
    struct scarlet_ir3_result *out);
/* Explicit profile entry. FS requires rasterization_samples==1; VS/CS require
 * 0. Other counts are rejected, never treated as supported MSAA. The original
 * entry above remains an ABI-compatible wrapper selecting the same profile. */
int scarlet_ir3_compile_with_samples(const uint32_t *words, size_t word_count,
    uint32_t stage, const char *entry, uint32_t push_constant_bytes,
    uint32_t rasterization_samples, struct scarlet_ir3_result *out);
/* Profile values match the
 * public YCbCr record; all five words participate in specialization identity.
 * count <=16, strictly increasing texture slots <16. No pointer is retained. */
struct scarlet_ir3_nv12_profile {
    uint32_t texture_slot, matrix, range, chroma_x, chroma_y;
};
int scarlet_ir3_compile_with_nv12(const uint32_t *words, size_t word_count,
    uint32_t stage, const char *entry, uint32_t push_constant_bytes,
    uint32_t rasterization_samples,
    const struct scarlet_ir3_nv12_profile *profiles, uint32_t profile_count,
    struct scarlet_ir3_result *out);
/* Exact sampler state joins the NV12 specialization/cache identity. Address
 * codes are Repeat=0, Edge=1, Mirror=2, Border=3; filters Nearest=0, Linear=1.
 * The existing five-word texture profile and result ABI stay unchanged. */
struct scarlet_ir3_nv12_sampler {
    uint32_t slot, address_u, address_v, min_filter, mag_filter;
};
int scarlet_ir3_compile_with_nv12_samplers(const uint32_t *words, size_t word_count,
    uint32_t stage, const char *entry, uint32_t push_constant_bytes,
    uint32_t rasterization_samples,
    const struct scarlet_ir3_nv12_profile *profiles, uint32_t profile_count,
    const struct scarlet_ir3_nv12_sampler *samplers, uint32_t sampler_count,
    struct scarlet_ir3_result *out);
void scarlet_ir3_result_free(struct scarlet_ir3_result *result);
#endif
