project(
	'waypipe',
	'c',
	license: 'MIT/Expat',
	meson_version: '>=0.47.0',
	default_options: [
		'c_std=c11',
		'warning_level=3',
		'werror=true',
	],
	version: '0.9.2',
)

# mention version
version = '"@0@"'.format(meson.project_version())
git = find_program('git', native: true, required: false)
if git.found()
	dir_arg = '--git-dir=@0@/.git'.format(meson.source_root())
	commit = run_command([git, dir_arg, 'rev-parse', '--verify', '-q', 'HEAD'])
	if commit.returncode() == 0
		version = '"@0@ (commit @1@)"'.format(meson.project_version(), commit.stdout().strip())
	endif
endif

subdir('waypipe-c')

scdoc = dependency('scdoc', version: '>=1.9.4', native: true, required: get_option('man-pages'))
if scdoc.found()
	scdoc_prog = find_program(scdoc.get_pkgconfig_variable('scdoc'), native: true)
	sh = find_program('sh', native: true)
	mandir = get_option('mandir')
	custom_target(
		'waypipe.1',
		input: 'waypipe.scd',
		output: 'waypipe.1',
		command: [
			sh, '-c', '@0@ < @INPUT@ > @1@'.format(scdoc_prog.path(), 'waypipe.1')
		],
		install: true,
		install_dir: '@0@/man1'.format(mandir)
	)
endif
