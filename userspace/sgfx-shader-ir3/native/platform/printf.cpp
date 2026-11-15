// SPDX-License-Identifier: MIT
// Real formatting comes from the SDK's pinned LLVM libc printf_core (Apache-2.0
// WITH LLVM-exception). Only the buffer/FILE and allocation adapters live here.
// The prefixed ABI prevents interposing other DSOs' libc or Rust std functions.
#undef vsnprintf
#undef snprintf
#undef vsprintf
#undef sprintf
#undef vasprintf
#undef asprintf
#undef vfprintf
#undef fprintf
#undef vprintf
#undef printf
#define LIBC_NAMESPACE scarlet_ir3_libc
#define LIBC_ERRNO_MODE LIBC_ERRNO_MODE_SYSTEM_INLINE
#define SCARLET_IR3_PRINTF_IMPLEMENTATION
#include "printf.h"
#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#ifdef __scarlet__
// SDK fenv.h exposes only fegetround/fesetround. LLVM's private FP helpers
// access FPCR/FPSR directly and need their own complete environment type and
// exception constants. These declarations stay inside this translation unit.
#include <fenv.h>
#undef FE_DOWNWARD
#undef FE_TONEAREST
#undef FE_TOWARDZERO
#undef FE_UPWARD
#include "include/llvm-libc-types/fenv_t.h"
#include "include/llvm-libc-macros/fenv-macros.h"
#define MATH_ERRNO 1
#define MATH_ERREXCEPT 2
#define math_errhandling (MATH_ERRNO | MATH_ERREXCEPT)
#endif
#include "src/__support/arg_list.h"
#include "src/stdio/printf_core/printf_main.h"
#include "src/stdio/printf_core/writer.h"
#include "src/__support/StringUtil/error_to_string.cpp"
namespace ll = LIBC_NAMESPACE;
namespace pc = ll::printf_core;
#define EXPORT extern "C" __attribute__((visibility("hidden")))

EXPORT int scarlet_ir3_vsnprintf(char *buffer,size_t size,const char *format,va_list list) {
    if(!format || (size && !buffer)) { errno=EINVAL; return -1; }
    ll::internal::ArgList args(list);
    pc::WriteBuffer<pc::WriteMode::FILL_BUFF_AND_DROP_OVERFLOW> wb(buffer,size ? size-1:0);
    pc::Writer writer(wb);
    int result=pc::printf_main(&writer,format,args);
    if(size) wb.buff[wb.buff_cur]=0;
    return result;
}
EXPORT int scarlet_ir3_snprintf(char *buffer,size_t size,const char *format,...) {
    va_list args;va_start(args,format);int n=scarlet_ir3_vsnprintf(buffer,size,format,args);va_end(args);return n;
}
EXPORT int scarlet_ir3_vsprintf(char *buffer,const char *format,va_list args) {
    return scarlet_ir3_vsnprintf(buffer,SIZE_MAX,format,args);
}
EXPORT int scarlet_ir3_sprintf(char *buffer,const char *format,...) {
    va_list args;va_start(args,format);int n=scarlet_ir3_vsprintf(buffer,format,args);va_end(args);return n;
}
// Single-pass growing sink. %n and repeated positional arguments retain their
// printf semantics; no sizing pass can consume or mutate the user's arguments.
struct Grow { char *data=nullptr; size_t size=0,capacity=0; };
static int append(ll::cpp::string_view bytes,void *opaque) {
    Grow *out=static_cast<Grow *>(opaque);
    if(bytes.size()>INT_MAX-out->size) { errno=EOVERFLOW;return -1; }
    size_t required=out->size+bytes.size()+1;
    if(required>out->capacity) {
        size_t capacity=required>out->capacity*2?required:out->capacity*2;
        char *data=static_cast<char *>(realloc(out->data,capacity));
        if(!data) {errno=ENOMEM;return -1;}
        out->data=data;out->capacity=capacity;
    }
    if(bytes.size()) memcpy(out->data+out->size,bytes.data(),bytes.size());
    out->size+=bytes.size();return 0;
}
EXPORT int scarlet_ir3_vasprintf(char **out,const char *format,va_list list) {
    if(!out || !format) {errno=EINVAL;return -1;}
    *out=nullptr;Grow grow;char stack[256];
    ll::internal::ArgList args(list);
    pc::WriteBuffer<pc::WriteMode::FLUSH_TO_STREAM> wb(stack,sizeof(stack),append,&grow);
    pc::Writer writer(wb);
    int result=pc::printf_main(&writer,format,args);
    if(result<0 || wb.overflow_write({})<0 || append({},&grow)<0) {free(grow.data);return -1;}
    grow.data[grow.size]=0;*out=grow.data;return result;
}
EXPORT int scarlet_ir3_asprintf(char **out,const char *format,...) {
    va_list args;va_start(args,format);int n=scarlet_ir3_vasprintf(out,format,args);va_end(args);return n;
}
static int file_write(ll::cpp::string_view bytes,void *opaque) {
    return fwrite(bytes.data(),1,bytes.size(),static_cast<FILE *>(opaque))==bytes.size()?0:-1;
}
EXPORT int scarlet_ir3_vfprintf(FILE *file,const char *format,va_list list) {
    if(!file || !format) {errno=EINVAL;return -1;}
    char stack[512];ll::internal::ArgList args(list);
    pc::WriteBuffer<pc::WriteMode::FLUSH_TO_STREAM> wb(stack,sizeof(stack),file_write,file);
    pc::Writer writer(wb);
    int result=pc::printf_main(&writer,format,args);
    return result<0 || wb.overflow_write({})<0 ? -1:result;
}
EXPORT int scarlet_ir3_fprintf(FILE *file,const char *format,...) {
    va_list args;va_start(args,format);int n=scarlet_ir3_vfprintf(file,format,args);va_end(args);return n;
}
EXPORT int scarlet_ir3_vprintf(const char *format,va_list args) {return scarlet_ir3_vfprintf(stdout,format,args);}
EXPORT int scarlet_ir3_printf(const char *format,...) {
    va_list args;va_start(args,format);int n=scarlet_ir3_vprintf(format,args);va_end(args);return n;
}
EXPORT int scarlet_ir3_format_check(void) {
    char buffer[128],junk;
    if(scarlet_ir3_snprintf(&junk,1,"%s:%u:%.2f","ir3",7u,1.25)!=10 || junk) return 1;
    if(scarlet_ir3_snprintf(nullptr,0,"%2$.*1$f",2,1.25)!=4) return 2;
    if(scarlet_ir3_snprintf(buffer,sizeof(buffer),"%+08.2f/%zu/%lld",1.25,size_t(17),-42LL)!=15 || strcmp(buffer,"+0001.25/17/-42")) return 3;
    char *allocated=nullptr;int count=-1;
    int n=scarlet_ir3_asprintf(&allocated,"%.3g/%n%a",1.25,&count,1.5);
    bool bad=n<0 || count!=5 || !allocated || strncmp(allocated,"1.25/0x1.8p+0",13);
    free(allocated);return bad?4:0;
}
