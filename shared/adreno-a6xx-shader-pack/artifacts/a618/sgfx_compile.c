/* SPDX-License-Identifier: GPL-2.0-only */
/* Deterministic host-only SGFX shader producer for pinned Mesa IR3. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "compiler/glsl_types.h"
#include "compiler/nir/nir_builder.h"
#include "freedreno/common/disasm.h"
#include "freedreno/common/freedreno_dev_info.h"
#include "freedreno/ir3/ir3_compiler.h"
#include "freedreno/ir3/ir3_nir.h"
#include "freedreno/ir3/ir3_shader.h"

enum fs_kind { FS_SOLID, FS_VERTEX_COLOR, FS_TEXTURE_RGBA, FS_TEXTURE_ALPHA_MASK,
               FS_TEXTURE_VERTEX_COLOR_RGBA, FS_TEXTURE_RGB_IGNORE_ALPHA };

struct compiled {
   const char *name;
   struct ir3_shader *shader;
   struct ir3_shader_variant *variant;
};

static nir_def *
load_const(nir_builder *b, unsigned base, unsigned components)
{
   return nir_load_const_ir3(b, components, 32, nir_imm_int(b, 0), .base = base);
}

static nir_def *
matrix_transform(nir_builder *b, nir_def *position)
{
   nir_def *c0 = load_const(b, 0, 4);
   nir_def *c1 = load_const(b, 4, 4);
   nir_def *c2 = load_const(b, 8, 4);
   nir_def *c3 = load_const(b, 12, 4);
   nir_def *r = nir_fmul(b, c0, nir_channel(b, position, 0));
   r = nir_fadd(b, nir_fmul(b, c1, nir_channel(b, position, 1)), r);
   r = nir_fadd(b, nir_fmul(b, c2, nir_channel(b, position, 2)), r);
   return nir_fadd(b, nir_fmul(b, c3, nir_channel(b, position, 3)), r);
}

static nir_variable *
io_var(nir_builder *b, nir_variable_mode mode, unsigned location,
       const struct glsl_type *type)
{
   return nir_create_variable_with_location(b->shader, mode, location, type);
}

static nir_shader *
build_vs(const char *name, bool position_is_vec2, bool has_color, bool color_is_vec3,
         bool has_uv)
{
   nir_builder nb = nir_builder_init_simple_shader(MESA_SHADER_VERTEX, NULL, "%s", name);
   nir_builder *b = &nb;
   nir_variable *in_pos = io_var(b, nir_var_shader_in, VERT_ATTRIB_GENERIC0,
                                 position_is_vec2 ? glsl_vec2_type() : glsl_vec4_type());
   nir_def *p = nir_load_var(b, in_pos);
   if (position_is_vec2)
      p = nir_vec4(b, nir_channel(b, p, 0), nir_channel(b, p, 1),
                   nir_imm_float(b, 0.0f), nir_imm_float(b, 1.0f));
   nir_variable *out_pos = io_var(b, nir_var_shader_out, VARYING_SLOT_POS,
                                  glsl_vec4_type());
   nir_store_var(b, out_pos, matrix_transform(b, p), 0xf);

   if (has_color) {
      nir_variable *in = io_var(b, nir_var_shader_in, VERT_ATTRIB_GENERIC1,
                                color_is_vec3 ? glsl_vec_type(3) : glsl_vec4_type());
      nir_variable *out = io_var(b, nir_var_shader_out, VARYING_SLOT_VAR0,
                                 glsl_vec4_type());
      nir_def *c = nir_load_var(b, in);
      if (color_is_vec3)
         c = nir_vec4(b, nir_channel(b, c, 0), nir_channel(b, c, 1),
                      nir_channel(b, c, 2), nir_imm_float(b, 1.0f));
      nir_store_var(b, out, c, 0xf);
   }
   if (has_uv) {
      unsigned attr = has_color ? VERT_ATTRIB_GENERIC2 : VERT_ATTRIB_GENERIC1;
      unsigned slot = has_color ? VARYING_SLOT_VAR1 : VARYING_SLOT_VAR0;
      nir_variable *in = io_var(b, nir_var_shader_in, attr, glsl_vec2_type());
      nir_variable *out = io_var(b, nir_var_shader_out, slot, glsl_vec2_type());
      nir_store_var(b, out, nir_load_var(b, in), 0x3);
   }
   return b->shader;
}

static nir_def *
sample_texture(nir_builder *b, nir_def *uv)
{
   b->shader->info.num_textures = 1;
   BITSET_SET(b->shader->info.textures_used, 0);
   BITSET_SET(b->shader->info.samplers_used, 0);
   return nir_tex(b, uv, .texture_index = 0, .sampler_index = 0,
                  .dim = GLSL_SAMPLER_DIM_2D, .dest_type = nir_type_float32);
}

static nir_shader *
build_fs(const char *name, enum fs_kind kind)
{
   nir_builder nb = nir_builder_init_simple_shader(MESA_SHADER_FRAGMENT, NULL, "%s", name);
   nir_builder *b = &nb;
   nir_def *color = load_const(b, 16, 4);
   nir_def *vertex_color = NULL, *uv = NULL, *sample = NULL, *result = NULL;
   if (kind == FS_VERTEX_COLOR || kind == FS_TEXTURE_VERTEX_COLOR_RGBA) {
      nir_variable *in = io_var(b, nir_var_shader_in, VARYING_SLOT_VAR0,
                                glsl_vec4_type());
      vertex_color = nir_load_var(b, in);
   }
   if (kind == FS_TEXTURE_RGBA || kind == FS_TEXTURE_ALPHA_MASK ||
       kind == FS_TEXTURE_RGB_IGNORE_ALPHA) {
      nir_variable *in = io_var(b, nir_var_shader_in, VARYING_SLOT_VAR0,
                                glsl_vec2_type());
      uv = nir_load_var(b, in);
   } else if (kind == FS_TEXTURE_VERTEX_COLOR_RGBA) {
      nir_variable *in = io_var(b, nir_var_shader_in, VARYING_SLOT_VAR1,
                                glsl_vec2_type());
      uv = nir_load_var(b, in);
   }
   if (uv)
      sample = sample_texture(b, uv);

   switch (kind) {
   case FS_SOLID:
      result = color;
      break;
   case FS_VERTEX_COLOR:
      result = nir_fmul(b, vertex_color, color);
      break;
   case FS_TEXTURE_RGBA:
      result = nir_fmul(b, sample, color);
      break;
   case FS_TEXTURE_ALPHA_MASK:
      result = nir_vec4(b, nir_channel(b, color, 0), nir_channel(b, color, 1),
                        nir_channel(b, color, 2),
                        nir_fmul(b, nir_channel(b, sample, 3),
                                 nir_channel(b, color, 3)));
      break;
   case FS_TEXTURE_VERTEX_COLOR_RGBA:
      result = nir_fmul(b, nir_fmul(b, sample, vertex_color), color);
      break;
   case FS_TEXTURE_RGB_IGNORE_ALPHA:
      result = nir_fmul(b,
                        nir_vec4(b, nir_channel(b, sample, 0),
                                 nir_channel(b, sample, 1),
                                 nir_channel(b, sample, 2),
                                 nir_imm_float(b, 1.0f)),
                        color);
      break;
   }
   nir_variable *out = io_var(b, nir_var_shader_out, FRAG_RESULT_DATA0,
                              glsl_vec4_type());
   nir_store_var(b, out, result, 0xf);
   return b->shader;
}

static struct compiled
compile_one(struct ir3_compiler *compiler, const char *name, nir_shader *nir)
{
   nir->options = ir3_get_compiler_options(compiler);
   nir_assign_io_var_locations(nir, nir_var_shader_in);
   nir_assign_io_var_locations(nir, nir_var_shader_out);
   struct ir3_const_allocations const_allocs = {};
   ir3_const_alloc(&const_allocs, IR3_CONST_ALLOC_UBO_RANGES, 5, 1);
   const struct ir3_shader_options options = {
      .api_wavesize = IR3_SINGLE_OR_DOUBLE,
      .real_wavesize = IR3_SINGLE_OR_DOUBLE,
      .const_allocs = const_allocs,
   };
   ir3_finalize_nir(compiler, &options.nir_options, nir);
   ir3_nir_lower_io(nir);
   struct ir3_shader *shader = ir3_shader_from_nir(compiler, nir, &options);
   struct ir3_shader_key key = {};
   struct ir3_shader_variant *v = ir3_shader_get_variant(shader, &key, false,
                                                         false, NULL, NULL);
   if (!v) {
      fprintf(stderr, "failed to compile %s\n", name);
      exit(2);
   }
   return (struct compiled){ name, shader, v };
}

static void
write_binary(const char *dir, const struct compiled *c)
{
   char path[1024];
   snprintf(path, sizeof(path), "%s/%s.bin", dir, c->name);
   FILE *f = fopen(path, "wb");
   if (!f || fwrite(c->variant->bin, 4, c->variant->info.sizedwords, f) !=
             c->variant->info.sizedwords || fclose(f)) {
      perror(path);
      exit(2);
   }
   snprintf(path, sizeof(path), "%s/%s.disasm", dir, c->name);
   f = fopen(path, "w");
   if (!f) { perror(path); exit(2); }
   struct shader_stats stats = {};
   disasm_a3xx_stat(c->variant->bin, c->variant->info.sizedwords, PRINT_RAW,
                    f, 618, &stats);
   fclose(f);
}

static void
print_variant_json(FILE *f, const struct compiled *c, bool last)
{
   const struct ir3_shader_variant *v = c->variant;
   struct shader_stats stats = {};
   FILE *sink = fopen("/dev/null", "w");
   disasm_a3xx_stat(v->bin, v->info.sizedwords, 0, sink, 618, &stats);
   fclose(sink);
   fprintf(f, "    {\"name\":\"%s\",\"stage\":\"%s\",", c->name,
           v->type == MESA_SHADER_VERTEX ? "vertex" : "fragment");
   fprintf(f, "\"binary_bytes\":%u,\"sizedwords\":%u,\"instrlen\":%u,",
           v->info.sizedwords * 4, v->info.sizedwords, v->instrlen);
   fprintf(f, "\"constlen\":%u,\"max_reg\":%d,\"max_half_reg\":%d,",
           v->constlen, v->info.max_reg, v->info.max_half_reg);
   fprintf(f, "\"const_alloc_max_vec4\":%u,\"imm_count_dwords\":%u,\"imm_values\":[",
           ir3_const_state(v)->allocs.max_const_offset_vec4, v->imm_state.count);
   for (unsigned i = 0; i < v->imm_state.count; i++) {
      if (i) fputc(',', f);
      fprintf(f, "%u", v->imm_state.values[i]);
   }
   fprintf(f, "],");
   fprintf(f, "\"halfregs_footprint\":%u,\"branchstack\":%u,\"branchstack_hw\":%u,",
           ir3_shader_halfregs(v), v->branchstack, ir3_shader_branchstack_hw(v));
   fprintf(f, "\"mergedregs\":%s,\"early_preamble\":%s,"
              "\"double_threadsize\":%s,\"threadsize\":%u,"
              "\"need_full_quad\":%s,\"need_pixlod\":%s,"
              "\"prefetch_end_of_quad\":%s,\"dual_src_blend\":%s,"
              "\"color0_mrt\":%s,\"multi_pos_output\":%s,"
              "\"clip_mask\":%u,\"cull_mask\":%u,",
           v->mergedregs ? "true" : "false",
           v->early_preamble ? "true" : "false",
           v->info.double_threadsize ? "true" : "false",
           v->info.double_threadsize ? 128 : 64,
           v->need_full_quad ? "true" : "false",
           v->need_pixlod ? "true" : "false",
           v->prefetch_end_of_quad ? "true" : "false",
           v->dual_src_blend ? "true" : "false",
           v->color0_mrt ? "true" : "false",
           v->multi_pos_output ? "true" : "false",
           v->clip_mask, v->cull_mask);
   fprintf(f, "\"pvtmem_size\":%u,\"num_samp\":%d,\"has_tex\":%u,\"has_samp\":%u,"
              "\"num_sampler_prefetch\":%u,\"sampler_prefetch\":[",
           v->pvtmem_size, v->num_samp, stats.has_tex || v->num_sampler_prefetch,
           stats.has_samp || v->num_sampler_prefetch, v->num_sampler_prefetch);
   for (unsigned i = 0; i < v->num_sampler_prefetch; i++) {
      const struct ir3_sampler_prefetch *p = &v->sampler_prefetch[i];
      if (i) fputc(',', f);
      fprintf(f, "{\"src\":%u,\"samp_id\":%u,\"tex_id\":%u,\"dst\":%u,"
                 "\"wrmask\":%u,\"half\":%u,\"bindless\":%s,\"tex_opc\":%u}",
              p->src, p->samp_id, p->tex_id, p->dst, p->wrmask,
              p->half_precision, p->bindless ? "true" : "false", p->tex_opc);
   }
   fprintf(f, "],");
   fprintf(f, "\"inputs_count\":%u,\"inputs\":[", v->inputs_count);
   for (unsigned i = 0; i < v->inputs_count; i++) {
      if (i) fputc(',', f);
      fprintf(f, "{\"slot\":%u,\"regid\":%u,\"compmask\":%u,\"inloc\":%u,"
                 "\"sysval\":%s,\"bary\":%s,\"half\":%s}",
              v->inputs[i].slot, v->inputs[i].regid, v->inputs[i].compmask,
              v->inputs[i].inloc, v->inputs[i].sysval ? "true" : "false",
              v->inputs[i].bary ? "true" : "false",
              v->inputs[i].half ? "true" : "false");
   }
   fprintf(f, "],\"outputs_count\":%u,\"outputs\":[", v->outputs_count);
   for (unsigned i = 0; i < v->outputs_count; i++) {
      if (i) fputc(',', f);
      fprintf(f, "{\"slot\":%u,\"regid\":%u,\"view\":%u,"
                 "\"aliased_components\":%u,\"half\":%s}",
              v->outputs[i].slot, v->outputs[i].regid,
              v->outputs[i].view, v->outputs[i].aliased_components,
              v->outputs[i].half ? "true" : "false");
   }
   fprintf(f, "],\"total_in\":%u,\"sysval_in\":%u,\"varying_in\":%u,"
              "\"attr_in\":%u,\"writes_pos\":%s,\"writes_smask\":%s,"
              "\"writes_psize\":%s,\"writes_viewport\":%s,"
              "\"writes_stencilref\":%s}%s\n",
           v->total_in, v->sysval_in, v->varying_in, v->attr_in,
           v->writes_pos ? "true" : "false",
           v->writes_smask ? "true" : "false",
           v->writes_psize ? "true" : "false",
           v->writes_viewport ? "true" : "false",
           v->writes_stencilref ? "true" : "false", last ? "" : ",");
}

static void
print_link_json(FILE *f, const char *name, const struct compiled *vs,
                const struct compiled *fs, bool last)
{
   struct ir3_shader_linkage l = {};
   ir3_link_shaders(&l, vs->variant, fs->variant, false);
   fprintf(f, "    {\"name\":\"%s\",\"vs\":\"%s\",\"fs\":\"%s\","
              "\"max_loc\":%u,\"varmask\":[%u,%u,%u,%u],\"vars\":[",
           name, vs->name, fs->name, l.max_loc,
           l.varmask[0], l.varmask[1], l.varmask[2], l.varmask[3]);
   for (unsigned i = 0; i < l.cnt; i++) {
      if (i) fputc(',', f);
      fprintf(f, "{\"slot\":%u,\"regid\":%u,\"compmask\":%u,\"loc\":%u}",
              l.var[i].slot, l.var[i].regid, l.var[i].compmask, l.var[i].loc);
   }
   fprintf(f, "]}%s\n", last ? "" : ",");
}

int
main(int argc, char **argv)
{
   if (argc != 2) { fprintf(stderr, "usage: %s OUTPUT_DIR\n", argv[0]); return 2; }
   mkdir(argv[1], 0755);
   glsl_type_singleton_init_or_ref();
   static const struct fd_dev_id dev_id = { .gpu_id = 618 };
   struct ir3_compiler *compiler = ir3_compiler_create(
      NULL, &dev_id, fd_dev_info_raw(&dev_id),
      &(struct ir3_compiler_options){ .disable_cache = true });
   if (!compiler) return 2;

   struct compiled v[13]; unsigned n = 0;
   v[n++] = compile_one(compiler, "vs_stride16_pos2", build_vs("vs_stride16_pos2", true, false, false, false));
   v[n++] = compile_one(compiler, "vs_stride16_pos2_uv2", build_vs("vs_stride16_pos2_uv2", true, false, false, true));
   v[n++] = compile_one(compiler, "vs_stride40_pos4", build_vs("vs_stride40_pos4", false, false, false, false));
   v[n++] = compile_one(compiler, "vs_stride40_pos4_color4", build_vs("vs_stride40_pos4_color4", false, true, false, false));
   v[n++] = compile_one(compiler, "vs_stride40_pos4_color4_uv2", build_vs("vs_stride40_pos4_color4_uv2", false, true, false, true));
   v[n++] = compile_one(compiler, "fs_solid", build_fs("fs_solid", FS_SOLID));
   v[n++] = compile_one(compiler, "fs_vertex_color", build_fs("fs_vertex_color", FS_VERTEX_COLOR));
   v[n++] = compile_one(compiler, "fs_texture_rgba", build_fs("fs_texture_rgba", FS_TEXTURE_RGBA));
   v[n++] = compile_one(compiler, "fs_texture_alpha_mask", build_fs("fs_texture_alpha_mask", FS_TEXTURE_ALPHA_MASK));
   v[n++] = compile_one(compiler, "fs_texture_vertex_color_rgba", build_fs("fs_texture_vertex_color_rgba", FS_TEXTURE_VERTEX_COLOR_RGBA));
   v[n++] = compile_one(compiler, "vs_stride24_pos4_uv2", build_vs("vs_stride24_pos4_uv2", false, false, false, true));
   v[n++] = compile_one(compiler, "vs_stride28_pos4_color3", build_vs("vs_stride28_pos4_color3", false, true, true, false));
   v[n++] = compile_one(compiler, "fs_texture_rgb_ignore_alpha", build_fs("fs_texture_rgb_ignore_alpha", FS_TEXTURE_RGB_IGNORE_ALPHA));
   for (unsigned i = 0; i < n; i++) write_binary(argv[1], &v[i]);

   char path[1024]; snprintf(path, sizeof(path), "%s/mesa-metadata.json", argv[1]);
   FILE *f = fopen(path, "w"); if (!f) { perror(path); return 2; }
   fprintf(f, "{\n  \"schema_version\":2,\n"
              "  \"mesa_sha\":\"3f1b217baffffa00cb8f53e158713a33e1bd4632\",\n"
              "  \"gpu_id\":618,\n  \"variants\":[\n");
   for (unsigned i = 0; i < n; i++) print_variant_json(f, &v[i], i + 1 == n);
   fprintf(f, "  ],\n  \"links\":[\n");
   print_link_json(f, "stride16_solid", &v[0], &v[5], false);
   print_link_json(f, "stride16_texture_rgba", &v[1], &v[7], false);
   print_link_json(f, "stride16_texture_alpha_mask", &v[1], &v[8], false);
   print_link_json(f, "stride40_solid", &v[2], &v[5], false);
   print_link_json(f, "stride40_vertex_color", &v[3], &v[6], false);
   print_link_json(f, "stride40_texture_vertex_color_rgba", &v[4], &v[9], false);
   print_link_json(f, "stride24_solid", &v[2], &v[5], false);
   print_link_json(f, "stride24_texture_rgba", &v[10], &v[7], false);
   print_link_json(f, "stride24_texture_rgb_ignore_alpha", &v[10], &v[12], false);
   print_link_json(f, "stride28_vertex_color", &v[11], &v[6], true);
   fprintf(f, "  ]\n}\n"); fclose(f);
   for (unsigned i = 0; i < n; i++) ir3_shader_destroy(v[i].shader);
   ir3_compiler_destroy(compiler);
   glsl_type_singleton_decref();
   return 0;
}
