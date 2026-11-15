#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Generate a PIC AArch64 Scarlet Meson crossfile from the actual installed SDK.

Only link probes use the SDK crt0/static runtime closure. Mesa's static compiler
archive gets no startup object; final native Rust links supply Scarlet's OSABI.
Matches scarlet-sdk a071222 native/default.nix's native-runtime install check.
No Linux identity, host headers, libc stubs or synthetic probe answers are used.
"""
import argparse
import json
import os
import pathlib
import re
import shutil
import struct
import subprocess
import tempfile


def executable(value):
    result = shutil.which(value or "")
    if not result:
        raise ValueError(f"required executable unavailable: {value}")
    # Preserve argv[0]: Nix coreutils/LLVM may use multicall symlinks.
    return str(pathlib.Path(result).absolute())


def required(path):
    path = pathlib.Path(path).resolve()
    if not path.is_file():
        raise ValueError(f"required SDK input missing: {path}")
    return str(path)


def meson_list(values):
    # Meson accepts single-quoted strings with escaped backslash/apostrophe.
    return "[" + ", ".join("'" + str(v).replace("\\", "\\\\").replace("'", "\\'")
                           + "'" for v in values) + "]"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--runtime", default=os.environ.get("SCARLET_NATIVE_RUNTIME"))
    parser.add_argument("--rust-toolchain", default=os.environ.get("SCARLET_RUST_TOOLCHAIN"))
    parser.add_argument("--cc", default=os.environ.get("SCARLET_NATIVE_CC"))
    parser.add_argument("--cxx", default=os.environ.get("SCARLET_NATIVE_CXX"))
    parser.add_argument("--ar", default=os.environ.get("SCARLET_NATIVE_AR", "llvm-ar"))
    parser.add_argument("--strip", default="llvm-strip")
    parser.add_argument("--lld", default="ld.lld")
    parser.add_argument("--verify", action="store_true", help="compile/link small C/C++ probes, never execute them")
    args = parser.parse_args()
    if not args.runtime or not args.rust_toolchain:
        parser.error("SCARLET_NATIVE_RUNTIME and SCARLET_RUST_TOOLCHAIN (or explicit paths) required")
    runtime = pathlib.Path(args.runtime).resolve()
    cc, cxx = executable(args.cc), executable(args.cxx)
    ar, strip, lld = executable(args.ar), executable(args.strip), executable(args.lld)
    resource = pathlib.Path(subprocess.check_output([cc, "-print-resource-dir"], text=True).strip())
    required(resource / "include/stddef.h")
    for header in ["stdlib.h", "math.h", "pthread.h", "c++/v1/__config_site", "c++/v1/vector"]:
        required(runtime / "include" / header)
    crt = required(pathlib.Path(args.rust_toolchain) /
                   "lib/rustlib/aarch64-unknown-scarlet/lib/scarlet-crt0.o")
    archives = [required(runtime / "lib" / name) for name in [
        "libc++.a", "libc++abi.a", "libscarlet_c_startup.a",
        "libscarlet_cxx_support.a", "libscarlet_c.a"]]
    # Raw Clang avoids the Nix wrapper. -nostdinc alone does not suppress CPATH.
    clear = ["CPATH", "C_INCLUDE_PATH", "CPLUS_INCLUDE_PATH", "OBJC_INCLUDE_PATH",
             "LIBRARY_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "SDKROOT",
             "MACOSX_DEPLOYMENT_TARGET", "NIX_CFLAGS_COMPILE", "NIX_LDFLAGS"]
    env_prefix = [executable("env")]
    for variable in clear:
        env_prefix += ["-u", variable]
    common = ["--target=aarch64-unknown-none-elf", f"--sysroot={runtime}",
              "-nostdinc", "-D__scarlet__=1", "-fPIC", "-fno-stack-protector",
              "-mno-outline-atomics"]
    c_args = common + ["-isystem", str(runtime / "include"),
                       "-isystem", str(resource / "include")]
    cpp_args = common + ["-nostdinc++", "-isystem", str(runtime / "include/c++/v1"),
                         "-isystem", str(runtime / "include"), "-isystem", str(resource / "include"),
                         "-fno-exceptions", "-fno-rtti", "-D_LIBCPP_HAS_CLOCK_GETTIME=1"]
    link = [f"--target=aarch64-unknown-none-elf", f"--sysroot={runtime}",
            "-nostdlib", "-static", f"-fuse-ld={lld}",
            "-Wl,--gc-sections,--no-undefined,-e,_start",
            "-Wl,--wrap=main,--undefined=__wrap_main", "-Wl,-z,max-page-size=4096",
            crt, "-Wl,--start-group", *archives, "-Wl,--end-group"]
    # Target pkg-config searches only target SDK paths, even under Nix's wrapper.
    pkg_dirs = f"{runtime}/lib/pkgconfig:{runtime}/share/pkgconfig"
    pkg = env_prefix + ["-u", "PKG_CONFIG_PATH", "PKG_CONFIG_PATH=",
        f"PKG_CONFIG_LIBDIR={pkg_dirs}", f"PKG_CONFIG_SYSROOT_DIR={runtime}",
        "PKG_CONFIG_PATH_arm64_apple_darwin=", "PKG_CONFIG_PATH_aarch64_unknown_linux_gnu=",
        executable("pkg-config")]
    text = "# Generated from installed Scarlet SDK; no target programs are executed.\n[binaries]\n"
    # Target/nostdinc must also apply during compiler default-include discovery,
    # or Meson can strip the explicit Clang builtin include as a host default.
    for name, command in [("c", env_prefix + [cc, *common]), ("cpp", env_prefix + [cxx, *common]),
                          ("ar", [ar]), ("strip", [strip]), ("pkg-config", pkg)]:
        text += name + " = " + meson_list(command) + "\n"
    text += ("\n[host_machine]\nsystem = 'scarlet'\ncpu_family = 'aarch64'\n"
             "cpu = 'armv8-a'\nendian = 'little'\n\n[properties]\nneeds_exe_wrapper = true\n"
             f"sys_root = {meson_list([runtime])[1:-1]}\npkg_config_libdir = {meson_list(pkg_dirs.split(':'))}\n"
             "\n[built-in options]\nb_staticpic = true\nb_pie = false\n")
    # Meson groups archive operands itself; explicit groups would nest and LLD
    # correctly rejects them. Standalone verification uses the SDK's group.
    meson_link = [flag for flag in link if flag not in ("-Wl,--start-group", "-Wl,--end-group")]
    for name, values in [("c_args", c_args[len(common):]), ("cpp_args", cpp_args[len(common):]),
                         ("c_link_args", meson_link), ("cpp_link_args", meson_link)]:
        text += name + " = " + meson_list(values) + "\n"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text)
    print(f"Crossfile: {args.output.resolve()}")
    print(f"Runtime: {runtime}\nC: {cc}\nC++: {cxx}\nClang headers: {resource / 'include'}")
    print("Profile: PIC AArch64 ELF, libc++ without exceptions/RTTI; target probes compile/link only")
    if args.verify:
        probes = [
            ("c", cc, c_args, """#include <stdint.h>
