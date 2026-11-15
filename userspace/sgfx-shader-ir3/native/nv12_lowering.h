/* SPDX-License-Identifier: GPL-2.0-only */
/* Combined A618 NV12 gives Y,Cb,Cr. A private nearest/clamp sampler can
 * reconstruct one logical color without extra plane handles/descriptors.
 * (q+.75)/size avoids ties and preserves floor((q+cosited)/2) chroma siting.
 * Address logical texels before reconstruction; never wrap UV independently.
 */
struct nv12_lowering_state {
   const struct scarlet_ir3_nv12_profile *profiles;
   uint32_t count;
   const struct scarlet_ir3_nv12_sampler *samplers;
   uint32_t sampler_count;
   uint32_t samples[16], queries[16];
   uint32_t nearest_sampler;
   bool reconstructs;
   const char *error;
};

static void nv12_coefficients(const struct scarlet_ir3_nv12_profile *p, float m[3][4])
{
   const float kr = p->matrix == 1 ? 0.299f : 0.2126f;
   const float kb = p->matrix == 1 ? 0.114f : 0.0722f;
   const float ys = p->range == 1 ? 255.0f / 219.0f : 1.0f;
   const float cs = p->range == 1 ? 255.0f / 224.0f : 1.0f;
   const float yo = p->range == 1 ? 16.0f / 255.0f : 0.0f;
   const float rv = 2.0f * (1.0f - kr) * cs;
   const float bu = 2.0f * (1.0f - kb) * cs;
   const float gu = -2.0f * kb * (1.0f - kb) / (1.0f - kr - kb) * cs;
   const float gv = -2.0f * kr * (1.0f - kr) / (1.0f - kb - kr) * cs;
   const float co = 128.0f / 255.0f;
   const float rows[3][4] = {
      {ys, 0.0f, rv, -ys * yo - rv * co},
      {ys, gu, gv, -ys * yo - (gu + gv) * co},
      {ys, bu, 0.0f, -ys * yo - bu * co},
   };
   memcpy(m, rows, sizeof(rows));
}

static nir_def *nv12_convert(nir_builder *b, nir_def *raw,
                             const struct scarlet_ir3_nv12_profile *p)
{
   float matrix[3][4]; nv12_coefficients(p, matrix);
   nir_def *v = nir_vec4(b, nir_channel(b, raw, 0), nir_channel(b, raw, 1),
                          nir_channel(b, raw, 2), nir_imm_float(b, 1.0f));
   nir_def *rgb[3];
   for (unsigned r = 0; r < 3; r++)
      rgb[r] = nir_fsat(b, nir_fdot4(b, v, nir_imm_vec4(b,
         matrix[r][0], matrix[r][1], matrix[r][2], matrix[r][3])));
   return nir_vec4(b, rgb[0], rgb[1], rgb[2], nir_imm_float(b, 1.0f));
}

static nir_def *nv12_dimensions(nir_builder *b, unsigned slot)
{
   nir_tex_instr *t = nir_tex_instr_create(b->shader, 1);
   t->op = nir_texop_txs; t->sampler_dim = GLSL_SAMPLER_DIM_2D;
   t->texture_index = slot; t->dest_type = nir_type_int32;
   t->src[0].src_type = nir_tex_src_lod;
   t->src[0].src = nir_src_for_ssa(nir_imm_int(b, 0));
   nir_def_init(&t->instr, &t->def, 2, 32);
   nir_builder_instr_insert(b, &t->instr);
   return &t->def;
}

static nir_def *nv12_address(nir_builder *b, nir_def *q, nir_def *size,
                             unsigned mode, nir_def **valid)
{
   nir_def *zero = nir_imm_int(b, 0), *last = nir_iadd_imm(b, size, -1);
   if (mode == 1) return nir_iclamp(b, q, zero, last);
   if (mode == 3) {
      *valid = nir_iand(b, *valid, nir_iand(b, nir_ige(b, q, zero), nir_ilt(b, q, size)));
      return nir_iclamp(b, q, zero, last);
   }
   nir_def *period = mode == 2 ? nir_imul_imm(b, size, 2) : size;
   nir_def *r = nir_irem(b, q, period);
   r = nir_bcsel(b, nir_ilt(b, r, zero), nir_iadd(b, r, period), r);
   if (mode == 2) r = nir_bcsel(b, nir_ilt(b, r, size), r,
                                nir_isub(b, nir_iadd_imm(b, period, -1), r));
   return r;
}

