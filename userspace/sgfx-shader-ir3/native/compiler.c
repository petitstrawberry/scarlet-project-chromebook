/* SPDX-License-Identifier: GPL-2.0-only */
/* Runtime compiler for pinned Mesa IR3; see compiler.h for the C ABI. */
#include "compiler.h"
#include <stdarg.h>
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

#include "compiler/spirv/nir_spirv.h"
#include "util/ralloc.h"
#include "nv12_lowering.h"
/* Bounded in-memory JSON writer: no temporary file or /dev/null dependency. */
struct json { char *data; size_t size, capacity; bool failed; };
static void json_printf(struct json *j, const char *fmt, ...) {
   if (j->failed) return;
   va_list args, copy; va_start(args, fmt); va_copy(copy, args);
   int n = vsnprintf(NULL, 0, fmt, copy); va_end(copy);
   if (n < 0 || (size_t)n > 1024*1024 - j->size) { j->failed=true; va_end(args); return; }
   size_t needed = j->size + n + 1;
   if (needed > j->capacity) {
      size_t cap = needed * 2;
      char *p = realloc(j->data, cap);
      if (!p) { j->failed=true; va_end(args); return; }
      j->data=p; j->capacity=cap;
   }
   vsnprintf(j->data+j->size, j->capacity-j->size, fmt, args);
   va_end(args); j->size += n;
}
static void json_char(int ch, struct json *j) { json_printf(j, "%c", ch); }
struct compiled { const char *name; struct ir3_shader *shader; struct ir3_shader_variant *variant; uint32_t rasterization_samples; const struct nv12_lowering_state *nv12; };
static void
print_variant_json(struct json *f, const struct compiled *c, bool last)
{
   const struct ir3_shader_variant *v = c->variant;
   json_printf(f, "    {\"name\":\"%s\",\"stage\":\"%s\",", c->name,
           v->type == MESA_SHADER_VERTEX ? "vertex" : v->type == MESA_SHADER_FRAGMENT ? "fragment" : "compute");
   json_printf(f, "\"binary_bytes\":%u,\"sizedwords\":%u,\"instrlen\":%u,",
           v->info.sizedwords * 4, v->info.sizedwords, v->instrlen);
   json_printf(f, "\"rasterization_samples\":%u,", c->rasterization_samples);
   json_printf(f, "\"constlen\":%u,\"max_reg\":%d,\"max_half_reg\":%d,",
           v->constlen, v->info.max_reg, v->info.max_half_reg);
   json_printf(f, "\"const_alloc_max_vec4\":%u,\"imm_count_dwords\":%u,\"imm_values\":[",
           ir3_const_state(v)->allocs.max_const_offset_vec4, v->imm_state.count);
   for (unsigned i = 0; i < v->imm_state.count; i++) {
      if (i) json_char(',', f);
      json_printf(f, "%u", v->imm_state.values[i]);
   }
   json_printf(f, "],");
   json_printf(f, "\"halfregs_footprint\":%u,\"branchstack\":%u,\"branchstack_hw\":%u,",
           ir3_shader_halfregs(v), v->branchstack, ir3_shader_branchstack_hw(v));
   json_printf(f, "\"mergedregs\":%s,\"early_preamble\":%s,"
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
   json_printf(f, "\"pvtmem_size\":%u,\"num_samp\":%d,\"has_tex\":%u,\"has_samp\":%u,"
              "\"num_sampler_prefetch\":%u,\"sampler_prefetch\":[",
           v->pvtmem_size, v->num_samp, v->num_samp > 0 || v->num_sampler_prefetch,
           BITSET_LAST_BIT(c->shader->nir->info.samplers_used) > 0 || v->num_sampler_prefetch, v->num_sampler_prefetch);
   for (unsigned i = 0; i < v->num_sampler_prefetch; i++) {
      const struct ir3_sampler_prefetch *p = &v->sampler_prefetch[i];
      if (i) json_char(',', f);
      json_printf(f, "{\"src\":%u,\"samp_id\":%u,\"tex_id\":%u,\"dst\":%u,"
                 "\"wrmask\":%u,\"half\":%u,\"bindless\":%s,\"tex_opc\":%u}",
              p->src, p->samp_id, p->tex_id, p->dst, p->wrmask,
              p->half_precision, p->bindless ? "true" : "false", p->tex_opc);
   }
   json_printf(f, "],");
   json_printf(f, "\"inputs_count\":%u,\"inputs\":[", v->inputs_count);
   for (unsigned i = 0; i < v->inputs_count; i++) {
      if (i) json_char(',', f);
      json_printf(f, "{\"slot\":%u,\"regid\":%u,\"compmask\":%u,\"inloc\":%u,"
                 "\"sysval\":%s,\"bary\":%s,\"half\":%s,\"flat\":%s,\"rasterflat\":%s}",
              v->inputs[i].slot, v->inputs[i].regid, v->inputs[i].compmask,
              v->inputs[i].inloc, v->inputs[i].sysval ? "true" : "false",
              v->inputs[i].bary ? "true" : "false",
              v->inputs[i].half ? "true" : "false", v->inputs[i].flat ? "true" : "false", v->inputs[i].rasterflat ? "true" : "false");
   }
   json_printf(f, "],\"outputs_count\":%u,\"outputs\":[", v->outputs_count);
   for (unsigned i = 0; i < v->outputs_count; i++) {
      if (i) json_char(',', f);
      json_printf(f, "{\"slot\":%u,\"regid\":%u,\"view\":%u,"
                 "\"aliased_components\":%u,\"half\":%s}",
              v->outputs[i].slot, v->outputs[i].regid,
              v->outputs[i].view, v->outputs[i].aliased_components,
              v->outputs[i].half ? "true" : "false");
   }
   json_printf(f, "],\"total_in\":%u,\"sysval_in\":%u,\"varying_in\":%u,"
              "\"attr_in\":%u,\"writes_pos\":%s,\"writes_smask\":%s,"
              "\"writes_psize\":%s,\"writes_viewport\":%s,"
              "\"writes_stencilref\":%s,",
           v->total_in, v->sysval_in, v->varying_in, v->attr_in,
           v->writes_pos ? "true" : "false",
           v->writes_smask ? "true" : "false",
           v->writes_psize ? "true" : "false",
           v->writes_viewport ? "true" : "false",
           v->writes_stencilref ? "true" : "false");
   const struct ir3_const_state *cs = ir3_const_state(v);
   json_printf(f, "\"abi_version\":1,\"gpu_id\":618,\"allocation_bytes\":%u,"
                  "\"constant_data_offset\":%u,\"constant_data_bytes\":%u,"
                  "\"immediate_offset_dwords\":%u,\"num_ubos\":%u,\"num_app_ubos\":%u,"
                  "\"num_driver_params\":%u,\"push_consts_type\":%u,\"const_allocations\":[",
       v->info.size, v->info.constant_data_offset, v->constant_data_size,
       cs->allocs.max_const_offset_vec4*4, cs->num_ubos, cs->num_app_ubos,
       cs->num_driver_params, cs->push_consts_type);
   for (unsigned i=0; i<IR3_CONST_ALLOC_MAX; ++i) {
      const struct ir3_const_allocation *a=&cs->allocs.consts[i];
      json_printf(f, "%s{\"kind\":%u,\"offset_vec4\":%u,\"size_vec4\":%u,"
                     "\"reserved_size_vec4\":%u,\"reserved_align_vec4\":%u}",
                  i ? ",":"", i,a->offset_vec4,a->size_vec4,a->reserved_size_vec4,a->reserved_align_vec4);
   }
   json_printf(f, "],\"reserved_vec4\":%u,\"driver_ubos\":[", cs->allocs.reserved_vec4);
   const struct ir3_driver_ubo *ubos[]={&cs->consts_ubo,&cs->driver_params_ubo,&cs->primitive_map_ubo,&cs->primitive_param_ubo};
   for(unsigned i=0;i<4;++i) json_printf(f,"%s{\"idx\":%d,\"size\":%u}",i?",":"",ubos[i]->idx,ubos[i]->size);
   json_printf(f,"],\"ubo_ranges\":[");
   for(unsigned i=0;i<cs->ubo_state.num_enabled;++i) {
      const struct ir3_ubo_range *r=&cs->ubo_state.range[i];
      json_printf(f,"%s{\"block\":%u,\"bindless_base\":%u,\"bindless\":%s,\"global\":%s,"
                    "\"offset\":%u,\"start\":%u,\"end\":%u}",i?",":"",r->ubo.block,
        r->ubo.bindless_base,r->ubo.bindless?"true":"false",r->ubo.global?"true":"false",r->offset,r->start,r->end);
   }
   json_printf(f,"],\"ubo_range_size\":%u,\"num_ssbos\":%u,\"num_uavs\":%u,"
                 "\"has_ssbo\":%s,\"pvtmem_per_wave\":%s,\"shared_size\":%u,"
                 "\"local_size\":[%u,%u,%u],\"local_size_variable\":%s,\"has_barrier\":%s,"
                 "\"need_driver_params\":%s,\"no_earlyz\":%s,\"has_kill\":%s,"
                 "\"sample_shading\":%s,\"fragcoord_compmask\":%u,\"frag_face\":%s,"
                 "\"reads_smask\":%s,\"reads_primid\":%s,\"prefetch_bary_type\":%u,"
                 "\"max_waves\":%d,\"subgroup_size\":%u,\"output_size\":%u,\"vtxid_base\":%u,",
      cs->ubo_state.size,v->num_ssbos,v->num_uavs,v->has_ssbo?"true":"false",v->pvtmem_per_wave?"true":"false",
      v->shared_size,v->local_size[0],v->local_size[1],v->local_size[2],v->local_size_variable?"true":"false",
      v->has_barrier?"true":"false",v->need_driver_params?"true":"false",v->no_earlyz?"true":"false",
      v->has_kill?"true":"false",v->sample_shading?"true":"false",v->fragcoord_compmask,v->frag_face?"true":"false",
      v->reads_smask?"true":"false",v->reads_primid?"true":"false",v->prefetch_bary_type,
      v->info.max_waves,v->info.subgroup_size,v->output_size,v->vtxid_base);
   json_printf(f,"\"image_mapping\":{\"num_tex\":%u,\"tex_base\":%u,\"ssbo_to_tex\":[",v->image_mapping.num_tex,v->image_mapping.tex_base);
   for(unsigned i=0;i<IR3_MAX_SHADER_BUFFERS;++i) json_printf(f,"%s%u",i?",":"",v->image_mapping.ssbo_to_tex[i]);
   json_printf(f,"],\"image_to_tex\":[");
   for(unsigned i=0;i<IR3_MAX_SHADER_IMAGES;++i) json_printf(f,"%s%u",i?",":"",v->image_mapping.image_to_tex[i]);
   json_printf(f,"],\"tex_to_image\":[");
   for(unsigned i=0;i<32;++i) json_printf(f,"%s%u",i?",":"",v->image_mapping.tex_to_image[i]);
   json_printf(f,"]},\"compute\":");
   if(v->type==MESA_SHADER_COMPUTE)
      json_printf(f,"{\"req_local_mem\":%u,\"force_linear_dispatch\":%s,\"round_robin_mode\":%s,\"local_invocation_id\":%u,\"work_group_id\":%u}",
         v->cs.req_local_mem,v->cs.force_linear_dispatch?"true":"false",v->cs.round_robin_mode?"true":"false",v->cs.local_invocation_id,v->cs.work_group_id);
   else json_printf(f,"null");
   if (c->nv12 && c->nv12->count) {
      json_printf(f,",\"sgfx_nv12_profile\":{\"version\":2,\"textures\":[");
      for (unsigned i = 0; i < c->nv12->count; i++) {
         const struct scarlet_ir3_nv12_profile *p = &c->nv12->profiles[i];
         json_printf(f,"%s{\"slot\":%u,\"matrix\":%u,\"range\":%u,\"chroma_x\":%u,\"chroma_y\":%u,\"sample_instructions\":%u,\"query_instructions\":%u}",
            i?",":"",p->texture_slot,p->matrix,p->range,p->chroma_x,p->chroma_y,c->nv12->samples[i],c->nv12->queries[i]);
      }
      json_printf(f,"],\"samplers\":[");
      for (unsigned i=0;i<c->nv12->sampler_count;i++) {
         const struct scarlet_ir3_nv12_sampler *p = &c->nv12->samplers[i];
         json_printf(f,"%s{\"slot\":%u,\"address_u\":%u,\"address_v\":%u,\"min_filter\":%u,\"mag_filter\":%u}",
            i?",":"",p->slot,p->address_u,p->address_v,p->min_filter,p->mag_filter);
      }
      json_printf(f,"],\"nearest_sampler\":");
      if (c->nv12->reconstructs) json_printf(f,"%u",c->nv12->nearest_sampler);
      else json_printf(f,"null");
      json_printf(f,"}");
   }
   json_printf(f,"}%s",last?"":",");

}


/* Naga remaps descriptor sets to group 0 and dense slots before SPIR-V.
 * The IR3 non-bindless UBO/SSBO ABI consumes a scalar slot and byte offset. */
static bool lower_resources(nir_builder *b, nir_intrinsic_instr *i, void *data) {
   (void)data;
   b->cursor=nir_before_instr(&i->instr);
   nir_def *value=NULL;
   switch(i->intrinsic) {
   case nir_intrinsic_image_deref_store:
   case nir_intrinsic_image_deref_size: {
      nir_deref_instr *deref = nir_src_as_deref(i->src[0]);
      while (deref->deref_type == nir_deref_type_cast) deref = nir_src_as_deref(deref->parent);
      nir_variable *var = nir_deref_instr_get_variable(deref);
      /* Frontend admits only individual, non-arrayed 2D RGBA8 write-only images. */
      assert(var && deref->deref_type == nir_deref_type_var && var->data.binding < 16);
      nir_rewrite_image_intrinsic(i, nir_imm_int(b, var->data.binding), nir_image_intrinsic_type_default);
      BITSET_SET(b->shader->info.images_used, var->data.binding);
      b->shader->info.num_images = MAX2(b->shader->info.num_images, var->data.binding + 1);
      return true;
   }
   case nir_intrinsic_vulkan_resource_index:
      value=nir_vec2(b,nir_iadd_imm(b,i->src[0].ssa,nir_intrinsic_binding(i)),nir_imm_int(b,0));
      break;
   case nir_intrinsic_vulkan_resource_reindex:
      value=nir_vec2(b,nir_iadd(b,nir_channel(b,i->src[0].ssa,0),i->src[1].ssa),nir_channel(b,i->src[0].ssa,1));
      break;
   case nir_intrinsic_load_vulkan_descriptor: value=i->src[0].ssa; break;
   case nir_intrinsic_load_push_constant:
      value=nir_load_const_ir3(b,i->num_components,i->def.bit_size,nir_ushr_imm(b,i->src[0].ssa,2),.base=nir_intrinsic_base(i)/4);
      break;
   default: return false;
   }
   nir_def_replace(&i->def,value);
   return true;
}
/* Match Turnip's shared-memory layout for this pinned IR3 compiler. */
static void shared_type_info(const struct glsl_type *type, unsigned *size, unsigned *alignment) {
   assert(glsl_type_is_vector_or_scalar(type));
   unsigned component = glsl_type_is_boolean(type) ? 4 : glsl_get_bit_size(type) / 8;
   *size = component * glsl_get_vector_elements(type);
   *alignment = component;
}
static nir_shader *load_spirv(struct ir3_compiler *compiler, const uint32_t *words,
                             size_t count, mesa_shader_stage stage, const char *entry,
                             uint32_t rasterization_samples, struct nv12_lowering_state *nv12)
{
   struct spirv_to_nir_options options = {
      .environment = NIR_SPIRV_VULKAN,
      .ignore_replacement = true,
      .skip_os_break_in_debug_build = true,
      .ubo_addr_format = nir_address_format_32bit_index_offset,
      .ssbo_addr_format = nir_address_format_32bit_index_offset,
      .push_const_addr_format = nir_address_format_32bit_offset,
      .shared_addr_format = nir_address_format_32bit_offset,
   };
   nir_shader *nir = spirv_to_nir(words, count, NULL, stage, entry, &options,
                                ir3_get_compiler_options(compiler));
   if (!nir) return NULL;
   if (stage == MESA_SHADER_FRAGMENT) {
      /* Mesa tu_shader.cc lowers PrimitiveId from SPIR-V's system value to a
       * flat VPC input. A618 supplies its real primitive counter at PRIMIDLOC;
       * leaving it as SYSTEM_VALUE_PRIMITIVE_ID would use an unbound FS reg.
       * No sample-frequency, multiview, or geometry-stage lowering is enabled.
       */
      const nir_lower_sysvals_to_varyings_options sysvals = { .primitive_id = true };
      NIR_PASS(_, nir, nir_lower_sysvals_to_varyings, &sysvals);
   }
   NIR_PASS(_, nir, nir_lower_variable_initializers, nir_var_all);
   NIR_PASS(_, nir, nir_lower_returns);
   NIR_PASS(_, nir, nir_inline_functions);
   nir_remove_non_entrypoints(nir);
   NIR_PASS(_, nir, nir_lower_global_vars_to_local);
   NIR_PASS(_, nir, nir_split_var_copies);
   NIR_PASS(_, nir, nir_split_struct_vars, nir_var_function_temp);
   NIR_PASS(_, nir, nir_lower_var_copies);
   NIR_PASS(_, nir, nir_lower_system_values);
   NIR_PASS(_, nir, nir_lower_explicit_io, nir_var_mem_ubo | nir_var_mem_ssbo, nir_address_format_32bit_index_offset);
   NIR_PASS(_, nir, nir_lower_explicit_io, nir_var_mem_push_const, nir_address_format_32bit_offset);
   NIR_PASS(_, nir, nir_shader_intrinsics_pass, lower_resources, nir_metadata_control_flow, NULL);
   unsigned used_image_slots = nir->info.num_images;
   NIR_PASS(_, nir, nir_opt_deref);
   NIR_PASS(_, nir, nir_opt_copy_prop);
   NIR_PASS(_, nir, nir_opt_dce);
   NIR_PASS(_, nir, nir_lower_samplers);
   if (nv12->count) {
      nv12_choose_sampler(nir, nv12);
      NIR_PASS(_, nir, nir_shader_tex_pass, lower_nv12_sample, nir_metadata_none, nv12);
      if (nv12->error) { ralloc_free(nir); return NULL; }
   }

   nir_foreach_function_impl(impl, nir) {
      nir_foreach_block(block, impl) {
         nir_foreach_instr(instr, block) {
            if (instr->type != nir_instr_type_tex) continue;
            nir_tex_instr *tex=nir_instr_as_tex(instr);
            BITSET_SET(nir->info.textures_used, tex->texture_index);
            /* IR3 shares the upper bound for texture and sampler descriptor
             * counts, although the actual instruction indices are independent. */
            if (nir_tex_instr_need_sampler(tex)) {
               BITSET_SET(nir->info.textures_used, tex->sampler_index);
               BITSET_SET(nir->info.samplers_used, tex->sampler_index);
            }
         }
      }
   }
   if (stage == MESA_SHADER_COMPUTE) {
      NIR_PASS(_, nir, nir_remove_dead_variables, nir_var_mem_shared, NULL);
      NIR_PASS(_, nir, nir_lower_vars_to_explicit_types, nir_var_mem_shared, shared_type_info);
      if (nir->info.shared_size > compiler->info->cs_shared_mem_size) {
         ralloc_free(nir);
         return NULL;
      }
      NIR_PASS(_, nir, nir_lower_explicit_io, nir_var_mem_shared, nir_address_format_32bit_offset);
      const nir_lower_compute_system_values_options opts={0};
      NIR_PASS(_, nir, nir_lower_compute_system_values, &opts);
   }
   nir_foreach_variable_with_modes(var, nir, nir_var_mem_ubo | nir_var_mem_ssbo) {
      if (var->data.mode == nir_var_mem_ubo) nir->info.num_ubos=MAX2(nir->info.num_ubos,var->data.binding+1);
      else nir->info.num_ssbos=MAX2(nir->info.num_ssbos,var->data.binding+1);
   }

   ir3_nir_lower_io_vars_to_temporaries(nir);
   NIR_PASS(_, nir, nir_lower_vars_to_ssa);
   nir_assign_io_var_locations(nir, nir_var_shader_in);
   nir_assign_io_var_locations(nir, nir_var_shader_out);
   ir3_nir_lower_io(nir);
   if (stage == MESA_SHADER_FRAGMENT && rasterization_samples == 1) {
      /* Official Mesa pass (also used by brw/agx for known samples==1), not a
       * generic constant substitute: lower sample ID/position and every
       * sample/centroid interpolation form to the one pixel-center sample.
       * Preserve the hardware coverage/helper sample-mask path.
       */
      const nir_lower_single_sampled_options single = { .lower_sample_mask_in = false };
      NIR_PASS(_, nir, nir_lower_single_sampled, &single);
      /* SPIR-V records SampleId/Sample decorations as requiring sample rate.
       * Their semantics have now been lowered under the explicit samples==1
       * profile, where one invocation/sample is the pixel-frequency path.
       */
      nir->info.fs.uses_sample_shading = false;
   }
   nir_shader_gather_info(nir, nir_shader_get_entrypoint(nir));
   /* Graphics shares a single IBO table. Never let stage-local SSBO liveness
    * move image slots across separately compiled VS/FS. Public buffer slots
    * occupy 0..15; image slots occupy 16..31 in every stage. */
   nir->info.num_images = used_image_slots;
   if (nir->info.num_images) nir->info.num_ssbos = 16;
   ir3_finalize_nir(compiler, &(struct ir3_shader_nir_options){0}, nir);
   return nir;
}

void scarlet_ir3_result_free(struct scarlet_ir3_result *out) {
   if (!out) return;
   free(out->allocation); free(out->metadata); free(out->error);
   memset(out,0,sizeof(*out));
}
int scarlet_ir3_compile_with_nv12_samplers(const uint32_t *words, size_t word_count, uint32_t stage,
                       const char *entry, uint32_t push_bytes, uint32_t rasterization_samples,
                       const struct scarlet_ir3_nv12_profile *profiles, uint32_t profile_count,
                       const struct scarlet_ir3_nv12_sampler *samplers, uint32_t sampler_count,
                       struct scarlet_ir3_result *out) {
   if(!out) return 1;
   memset(out,0,sizeof(*out));
   if (profile_count > 16 || (profile_count && !profiles)) return 1;
   for (unsigned i = 0; i < profile_count; i++) {
      const struct scarlet_ir3_nv12_profile *p = &profiles[i];
      if (p->texture_slot >= 16 || (i && profiles[i-1].texture_slot >= p->texture_slot) ||
          p->matrix < 1 || p->matrix > 2 || p->range < 1 || p->range > 2 ||
          p->chroma_x < 1 || p->chroma_x > 2 || p->chroma_y < 1 || p->chroma_y > 2) return 1;
   }
   if (sampler_count > 16 || (sampler_count && !samplers)) return 1;
   for (unsigned i=0;i<sampler_count;i++) {
      const struct scarlet_ir3_nv12_sampler *p = &samplers[i];
      if (p->slot>=16 || (i && samplers[i-1].slot>=p->slot) || p->address_u>3 || p->address_v>3 ||
          p->min_filter>1 || p->mag_filter>1) return 1;
   }
   struct nv12_lowering_state nv12 = { .profiles = profiles, .count = profile_count,
      .samplers = samplers, .sampler_count = sampler_count };

   if ((stage == 1 && rasterization_samples != 1) ||
       (stage != 1 && rasterization_samples != 0)) return 1;
#ifdef __scarlet__
   /* Exercise the actual varargs/float ABI before entering Mesa. A broken
    * native runtime must report an error, never silently return bogus IR. */
   if(scarlet_ir3_format_check()) {
      const char *error="LLVM printf runtime self-test failed";
      out->error=malloc(strlen(error)+1);
      if(out->error) strcpy(out->error,error);
      return 3;
   }
#endif
   if(!words || word_count<5 || word_count>1024*1024 || words[0]!=0x07230203 || !entry || !*entry || stage>2 || push_bytes>128) return 1;
   int status=2;
   struct ir3_compiler *compiler=NULL;
   struct ir3_shader *shader=NULL;
   nir_shader *nir=NULL;
   struct json json={0};
   glsl_type_singleton_init_or_ref();
   const struct fd_dev_id id={.gpu_id=618};
   struct fd_dev_info device_info=*fd_dev_info_raw(&id);
   /* ABI 1 emits explicit texture sampling, with no pre-dispatch sampler state.
    * Keep optimization choices local; never mutate Mesa's global device table. */
   device_info.props.has_fs_tex_prefetch=false;
   compiler=ir3_compiler_create(NULL,&id,&device_info,&(struct ir3_compiler_options){.disable_cache=true});
   if(!compiler) goto cleanup;
   /* ABI 1 reserves constants only for CP uploads; no shader preamble stores. */
   compiler->has_preamble=false;
   mesa_shader_stage stages[]={MESA_SHADER_VERTEX,MESA_SHADER_FRAGMENT,MESA_SHADER_COMPUTE};
   nir=load_spirv(compiler,words,word_count,stages[stage],entry,rasterization_samples,&nv12);
   if(!nir) goto cleanup;
   struct ir3_shader_options options={.api_wavesize=IR3_SINGLE_OR_DOUBLE,.real_wavesize=IR3_SINGLE_OR_DOUBLE};
   if(push_bytes) {
      options.push_consts_type=IR3_PUSH_CONSTS_PER_STAGE;
      options.push_consts_dwords=(push_bytes+3)/4;
      ir3_const_alloc(&options.const_allocs,IR3_CONST_ALLOC_PUSH_CONSTS,(push_bytes+15)/16,1);
   }
   shader=ir3_shader_from_nir(compiler,nir,&options); nir=NULL;
   if(!shader) goto cleanup;
   struct ir3_shader_key key={0};
   struct ir3_shader_variant *v=ir3_shader_get_variant(shader,&key,false,false,NULL,NULL);
   if(!v || !v->info.size || v->info.size>16*1024*1024) goto cleanup;
   out->allocation=malloc(v->info.size);
   if(!out->allocation) goto cleanup;
   memcpy(out->allocation,v->bin,v->info.size); out->allocation_size=v->info.size;
   struct compiled result={"runtime",shader,v,rasterization_samples,&nv12};
   print_variant_json(&json,&result,true);
   if(json.failed) goto cleanup;
   out->metadata=json.data; out->metadata_size=json.size; json.data=NULL;
   status=0;
cleanup:
   free(json.data);
   if(nir) ralloc_free(nir);
   if(shader) ir3_shader_destroy(shader);
   if(compiler) ir3_compiler_destroy(compiler);
   glsl_type_singleton_decref();
   if(status) {
      scarlet_ir3_result_free(out);
      const char *error=nv12.error ? nv12.error : "Mesa SPIR-V/NIR/IR3 compilation or allocation failed";
      out->error=malloc(strlen(error)+1);
      if(out->error) strcpy(out->error,error);
   }
   return status;
}

/* Preserve the original C symbol for existing private callers and link-root
 * validation. New Rust callers require the explicit profile entry above, so an
 * older archive cannot silently compile newly admitted sample syntax. */
int scarlet_ir3_compile(const uint32_t *words, size_t word_count, uint32_t stage,
                       const char *entry, uint32_t push_bytes,
                       struct scarlet_ir3_result *out) {
   return scarlet_ir3_compile_with_samples(words, word_count, stage, entry,
                                          push_bytes, stage == 1 ? 1 : 0, out);
}

/* Existing live cache10 entry remains available with identical defaults. */
int scarlet_ir3_compile_with_nv12(const uint32_t *words, size_t count, uint32_t stage,
      const char *entry, uint32_t push, uint32_t samples,
      const struct scarlet_ir3_nv12_profile *profiles, uint32_t profile_count,
      struct scarlet_ir3_result *out) {
   return scarlet_ir3_compile_with_nv12_samplers(words,count,stage,entry,push,samples,
      profiles,profile_count,NULL,0,out);
}

int scarlet_ir3_compile_with_samples(const uint32_t *words, size_t word_count, uint32_t stage,
                       const char *entry, uint32_t push_bytes, uint32_t rasterization_samples,
                       struct scarlet_ir3_result *out) {
   return scarlet_ir3_compile_with_nv12(words,word_count,stage,entry,push_bytes,rasterization_samples,NULL,0,out);
}
