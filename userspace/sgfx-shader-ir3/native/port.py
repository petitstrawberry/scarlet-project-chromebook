# SPDX-License-Identifier: MIT
# Bounded POSIX port adapted from sgfx-shader-agx; actual Mesa compiler is intact.
import hashlib,json,os,pathlib
def remove_function(text,name):
    marker='\n'+name+'('
    pos=text.index(marker)
    start=text.rfind('\n',0,pos)
    begin=text.index('{',pos)
    depth=1;end=begin+1
    while depth:
        if text[end]=='{':depth+=1
        if text[end]=='}':depth-=1
        end+=1
    return text[:start]+f'\n/* Unused standalone compiler function {name} omitted. */\n'+text[end:]

def apply(source,overlay,write,HERE):
 root=(source/'meson.build').read_text()
 root=root.replace('pre_args = [', "pre_args = ['-include', "+repr(str(HERE/'platform/compat.h'))+", '-femulated-tls', '-ffunction-sections', '-fdata-sections',",1)
 root=root.replace("if host_machine.system() != 'windows'\n  if not cc.has_function('dlopen')", "if host_machine.system() not in ['windows', 'scarlet']\n  if not cc.has_function('dlopen')")
 write(overlay,'meson.build',root)
 path='src/freedreno/ir3/meson.build'
 target=(overlay/path).read_text().replace('files('+repr(str(HERE/'compiler.c'))+')', 'files('+repr(str(HERE/'compiler.c'))+','+repr(str(HERE/'platform/string.c'))+','+repr(str(HERE/'platform/printf.cpp'))+')')
 libc=os.environ.get('SCARLET_LLVM_LIBC_SOURCE_DIR')
 if not libc:raise SystemExit('SCARLET_LLVM_LIBC_SOURCE_DIR required')
 crt=pathlib.Path(libc).parent/'compiler-rt/lib/builtins'
 if hashlib.sha256((crt/'emutls.c').read_bytes()).hexdigest()!='a4437f3e329a0835adbc7df38e36df12f358d05c073b11813c409f849c2345d2':raise SystemExit('wrong LLVM emutls source')
 target="scarlet_emutls=static_library('scarlet_emutls',files("+repr(str(crt/'emutls.c'))+"),c_args:['-Wno-error=missing-prototypes'],build_by_default:false)\n"+target.replace('link_whole: [','link_whole: [scarlet_emutls,')
 target=target.replace('c_args: [no_override_init_args],', 'c_args: [no_override_init_args], cpp_args: ['+repr('-I'+libc)+'],')
 write(overlay,path,target)
 # Scarlet implements the POSIX runtime interfaces used by Mesa, without
 # being Linux. Keep this an explicit OS port, never spoof __linux__.
 path='src/util/detect_os.h'
 text=(source/path).read_text().replace('#if defined(__linux__)',
  '#if defined(__scarlet__)\n#define DETECT_OS_POSIX 1\n#endif\n\n#if defined(__linux__)',1)
 write(overlay,path,text)
 path='src/util/meson.build'
 text=(source/path).read_text()
 # Pipeline caching is owned by SGFX. Mesa's optional on-disk database is
 # outside this standalone compiler and has no references from NIR/Asahi.
 unused_objects=json.loads((HERE/'util-closure.json').read_text())['unused_util']
 unused_files=[obj[:-2] for obj in unused_objects]
 unused_files += ['disk_cache.c','disk_cache_os.c']
 for unused in unused_files:
  text=text.replace("  '"+unused+"',\n",'')
 write(overlay,path,text)
 path='src/compiler/nir/nir_constant_expressions.py'
 text=(source/path).read_text().replace('fenv_t __nir_const_fenv','struct scarlet_ir3_fenv __nir_const_fenv').replace('feholdexcept(&','scarlet_ir3_feholdexcept(&').replace('fesetenv(&','scarlet_ir3_fesetenv(&')
 write(overlay,path,text)
 path='src/util/log.c'
 text=(source/path).read_text().replace('#if DETECT_OS_POSIX', '#if DETECT_OS_POSIX && !defined(__scarlet__)').replace('#if !DETECT_OS_WINDOWS', '#if !DETECT_OS_WINDOWS && !defined(__scarlet__)')
 write(overlay,path,text)
 path='src/util/memstream.h'
 text=(source/path).read_text().replace('#ifdef _WIN32\n   char **bufp;', '#if defined(_WIN32) || defined(__scarlet__)\n   char **bufp;')
 write(overlay,path,text)
 path='src/util/memstream.c'
 text=(source/path).read_text()
 text=text.replace('#else\n   FILE *const f = open_memstream',
  '#elif defined(__scarlet__)\n   mem->f = tmpfile();\n   mem->bufp = bufp; mem->sizep = sizep;\n   *bufp = NULL; *sizep = 0;\n   return mem->f != NULL;\n#else\n   FILE *const f = open_memstream',1)
 text=text.replace('#ifdef _WIN32\nstatic void', '#if defined(_WIN32) || defined(__scarlet__)\nstatic void',1)
 text=text.replace('#ifdef _WIN32\n   u_memstream_update_buffer(mem);\n   remove(mem->temp);\n#endif',
  '#if defined(_WIN32) || defined(__scarlet__)\n   u_memstream_update_buffer(mem);\n#endif\n#ifdef _WIN32\n   remove(mem->temp);\n#endif')
 text=text.replace('#ifndef _WIN32\n   return fflush(mem->f);','#if !defined(_WIN32) && !defined(__scarlet__)\n   return fflush(mem->f);')
 write(overlay,path,text)
 path='src/util/os_misc.c'
 text=(source/path).read_text()
 text=text.replace('#else\n#error unexpected platform in os_sysinfo.c', '#elif defined(__scarlet__)\n#include <unistd.h>\n#else\n#error unexpected platform in os_sysinfo.c',1)
 for name in ['os_set_option','os_get_total_physical_memory','os_get_available_system_memory','os_get_page_size']:
  text=remove_function(text,name)
 text=text[:text.rfind('#if DETECT_OS_APPLE',0,text.index('static bool jit_allowed;'))]
 write(overlay,path,text)
 path='src/c11/impl/threads_posix.c'
 text=(source/path).read_text()
 if hashlib.sha256(text.encode()).hexdigest()!='61a87567a935f9a9b0b9bb69ea08226f88c292b4b6caa439e9bf7824cb93b29b':
  raise SystemExit('wrong pinned C11 source')
 # Neither function is referenced by Asahi/NIR/util. Do not export a pretend
 # pthread_exit or timedlock. A future caller fails to link until implemented.
 begin=text.index('static int\nthreads_timespec_compare(')
 end=text.index('// 7.25.4.5',begin)
 text=text[:begin]+'/* Unused standalone mtx_timedlock omitted. */\n\n'+text[end:]
 begin=text.index('// 7.25.5.5')
 end=text.index('// 7.25.5.6',begin)
 text=text[:begin]+'/* Unused standalone thrd_exit omitted. */\n'+text[end:]
 write(overlay,path,text)
 for path in ['src/util/u_thread.h','src/util/u_thread.c']:
  text=(source/path).read_text().replace('!defined(__HAIKU__)',
   '!defined(__HAIKU__) && !defined(__scarlet__)')
  write(overlay,path,text)
 path='include/drm-uapi/drm.h'
 text=(source/path).read_text().replace('#if defined(__GNU__)', '#if defined(__GNU__) || defined(__scarlet__)',1)
 write(overlay,path,text)
 path='src/util/os_misc.h'
 text=(source/path).read_text().replace('#if DETECT_OS_POSIX\n',
  '#if DETECT_OS_POSIX && !defined(__scarlet__)\n',1)
 text=text.replace('#elif DETECT_OS_POSIX\n#  define os_break()',
  '#elif defined(__scarlet__)\n#  define os_break() __builtin_trap()\n'
  '#elif DETECT_OS_POSIX\n#  define os_break()',1)
 write(overlay,path,text)
 path='src/util/u_endian.h'
 text=(source/path).read_text().replace('#elif defined(__OPENCL_VERSION__)',
  '#elif defined(__scarlet__)\n'
  '#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__\n'
  '#define UTIL_ARCH_LITTLE_ENDIAN 1\n#define UTIL_ARCH_BIG_ENDIAN 0\n'
  '#else\n#define UTIL_ARCH_LITTLE_ENDIAN 0\n#define UTIL_ARCH_BIG_ENDIAN 1\n'
  '#endif\n\n#elif defined(__OPENCL_VERSION__)',1)
 write(overlay,path,text)

 # Disk cache is disabled in the compiler instance. Scarlet has no ELF build-id
 # introspection; leave the real serialization code, omit only cache setup.
 path='src/freedreno/ir3/ir3_disk_cache.c'
 text=(source/path).read_text()
 begin=text.index('   if (ir3_shader_debug & IR3_DBG_NOCACHE)')
 end=text.index('\n}\n',begin)
 text=text[:begin]+'#if HAVE_BUILD_ID\n'+text[begin:end]+'\n#else\n   assert(compiler->options.disable_cache);\n#endif'+text[end:]
 write(overlay,path,text)

 path='src/freedreno/ir3/ir3_shader.c'
 text=(source/path).read_text()
 text='#include "util/memstream.h"\n'+text
 text=text.replace('FILE *stream = open_memstream(&stream_data, &stream_size);',
  'struct u_memstream memory;\n   if (!u_memstream_open(&memory, &stream_data, &stream_size)) abort();\n   FILE *stream=u_memstream_get(&memory);')
 text=text.replace('fclose(stream);','u_memstream_close(&memory);')
 text=text.replace('FILE *disasm_stream = open_memstream(&disasm_data, &disasm_size);',
  'struct u_memstream memory;\n   if (!u_memstream_open(&memory, &disasm_data, &disasm_size)) { ralloc_free(rt_v); return NULL; }\n   FILE *disasm_stream=u_memstream_get(&memory);')
 text=text.replace('fflush(disasm_stream);','u_memstream_flush(&memory);').replace('fclose(disasm_stream);','u_memstream_close(&memory);')
 write(overlay,path,text)
