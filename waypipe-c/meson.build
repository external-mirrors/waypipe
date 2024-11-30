# DEFAULT_SOURCE implies POSIX_C_SOURCE 200809L + extras like CMSG_LEN
# requires glibc >= 4.19 (2014), freebsd libc (since 2016?), musl >= 1.15 (2014)
add_project_arguments('-D_DEFAULT_SOURCE', language: 'c')

# Sometimes ignoring the result of read()/write() is the right thing to do
add_project_arguments('-Wno-unused-result', language: 'c')

cc = meson.get_compiler('c')
config_data = configuration_data()
config_data.set('WAYPIPE_VERSION', version)

# Make build reproducible if possible
python3 = import('python').find_installation()
prefix_finder = 'import os.path; print(os.path.join(os.path.relpath(\'@0@\', \'@1@\'),\'\'))'
r = run_command(python3, '-c', prefix_finder.format(meson.source_root(), meson.build_root()))
relative_dir = r.stdout().strip()
if cc.has_argument('-fmacro-prefix-map=/prefix/to/hide=')
	add_project_arguments(
		'-fmacro-prefix-map=@0@='.format(relative_dir),
		language: 'c',
	)
else
	add_project_arguments(
		'-DWAYPIPE_REL_SRC_DIR="@0@"'.format(relative_dir),
		language: 'c',
	)
endif

libgbm = dependency('gbm', required: get_option('with_dmabuf'))
libdrm = dependency('libdrm', required: get_option('with_dmabuf'))
if libgbm.found() and libdrm.found()
	config_data.set('HAS_DMABUF', 1, description: 'Support DMABUF replication')
	has_dmabuf = true
else
	has_dmabuf = false
endif
pthreads = dependency('threads')
rt = cc.find_library('rt')
# XXX dtrace -G (Solaris, FreeBSD, NetBSD) isn't supported yet
is_linux = host_machine.system() == 'linux'
is_darwin = host_machine.system() == 'darwin'
if (is_linux or is_darwin) and get_option('with_systemtap') and cc.has_header('sys/sdt.h')
	config_data.set('HAS_USDT', 1, description: 'Enable static trace probes')
endif
has_flag_to_host = '''
// linux/vm_sockets.h doesn't compile on its own
// "invalid application of 'sizeof' to incomplete type 'struct sockaddr'"
#include <sys/socket.h>
#include <linux/vm_sockets.h>
#ifndef VMADDR_FLAG_TO_HOST
#error
#endif
int main(void) {
        return 0;
}
'''
if is_linux and cc.has_header('linux/vm_sockets.h') and cc.compiles(has_flag_to_host, name: 'has VMADDR_FLAG_TO_HOST')
	config_data.set('HAS_VSOCK', 1, description: 'Enable VM Sockets (VSOCK)')
endif
liblz4 = dependency('liblz4', version: '>=1.7.0', required: get_option('with_lz4'))
if liblz4.found()
	config_data.set('HAS_LZ4', 1, description: 'Enable LZ4 compression')
endif
libzstd = dependency('libzstd', version: '>=0.4.6', required: get_option('with_zstd'))
if libzstd.found()
	config_data.set('HAS_ZSTD', 1, description: 'Enable Zstd compression')
endif
libavcodec = dependency('libavcodec', required: get_option('with_video'))
libavutil = dependency('libavutil', required: get_option('with_video'))
libswscale = dependency('libswscale', required: get_option('with_video'))
libva = dependency('libva', required: get_option('with_vaapi'))
if libavcodec.found() and libavutil.found() and libswscale.found()
	config_data.set('HAS_VIDEO', 1, description: 'Enable video (de)compression')
	if libva.found()
		config_data.set('HAS_VAAPI', 1, description: 'Enable hardware video (de)compression with VAAPI')
	endif
endif
wayland_scanner = dependency('wayland-scanner', version: '>=1.15.0', required: get_option('with_secctx'), native: true)
wayland_client = dependency('wayland-client', required: get_option('with_secctx'))
if wayland_scanner.found() and wayland_client.found()
	config_data.set('HAS_SECURITY_CONTEXT', 1, description: 'Enable security-context-v1 support')
endif

waypipe_includes = [include_directories('protocols'), include_directories('.')]
if libdrm.found()
	waypipe_includes += include_directories(libdrm.get_pkgconfig_variable('includedir'))
endif

subdir('protocols')