static nir_def *nv12_texel(nir_builder *b, struct nv12_lowering_state *s,
                           unsigned index, nir_def *q, nir_def *dims,
                           unsigned u, unsigned v, nir_def *valid)
{
   s->reconstructs = true;
   nir_def *x = nv12_address(b, nir_channel(b, q, 0), nir_channel(b, dims, 0), u, &valid);
   nir_def *y = nv12_address(b, nir_channel(b, q, 1), nir_channel(b, dims, 1), v, &valid);
   nir_push_if(b, valid);
   nir_def *coord = nir_fmul(b, nir_fadd_imm(b, nir_i2f32(b, nir_vec2(b, x, y)), .75f),
                              nir_frcp(b, nir_i2f32(b, dims)));
   nir_tex_instr *t = nir_tex_instr_create(b->shader, 2);
   t->op = nir_texop_txl; t->sampler_dim = GLSL_SAMPLER_DIM_2D;
   t->texture_index = s->profiles[index].texture_slot;
   t->sampler_index = s->nearest_sampler; t->dest_type = nir_type_float32;
   t->coord_components = 2;
   t->src[0].src_type = nir_tex_src_coord; t->src[0].src = nir_src_for_ssa(coord);
   t->src[1].src_type = nir_tex_src_lod; t->src[1].src = nir_src_for_ssa(nir_imm_float(b, 0));
   nir_def_init(&t->instr, &t->def, 4, 32);
   nir_builder_instr_insert(b, &t->instr);
   nir_def *color = nv12_convert(b, &t->def, &s->profiles[index]);
   nir_push_else(b, NULL);
   nir_def *zero = nir_imm_vec4(b, 0, 0, 0, 0);
   nir_pop_if(b, NULL);
   return nir_if_phi(b, color, zero);
}

static nir_def *nv12_coord(nir_builder *b, nir_tex_instr *t, nir_def *dims)
{
   int src = nir_tex_instr_src_index(t, nir_tex_src_coord);
   nir_def *p = nir_fmul(b, t->src[src].src.ssa, nir_i2f32(b, dims));
   src = nir_tex_instr_src_index(t, nir_tex_src_offset);
   if (src >= 0) p = nir_fadd(b, p, nir_i2f32(b, t->src[src].src.ssa));
   return p;
}

