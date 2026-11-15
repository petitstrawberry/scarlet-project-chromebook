#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Source-built, self-contained pinned Mesa IR3 archive; no precompiled shaders.
Uses a private overlay: never changes the supplied Mesa source tree.
"""
import argparse, hashlib, json, os, pathlib, shutil, subprocess
HERE=pathlib.Path(__file__).resolve().parent
REV='3f1b217baffffa00cb8f53e158713a33e1bd4632'
TARGET='''
scarlet_ir3 = static_library('scarlet_ir3', files(@SOURCE@),
 include_directories: [inc_freedreno, inc_include, inc_src],
 dependencies: [idep_mesautil, idep_nir, idep_vtn, idep_libir3decode],
 link_whole: [libfreedreno_ir3,libfreedreno_common,libir3encode,_libir3decode,libisaspec,
              _libnir,libvtn,libcompiler,_libmesa_util,libmesa_util_links,
              _libmesa_util_c11,blake3],
 c_args: [no_override_init_args], build_by_default: false)
'''
def write(root,path,text):
 p=root/path
 if p.is_file() and not p.is_symlink() and p.read_text()==text:return
 if p.is_symlink():p.unlink()
 p.write_text(text)
def run(args,env):
 print('+',*map(str,args),flush=True);subprocess.run(list(map(str,args)),env=env,check=True)
def main():
 p=argparse.ArgumentParser(description=__doc__)
 p.add_argument('--source',required=True,type=pathlib.Path);p.add_argument('--work',required=True,type=pathlib.Path)
 p.add_argument('--cross',type=pathlib.Path);p.add_argument('--jobs',default='6')
 a=p.parse_args();source=a.source.resolve();work=a.work.resolve();overlay=work/'source';build=work/'build'
 manifest=json.loads((HERE/'mesa-inputs.json').read_text())
 source_hash=hashlib.sha256();count=0
 for path in sorted(source.rglob('*'),key=lambda p: str(p.relative_to(source))):
  name=path.relative_to(source)
  if '.git' in name.parts or not path.is_file():continue
  source_hash.update(str(name).encode()+b'\0'+hashlib.sha256(path.read_bytes()).digest());count+=1
 if count!=manifest['files'] or source_hash.hexdigest()!=manifest['source_sha256']:
  raise SystemExit('wrong Mesa source tree: require clean git archive of '+REV)
 if not overlay.exists():
  for directory,dirs,files in os.walk(source):
   dirs[:]=[d for d in dirs if d!='.git']
   dst=overlay/pathlib.Path(directory).relative_to(source);dst.mkdir(parents=True,exist_ok=True)
   for f in files:(dst/f).symlink_to(pathlib.Path(directory)/f)
 # RD trace output is a device tool, not a compiler dependency (would need zlib).
 common=(source/'src/freedreno/common/meson.build').read_text().replace("    'freedreno_rd_output.c',\n",'')
 write(overlay,'src/freedreno/common/meson.build',common)
 target=TARGET.replace('@SOURCE@',repr(str(HERE/'compiler.c')))
 write(overlay,'src/freedreno/ir3/meson.build',(source/'src/freedreno/ir3/meson.build').read_text()+target)
 if a.cross:
  from port import apply
  apply(source,overlay,write,HERE)
 env=os.environ.copy();env['MESA_GIT_SHA1_OVERRIDE']=REV;env['PYTHONDONTWRITEBYTECODE']='1'
 if not (build/'build.ninja').exists():
  args=['meson','setup',build,overlay,'--wrap-mode=nofallback','-Dgallium-drivers=softpipe','-Dvulkan-drivers=',
   '-Dplatforms=','-Dglx=disabled','-Degl=disabled','-Dgbm=disabled','-Dgles1=disabled','-Dgles2=disabled',
   '-Dopengl=true','-Dtools=freedreno','-Dllvm=disabled','-Dbuild-tests=false',
   '-Dbuildtype=release','-Db_ndebug=false','-Db_staticpic=true','-Dspirv-tools=disabled',
   '-Dzlib=disabled','-Dzstd=disabled','-Dxmlconfig=disabled','-Dshader-cache=disabled']
  if a.cross:args+=['--cross-file',a.cross.resolve()]
  run(args,env)
 run(['ninja','-C',build,'-j',a.jobs,'src/freedreno/ir3/libscarlet_ir3.a'],env)
 out=work/'lib';out.mkdir(exist_ok=True)
 archive=out/'libscarlet_ir3.a'
 temporary=out/'libscarlet_ir3.a.next'
 temporary.unlink(missing_ok=True)
 subprocess.run(['llvm-ar','-M'],input='CREATE '+str(temporary)+'\nADDLIB '+str(build/'src/freedreno/ir3/libscarlet_ir3.a')+'\nSAVE\nEND\n',text=True,env=env,check=True)
 temporary.replace(archive)
 record={'mesa_revision':REV,'target':'scarlet' if a.cross else 'host','archive_sha256':hashlib.sha256(archive.read_bytes()).hexdigest()}
 (out/'provenance.json').write_text(json.dumps(record,indent=2)+'\n');print(archive)
if __name__=='__main__':main()