waypipe_source_files = ['dmabuf.c', 'handlers.c', 'kernel.c', 'mainloop.c', 'parsing.c', 'platform.c', 'shadow.c', 'interval.c', 'util.c', 'video.c']
waypipe_deps = [
	pthreads,        # To run expensive computations in parallel
	rt,              # For shared memory
]
if config_data.has('HAS_DMABUF')
	# General GPU buffer creation, aligned with dmabuf proto
	waypipe_deps += [libgbm]
endif
if config_data.has('HAS_LZ4')
	waypipe_deps += [liblz4] # Fast compression option
endif
if config_data.has('HAS_ZSTD')
	waypipe_deps += [libzstd] # Slow compression option
endif
if config_data.has('HAS_VIDEO')
	waypipe_deps += [libavcodec,libavutil,libswscale]
endif
if config_data.has('HAS_VAAPI')
	waypipe_deps += [libva] # For NV12->RGB conversions
endif

# Conditionally compile SIMD-optimized code.
# (The meson simd module is a bit too limited for this)
kernel_libs = []
if cc.has_argument('-mavx512f') and cc.has_argument('-mlzcnt') and cc.has_argument('-mbmi') and get_option('with_avx512f')
	kernel_libs += static_library('kernel_avx512f', 'kernel_avx512f.c', c_args:['-mavx512f', '-mlzcnt', '-mbmi'])
	config_data.set('HAVE_AVX512F', 1, description: 'Compiler supports AVX-512F')
endif
if cc.has_argument('-mavx2') and cc.has_argument('-mlzcnt') and cc.has_argument('-mbmi') and get_option('with_avx2')
	kernel_libs += static_library('kernel_avx2', 'kernel_avx2.c', c_args:['-mavx2', '-mlzcnt', '-mbmi'])
	config_data.set('HAVE_AVX2', 1, description: 'Compiler supports AVX2')
endif
if cc.has_argument('-msse3') and get_option('with_sse3')
	kernel_libs += static_library('kernel_sse3', 'kernel_sse3.c', c_args:['-msse3'])
	config_data.set('HAVE_SSE3', 1, description: 'Compiler supports SSE 3')
endif
if ( host_machine.cpu_family() == 'aarch64' or cc.has_argument('-mfpu=neon') ) and get_option('with_neon_opts')
	neon_args = host_machine.cpu_family() == 'aarch64' ? [] : ['-mfpu=neon']

	# Clang additionally enforces that NEON code only be compiled
	# to target a CPU that actually supports NEON instructions,
	# so bump the host CPU version for the optionally executed code only.
	if host_machine.cpu_family() == 'arm' and cc.get_id() == 'clang'
		host_cpu = host_machine.cpu()
		if host_cpu.contains('4') or host_cpu.contains('5') or host_cpu.contains('6')
			neon_args += ['-march=armv7-a']
		endif
	endif

	kernel_libs += static_library('kernel_neon', 'kernel_neon.c', c_args:neon_args)
	config_data.set('HAVE_NEON', 1, description: 'Compiler supports NEON')
endif
if config_data.has('HAS_SECURITY_CONTEXT')
	wayland_scanner_prog = find_program(wayland_scanner.get_variable(pkgconfig: 'wayland_scanner'))
	wl_security_context_xml = '../protocols/security-context-v1.xml'
	protocols_src += custom_target(
		'security-context-v1-protocol.c',
		input: wl_security_context_xml,
		output: 'security-context-v1-protocol.c',
		command: [wayland_scanner_prog, 'private-code', '@INPUT@', '@OUTPUT@'],
	)
	protocols_src += custom_target(
		'security-context-v1-protocol.h',
		input: wl_security_context_xml,
		output: 'security-context-v1-protocol.h',
		command: [wayland_scanner_prog, 'client-header', '@INPUT@', '@OUTPUT@'],
	)

    waypipe_deps += [wayland_client]
    waypipe_source_files += ['secctx.c']
endif

configure_file(
	output: 'config-waypipe.h',
	configuration: config_data,
)

lib_waypipe_src = static_library(
	'waypipe_src',
	waypipe_source_files + protocols_src,
	include_directories: waypipe_includes,
	link_with: kernel_libs,
	dependencies: waypipe_deps,
)

waypipe_prog = executable(
	'waypipe-c',
	['waypipe.c', 'bench.c', 'client.c', 'server.c'],
	link_with: lib_waypipe_src,
	install: true
)

if get_option('tests')
	subdir('test')
endif