static bool lower_nv12_sample(nir_builder *b, nir_tex_instr *tex, void *data)
{
   struct nv12_lowering_state *s = data;
   unsigned index;
   for (index = 0; index < s->count; index++)
      if (s->profiles[index].texture_slot == tex->texture_index) break;
   if (index == s->count) return false;
   if (tex->sampler_dim != GLSL_SAMPLER_DIM_2D || tex->is_array || tex->is_shadow ||
       nir_tex_instr_src_index(tex, nir_tex_src_texture_offset) >= 0 ||
       nir_tex_instr_src_index(tex, nir_tex_src_sampler_offset) >= 0) {
      s->error = "NV12 requires statically bound non-array 2D color resources"; return false;
   }
   switch (tex->op) {
   case nir_texop_txs: case nir_texop_query_levels: case nir_texop_lod:
      s->queries[index]++; return false;
   case nir_texop_tex: case nir_texop_txb: case nir_texop_txl: case nir_texop_txd:
   case nir_texop_txf: case nir_texop_tg4: break;
   default: s->error = "unsupported NV12 texture operation"; return false;
   }
   if (tex->dest_type != nir_type_float32 || tex->def.bit_size != 32 || tex->def.num_components != 4) {
      s->error = "NV12 requires four-component f32 results"; return false;
   }
   const struct scarlet_ir3_nv12_sampler *sampler = NULL;
   for (unsigned i = 0; i < s->sampler_count; i++)
      if (s->samplers[i].slot == tex->sampler_index) sampler = &s->samplers[i];
   bool border = sampler && (sampler->address_u == 3 || sampler->address_v == 3);
   if (tex->op != nir_texop_txf && tex->op != nir_texop_tg4 && !border) {
      b->cursor = nir_after_instr(&tex->instr);
      nir_def_rewrite_uses_after(&tex->def, nv12_convert(b, &tex->def, &s->profiles[index]));
      s->samples[index]++; return true;
   }
   if (tex->op != nir_texop_txf && !sampler) {
      s->error = "NV12 gather/border requires the exact bound sampler profile"; return false;
   }
   b->cursor = nir_before_instr(&tex->instr);
   nir_def *dims = nv12_dimensions(b, tex->texture_index), *result;
   if (tex->op == nir_texop_txf) {
      int c = nir_tex_instr_src_index(tex, nir_tex_src_coord), l = nir_tex_instr_src_index(tex, nir_tex_src_lod);
      nir_def *valid = l >= 0 ? nir_ieq_imm(b, tex->src[l].src.ssa, 0) : nir_imm_true(b);
      result = nv12_texel(b, s, index, tex->src[c].src.ssa, dims, 3, 3, valid);
   } else {
      nir_def *p = nv12_coord(b, tex, dims);
      if (tex->op == nir_texop_tg4) {
         nir_def *q = nir_f2i32(b, nir_ffloor(b, nir_fadd_imm(b, p, -.5f)));
         const int xy[4][2] = {{0,1},{1,1},{1,0},{0,0}};
         nir_def *values[4];
         for (unsigned i = 0; i < 4; i++) {
            nir_def *at = nir_iadd(b, q, nir_imm_ivec2(b, xy[i][0], xy[i][1]));
            values[i] = nir_channel(b, nv12_texel(b, s, index, at, dims,
               sampler->address_u, sampler->address_v, nir_imm_true(b)), tex->component);
         }
         result = nir_vec(b, values, 4);
      } else {
         nir_def *linear;
         if (sampler->min_filter == sampler->mag_filter) {
            linear = nir_imm_bool(b, sampler->min_filter != 0);
         } else {
            nir_def *minifying;
            int l = nir_tex_instr_src_index(tex, nir_tex_src_lod);
            if (l >= 0) minifying = nir_flt(b, nir_imm_float(b, 0), tex->src[l].src.ssa);
            else {
               int dx = nir_tex_instr_src_index(tex, nir_tex_src_ddx), dy = nir_tex_instr_src_index(tex, nir_tex_src_ddy);
               nir_def *x = dx >= 0 ? nir_fmul(b, tex->src[dx].src.ssa, nir_i2f32(b,dims)) : nir_ddx(b,p);
               nir_def *y = dy >= 0 ? nir_fmul(b, tex->src[dy].src.ssa, nir_i2f32(b,dims)) : nir_ddy(b,p);
               nir_def *rho = nir_fmax(b,nir_fdot2(b,x,x),nir_fdot2(b,y,y));
               int bias = nir_tex_instr_src_index(tex,nir_tex_src_bias);
               if (bias >= 0) rho = nir_fmul(b,rho,nir_fexp2(b,nir_fmul_imm(b,tex->src[bias].src.ssa,2)));
               minifying = nir_flt(b,nir_imm_float(b,1),rho);
            }
            linear = nir_bcsel(b, minifying, nir_imm_bool(b,sampler->min_filter),nir_imm_bool(b,sampler->mag_filter));
         }
         nir_push_if(b, linear);
         nir_def *center = nir_fadd_imm(b,p,-.5f), *q = nir_f2i32(b,nir_ffloor(b,center));
         nir_def *frac = nir_ffract(b,center), *colors[4];
         for (unsigned i = 0; i < 4; i++) colors[i] = nv12_texel(b,s,index,
            nir_iadd(b,q,nir_imm_ivec2(b,i&1,i>>1)),dims,sampler->address_u,sampler->address_v,nir_imm_true(b));
         nir_def *lo = nir_flrp(b,colors[0],colors[1],nir_channel(b,frac,0));
         nir_def *hi = nir_flrp(b,colors[2],colors[3],nir_channel(b,frac,0));
         nir_def *filtered = nir_flrp(b,lo,hi,nir_channel(b,frac,1));
         nir_push_else(b,NULL);
         nir_def *nearest = nv12_texel(b,s,index,nir_f2i32(b,nir_ffloor(b,p)),dims,
            sampler->address_u,sampler->address_v,nir_imm_true(b));
         nir_pop_if(b,NULL);
         result = nir_if_phi(b,filtered,nearest);
      }
   }
   nir_def_rewrite_uses(&tex->def,result);
   nir_instr_remove(&tex->instr);
   s->samples[index]++; return true;
}

static void nv12_choose_sampler(nir_shader *nir, struct nv12_lowering_state *s)
{
   uint32_t used = 0;
   nir_foreach_function_impl(impl,nir) nir_foreach_block(block,impl) nir_foreach_instr(instr,block) {
      if (instr->type != nir_instr_type_tex) continue;
      nir_tex_instr *t = nir_instr_as_tex(instr);
      if (t->op == nir_texop_tex || t->op == nir_texop_txb || t->op == nir_texop_txl ||
          t->op == nir_texop_txd || t->op == nir_texop_tg4 || t->op == nir_texop_lod)
         used |= 1u << t->sampler_index;
   }
   for (s->nearest_sampler=0; s->nearest_sampler<16; s->nearest_sampler++)
      if (!(used & (1u<<s->nearest_sampler))) break;
}
