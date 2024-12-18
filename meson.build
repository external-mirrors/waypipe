project(
    'waypipe',
    'c',
    license: ['GPL3+', 'MIT/Expat'],
    meson_version: '>=0.57.0',
    default_options: ['c_std=c11', 'warning_level=3', 'werror=true'],
    version: '0.9.2',
)

# mention version
raw_version = '@0@'.format(meson.project_version())
git = find_program('git', native: true, required: false)
if git.found()
    dir_arg = '--git-dir=@0@/.git'.format(meson.project_source_root())
    commit = run_command([git, dir_arg, 'rev-parse', '--verify', '-q', 'HEAD'])
    if commit.returncode() == 0
        raw_version = '@0@ (commit @1@)'.format(
            meson.project_version(),
            commit.stdout().strip(),
        )
    endif
endif
version = '"@0@"'.format(raw_version)

if get_option('build_rs')
    cargo = find_program('cargo', native: true)
    sh = find_program('sh', native: true)
    objcopy = find_program('objcopy', native: true)

    # Because meson and cargo interact badly, option implementation and dependency resolution are deferred to cargo
    # However, to 'autodetect' what features are available, we duplicate some of the build logic
    features = []
    has_lz4 = false
    has_zstd = false
    has_dmabuf = false
    has_video = false

    # compute maximum of with_lz4/with_zstd/with_video features
    max_feature = get_option('with_lz4')
    if (max_feature.auto() and get_option('with_zstd').enabled()) or (not max_feature.enabled() and get_option(
        'with_zstd',
    ).auto())
        max_feature = get_option('with_zstd')
    endif
    if (max_feature.auto() and get_option('with_video').enabled()) or (not max_feature.enabled() and get_option(
        'with_video',
    ).auto())
        max_feature = get_option('with_video')
    endif

    bindgen = find_program(
        'bindgen',
        version: '>= 0.66.0',
        native: true,
        required: max_feature,
    )
    liblz4 = dependency(
        'liblz4',
        version: '>=1.7.0',
        required: get_option('with_lz4'),
    )
    if bindgen.found() and liblz4.found()
        has_lz4 = true
        features += ['lz4']
    endif
    libzstd = dependency(
        'libzstd',
        version: '>=0.4.6',
        required: get_option('with_zstd'),
    )
    if bindgen.found() and libzstd.found()
        has_zstd = true
        features += ['zstd']
    endif
    libvulkan = dependency('vulkan', required: get_option('with_dmabuf'))
    if libvulkan.found()
        # Note: 'ash' is the vulkan wrapper used, and may require other libraries/programs to be present
        has_dmabuf = true
        features += ['dmabuf']
    endif
    glslc = find_program(
        'glslc',
        native: true,
        required: get_option('with_video'),
    )
    libavutil = dependency('libavutil', required: get_option('with_video'))
    libavcodec = dependency('libavcodec', required: get_option('with_video'))
    if bindgen.found() and libavcodec.found() and libavutil.found() and glslc.found()
        has_video = true
        features += ['video']
    endif

    target_dir = meson.project_build_root() / 'target'
    output = target_dir / 'release/waypipe'
    manifest = meson.project_source_root() / 'Cargo.toml'
    profile = 'release'
    unstripped = custom_target(
        'waypipe-unstripped',
        build_always_stale: true,
        output: 'waypipe-unstripped',
        env: {
            'WAYPIPE_VERSION': raw_version,
            'WAYPIPE_FEATURE_LZ4': has_lz4.to_string('true', 'false'),
            'WAYPIPE_FEATURE_ZSTD': has_zstd.to_string('true', 'false'),
            'WAYPIPE_FEATURE_DMABUF': has_dmabuf.to_string('true', 'false'),
            'WAYPIPE_FEATURE_VIDEO': has_video.to_string('true', 'false'),
        },
        command: [
            sh,
            '@SOURCE_ROOT@' / 'compile_wrapper.sh',
            profile,
            manifest,
            target_dir,
            ','.join(features),
            '@OUTPUT@',
        ],
    )


    debug_destination = '/usr/debug/waypipe'
    debug_name = 'waypipe.debug'
    # TODO: where to install debug info. The current directory works when testing with gdb.
    debuginfo = custom_target(
        'waypipe-debuginfo',
        input: unstripped,
        output: debug_name,
        command: [
            objcopy.full_path(),
            '--only-keep-debug',
            unstripped,
            '@OUTPUT@',
        ],
    )

    debuglink = '--add-gnu-debuglink=@0@'.format(debug_name)
    final_binary = custom_target(
        'waypipe',
        input: [unstripped, debuginfo],
        output: 'waypipe',
        command: [
            # Strip (but link) the debuginfo
            objcopy.full_path(),
            '--strip-debug',
            '--strip-unneeded',
            '--remove-section=".gnu_debuglink"',
            debuglink,
            unstripped,
            '@OUTPUT@',
        ],
        install: true,
        install_dir: 'bin',
    )

    test(
        'running cargo test',
        cargo,
        args: [
            'test',
            '--frozen',
            '-v',
            '--manifest-path',
            manifest,
            '--no-default-features',
            '--target-dir',
            target_dir,
            '--features',
            ','.join(features),
        ],
        timeout: 200,
    )

    test(
        'running (shortened) waypipe bench',
        sh,
        args: [
            '-c',
            ' '.join([final_binary.full_path(), '--test-fast-bench', 'bench']),
        ],
        depends: final_binary,
    )
endif

if get_option('build_c')
    subdir('waypipe-c')
endif

scdoc = dependency(
    'scdoc',
    version: '>=1.9.4',
    native: true,
    required: get_option('man-pages'),
)
if scdoc.found()
    scdoc_prog = find_program(
        scdoc.get_variable(pkgconfig: 'scdoc'),
        native: true,
    )
    sh = find_program('sh', native: true)
    mandir = get_option('mandir')
    custom_target(
        'waypipe.1',
        input: 'waypipe.scd',
        output: 'waypipe.1',
        command: [
            sh,
            '-c',
            '@0@ < @INPUT@ > @1@'.format(scdoc_prog.full_path(), 'waypipe.1'),
        ],
        install: true,
        install_dir: '@0@/man1'.format(mandir),
    )
endif