#include <stdlib.h>
#include <math.h>
#include <pthread.h>
static pthread_once_t once = PTHREAD_ONCE_INIT;
static void init(void) {}
int main(void) { double (*volatile root)(double)=sqrt;
  return pthread_once(&once,init) || root(9.0)!=3.0; }
"""),
            ("cpp", cxx, cpp_args, """#include <vector>
#include <string>
#include <mutex>
#include <cmath>
int main() {std::mutex m; std::lock_guard<std::mutex> lock(m);
  std::vector<std::string> v; v.emplace_back("Scarlet");
  return v[0].size()!=7 || std::sqrt(9.0)!=3.0;}
""")]
        with tempfile.TemporaryDirectory(prefix="scarlet-ir3-cross-probe-") as directory:
            work = pathlib.Path(directory)
            for language, compiler, flags, code in probes:
                source = work / ("probe.c" if language == "c" else "probe.cpp")
                binary = work / language
                source.write_text(code)
                command = env_prefix + [compiler, *flags, "-O2", "-std=c11" if language == "c" else "-std=c++17",
                                        str(source), *link, "-o", str(binary)]
                subprocess.run(command, check=True)
                headers = subprocess.run(env_prefix + [compiler, *flags, "-H", "-fsyntax-only",
                    "-std=c11" if language == "c" else "-std=c++17", str(source)],
                    check=True, capture_output=True, text=True)
                includes = [pathlib.Path(match.group(1)).resolve()
                    for line in headers.stderr.splitlines()
                    if (match := re.match(r"^\.+ (/.*)$", line))]
                if not includes or any(not (p.is_relative_to(runtime) or
                    p.is_relative_to(resource.resolve())) for p in includes):
                    raise ValueError(f"{language}: unexpected host include path in header trace")
                elf = binary.read_bytes()
                if elf[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<HH", elf, 16) != (2, 183):
                    raise ValueError(f"{language}: expected AArch64 ELF executable from real static link")
                phoff = struct.unpack_from("<Q", elf, 32)[0]
                phsize, phcount = struct.unpack_from("<HH", elf, 54)
                if any(struct.unpack_from("<I", elf, phoff + i * phsize)[0] in (2, 3, 7)
                       for i in range(phcount)):
                    raise ValueError(f"{language}: unexpected dynamic loader/dependency/TLS in static probe")
                # Deliberately don't execute a target binary on the host.
                print(json.dumps({"probe": language, "elf": "AArch64 EXEC", "bytes": len(elf),
                                  "sdk_headers": len(includes), "executed": False}))


if __name__ == "__main__":
    main()
