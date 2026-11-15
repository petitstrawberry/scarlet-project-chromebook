#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Strict native compiler archive + real SDK root-link audit; never executes target code."""
import argparse,ast,configparser,hashlib,pathlib,re,subprocess,tempfile
ROOT=r'''
#include <stdint.h>
#include <stddef.h>
struct compiler_result;
extern int scarlet_ir3_compile(const uint32_t*,size_t,uint32_t,const char*,uint32_t,struct compiler_result*);
extern void scarlet_ir3_result_free(struct compiler_result*);

static __typeof__(&scarlet_ir3_compile) volatile compile_root=scarlet_ir3_compile;
static __typeof__(&scarlet_ir3_result_free) volatile free_root=scarlet_ir3_result_free;

int main(void){return ((uintptr_t)compile_root ^ (uintptr_t)free_root)==0;}
'''
def output(args):return subprocess.check_output(list(map(str,args)),text=True)
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--archive',type=pathlib.Path,required=True)
 p.add_argument('--cross',type=pathlib.Path,required=True);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
 archive=a.archive.resolve()
 if archive.read_bytes()[:8]!=b'!<arch>\n':raise SystemExit('require self-contained, non-thin archive')
 headers=output(['llvm-readelf','-h','-S','-r',archive])
 machines=re.findall(r'Machine:\s*(.*)',headers)
 if not machines or any(m.strip()!='AArch64' for m in machines):raise SystemExit('non-AArch64 compiler object')
 if re.search(r'\.tdata\b|\.tbss\b|R_AARCH64_TLS|R_AARCH64_TLSDESC',headers):raise SystemExit('ELF TLS in compiler archive')
 unresolved=output(['llvm-nm','-u',archive])
 if re.search(r'\bU (?:v?snprintf|v?sprintf|v?asprintf|v?fprintf|v?printf)$',unresolved,re.M):raise SystemExit('Mesa escaped private LLVM printf adapter into SDK integer-only formatting')
 if re.search(r'\b(?:thrd_exit|pthread_exit|mtx_timedlock|pthread_mutex_timedlock|__tls_get_addr)\b',unresolved):raise SystemExit('unsupported C11/ELF TLS reference')
 config=configparser.ConfigParser(interpolation=None);config.read(a.cross)
 values=lambda section,key:ast.literal_eval(config[section][key])
 a.output.parent.mkdir(parents=True,exist_ok=True)
 with tempfile.TemporaryDirectory(prefix='ir3-compiler-root-') as directory:
  source=pathlib.Path(directory)/'root.c';source.write_text(ROOT)
  command=values('binaries','c')+values('built-in options','c_args')+[str(source),'-Wl,--start-group',str(archive)]+values('built-in options','c_link_args')+['-Wl,--end-group','-o',str(a.output.resolve())]
  subprocess.run(command,check=True)
 elf=output(['llvm-readelf','-h','-l',a.output])
 if 'AArch64' not in elf or re.search(r'^\s*(TLS|INTERP|DYNAMIC)\s',elf,re.M):raise SystemExit('unexpected linked ELF architecture/program header')
 for line in output(['llvm-nm','-u',a.output]).splitlines():
  if line.split()!=['w','environ']:raise SystemExit('unexpected unresolved symbol: '+line)
 print(f'PASS: {len(machines)} AArch64 objects, no ELF TLS, strict SDK compiler/diagnostic root link; target not executed')
 print('archive SHA256:',hashlib.sha256(archive.read_bytes()).hexdigest())
if __name__=='__main__':main()
