# AppImage runtime notices and source

These notices accompany the pinned type-2 runtime, version `20251108`, source
commit `dd6cebedcbddde9c82f89b011e8e1d40b6e43868`.

The runtime's own MIT notice lists musl, libfuse, squashfuse, zstd and zlib as
statically linked components. Full component notices are retained here. Notice
files were read from these primary sources on 2026-10-14:

- [Runtime LICENSE](https://github.com/AppImage/type2-runtime/blob/20251108/LICENSE)
- [libfuse 3.15.0 LGPL 2.1](https://github.com/libfuse/libfuse/blob/fuse-3.15.0/LGPL2.txt)
- [squashfuse 0.5.2](https://github.com/vasi/squashfuse/blob/0.5.2/LICENSE)
- [musl COPYRIGHT](https://git.musl-libc.org/cgit/musl/tree/COPYRIGHT?h=v1.2.5)
- [zstd BSD notice](https://github.com/facebook/zstd/blob/v1.5.7/LICENSE)
- [zlib notice](https://github.com/madler/zlib/blob/v1.3.1/LICENSE)

The release builder adds `source/type2-runtime.tar`, containing the exact
runtime source tree, including its libfuse patch and build instructions, and
`source/fuse-3.15.0.tar.xz`, whose checksum is pinned in the runtime's dependency
build recipe. These allow rebuilding/replacing the separate runtime prefix;
the upstream runtime source documents the required build environment.
They are included in the AppImage's `usr/share/doc/gp200-studio/licenses/`
directory, available by extracting the AppImage without FUSE.
