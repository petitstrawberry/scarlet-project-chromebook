// SPDX-License-Identifier: MIT
// Host differential coverage for the same LLVM formatter used on Scarlet.
#define SCARLET_IR3_PRINTF_IMPLEMENTATION
#include "printf.h"
#include <cassert>
#include <cmath>
#include <cstring>
#include <cstdlib>
#include <string>
#include <vector>
#include <cstdint>
static unsigned checks;
static void compare(const char *fmt,...) {
    for (size_t size: {size_t(0),size_t(1),size_t(2),size_t(9),size_t(64),size_t(2048)}) {
        char expected[2048],actual[2048];
        memset(expected,0x5a,sizeof(expected));memset(actual,0x5a,sizeof(actual));
        va_list args;va_start(args,fmt);int n=vsnprintf(expected,size,fmt,args);va_end(args);
        va_start(args,fmt);int m=scarlet_ir3_vsnprintf(actual,size,fmt,args);va_end(args);
        if(n!=m || memcmp(expected,actual,sizeof(actual))) {fprintf(stderr,"format %s size=%zu n=%d m=%d expected=%s actual=%s\n",fmt,size,n,m,size?expected:"<empty>",size?actual:"<empty>");abort();}
        ++checks;
    }
}
int main() {
    assert(scarlet_ir3_format_check()==0);
    compare("hello:%s/%.*s/%c/%%","mesa",3,"abcdef",'q');
    compare("%s","");compare("%.0d",0);
    compare("%+08d/%#08x/%#.4o/%-9u",-37,0xcafe,7,123u);
    compare("%hhd/%hd/%ld/%lld/%zu/%td/%ju",-128,-32768,-1234567890L,INT64_MIN,SIZE_MAX,ptrdiff_t(-7),uintmax_t(17));
    for(double value: {0.0,-0.0,1.25,-9.625,1e-250,1e250}) {
        compare("%f/%+.3e/%#.9g/%012.2f",value,value,value,value);
    }
    // C permits implementation-dependent spelling/signs of infinity/NaN.
    char special[80];
    assert(scarlet_ir3_snprintf(special,sizeof(special),"%f/%f",double(INFINITY),double(NAN))>0);
    assert(strstr(special,"inf") && strstr(special,"nan"));
    compare("%2$.*1$f/%3$d",2,1.25,37);
    compare("%*.*s",-30,3,"abcdef");
    std::string large(1500,'r');char *text=nullptr;
    int n=scarlet_ir3_asprintf(&text,"[%s] %.3f",large.c_str(),1.25);
    assert(n==1508 && std::string(text)=="["+large+"] 1.250");free(text);
    FILE *f=tmpfile();assert(f);
    assert(scarlet_ir3_fprintf(f,"%s=%.2f",large.c_str(),2.5)==1505);
    rewind(f);char bytes[1600]={};assert(fread(bytes,1,sizeof(bytes),f)==1505);
    assert(std::string(bytes)==large+"=2.50");fclose(f);
    printf("PASS %u printf comparison cases; size-only/truncation/positional/float/integer/asprintf/FILE sinks\n",checks);
}
